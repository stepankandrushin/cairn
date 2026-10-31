//! Reading a sync root. Whatever cannot be read becomes a skip with a reason, and a path this scan
//! could not observe is recorded as such, so it is never mistaken for a delete.

use crate::chunker::manifest_of;
use crate::fsops::{is_not_found, is_permission, Dir, Stat};
use crate::metadata::{capture, Accounts};
use crate::state::{Fingerprint, Row};
use cairn_failpoint::hit;
use cairn_protocol::name::display_bytes;
use cairn_protocol::{Bytes, Content, Hash, Metadata, Name, Skip, SkipKind, Timestamp, STATE_DIR};
use rustix::fs::FileType;
use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

/// A write within this window of a read may leave the timestamps it read unchanged.
const RACY_WINDOW_NS: i128 = 100_000_000;

/// A file changed more recently than this may be in the middle of a save: a program that truncates
/// and then writes leaves an empty file that reads as perfectly steady. It waits this long first.
const STABLE_WINDOW_NS: i128 = 250_000_000;

/// How often a file that changed under a read is tried again within one scan.
const READ_ATTEMPTS: u32 = 3;

/// One inode: several paths when hard-linked.
#[derive(Clone, Debug)]
pub struct Scanned {
    pub paths: Vec<Vec<Name>>,
    pub fp: Fingerprint,
    pub racy: bool,
    pub content: Content,
    pub metadata: Metadata,
    /// Some metadata could not be read, so this scan says nothing about what changed.
    pub metadata_incomplete: bool,
}

#[derive(Debug)]
pub struct ScanResult {
    pub root: Scanned,
    pub nodes: Vec<Scanned>,
    pub skips: Vec<Skip>,
    /// Entries this scan could not observe, each with everything under it.
    pub unobserved: Vec<Vec<Name>>,
    /// The inodes of those, which may be known objects under a new name.
    pub unobserved_inodes: Vec<(u64, u64)>,
    /// Something could not even be stat'ed, so it may be anything: no absence proves a delete.
    pub unidentified: bool,
    /// Directories that were seen but whose contents could not be listed.
    pub unlisted: Vec<Vec<Name>>,
    /// The tree changed while it was walked, so a missing entry proves nothing.
    pub unstable: bool,
    pub new_manifests: Vec<(Hash, Vec<u8>)>,
    /// This device's leftover temporaries that a read-only directory kept from being removed.
    pub stale_temps: Vec<(Vec<Name>, Vec<u8>)>,
}

pub struct ScanOptions<'a> {
    pub temp_prefix: &'a [u8],
    pub accounts: &'a Accounts,
    /// What is known by inode, to avoid reading unchanged files.
    pub rows: &'a HashMap<(u64, u64), &'a Row>,
    pub remove_own_temps: bool,
}

pub fn fingerprint(st: &Stat) -> Fingerprint {
    Fingerprint {
        dev: st.dev,
        ino: st.ino,
        size: st.size,
        mtime: st.mtime,
        ctime: st.ctime,
        btime: st.btime,
    }
}

fn now_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i128)
}

fn ns(t: Timestamp) -> i128 {
    i128::from(t.sec) * 1_000_000_000 + i128::from(t.nsec)
}

/// Whether a later write could leave these timestamps as they are. ctime alone decides: every write
/// moves it, and unlike mtime it cannot be set by hand.
pub fn is_racy(st: &Stat, precision_ns: u32) -> bool {
    now_ns() - ns(st.ctime) < RACY_WINDOW_NS + i128::from(precision_ns)
}

/// Wait out what is left of the stable window since this file last changed.
fn wait_until_stable(st: &Stat) {
    let age = now_ns() - ns(st.ctime);
    if age < STABLE_WINDOW_NS {
        // A clock stepped backwards makes the age negative; the wait is still bounded.
        let wait = (STABLE_WINDOW_NS - age).min(STABLE_WINDOW_NS);
        std::thread::sleep(std::time::Duration::from_nanos(wait as u64));
    }
}

pub fn display_path(path: &[Name]) -> String {
    let parts: Vec<String> = path.iter().map(|n| display_bytes(n.as_bytes())).collect();
    parts.join("/")
}

struct Walk<'a> {
    opts: &'a ScanOptions<'a>,
    precision_ns: u32,
    nodes: Vec<Scanned>,
    linked: HashMap<(u64, u64), usize>,
    skips: Vec<Skip>,
    unobserved: Vec<Vec<Name>>,
    unobserved_inodes: Vec<(u64, u64)>,
    unidentified: bool,
    current_inode: Option<(u64, u64)>,
    unlisted: Vec<Vec<Name>>,
    new_manifests: BTreeMap<Hash, Vec<u8>>,
    stale_temps: Vec<(Vec<Name>, Vec<u8>)>,
    /// Each directory with what it looked like when listed.
    dirs: Vec<(Vec<Name>, Timestamp, Timestamp)>,
}

impl Walk<'_> {
    fn skip(&mut self, path: &[Name], kind: SkipKind, detail: Option<String>) {
        self.skips.push(Skip {
            path: display_path(path),
            kind,
            detail,
        });
    }

    fn unobservable(&mut self, path: &[Name], kind: SkipKind, detail: Option<String>) {
        self.skip(path, kind, detail);
        self.unobserved.push(path.to_vec());
        match self.current_inode {
            Some(inode) => self.unobserved_inodes.push(inode),
            None => self.unidentified = true,
        }
    }

    /// Returns whether the directory could be listed.
    fn walk_dir(&mut self, dir: &Dir, path: &mut Vec<Name>, st_mnt: u64) -> bool {
        hit("scan.dir");
        let is_root = path.is_empty();
        let listed = dir.stat_self().and_then(|st| Ok((st, dir.list()?)));
        let (st, names) = match listed {
            Ok(v) => v,
            Err(e) if is_permission(&e) => {
                self.skip(path, SkipKind::PermissionDenied, Some(e.to_string()));
                self.unlisted.push(path.clone());
                return false;
            }
            Err(e) => {
                self.skip(path, SkipKind::ReadError, Some(e.to_string()));
                self.unlisted.push(path.clone());
                return false;
            }
        };
        let mut removed_temp = false;
        let mut entries = Vec::with_capacity(names.len());
        for raw in names {
            if is_root && raw == STATE_DIR {
                continue;
            }
            if raw.starts_with(self.opts.temp_prefix) {
                // Our own leftover from a killed run: it never held anything at a synced path.
                if self.opts.remove_own_temps
                    && dir
                        .stat(&raw)
                        .is_ok_and(|s| s.file_type != FileType::Directory)
                {
                    match dir.unlink(&raw) {
                        Ok(()) => removed_temp = true,
                        Err(_) => self.stale_temps.push((path.clone(), raw)),
                    }
                }
                continue;
            }
            match Name::new(raw) {
                Ok(name) => entries.push(name),
                Err(e) => self.skip(path, SkipKind::ReadError, Some(e.to_string())),
            }
        }
        // Removing a temporary moved this directory's own timestamps.
        let st = if removed_temp {
            dir.stat_self().unwrap_or(st)
        } else {
            st
        };
        self.dirs.push((path.clone(), st.mtime, st.ctime));
        for name in entries {
            path.push(name);
            self.entry(dir, path, st_mnt);
            path.pop();
        }
        true
    }

    fn entry(&mut self, dir: &Dir, path: &mut Vec<Name>, parent_mnt: u64) {
        let name = path.last().expect("an entry has a name").clone();
        let raw = name.as_bytes();
        self.current_inode = None;
        let st = match dir.stat(raw) {
            Ok(st) => st,
            Err(e) if is_not_found(&e) => {
                return self.unobservable(path, SkipKind::VanishedDuringScan, None);
            }
            Err(e) if is_permission(&e) => {
                return self.unobservable(path, SkipKind::PermissionDenied, Some(e.to_string()));
            }
            Err(e) => return self.unobservable(path, SkipKind::ReadError, Some(e.to_string())),
        };
        self.current_inode = Some((st.dev, st.ino));
        if st.mnt_id != 0 && parent_mnt != 0 && st.mnt_id != parent_mnt {
            return self.unobservable(path, SkipKind::MountPoint, None);
        }
        match st.file_type {
            FileType::RegularFile => self.file(dir, path, st),
            FileType::Directory => self.subdir(dir, path, st),
            FileType::Symlink => self.symlink(dir, path, st),
            FileType::Fifo => self.skip(path, SkipKind::Fifo, None),
            FileType::Socket => self.skip(path, SkipKind::Socket, None),
            FileType::CharacterDevice => self.skip(path, SkipKind::CharDevice, None),
            FileType::BlockDevice => self.skip(path, SkipKind::BlockDevice, None),
            FileType::Unknown => {
                self.skip(path, SkipKind::ReadError, Some("unknown file type".into()))
            }
        }
    }

    /// The second and later names of an inode join the first.
    fn seen_link(&mut self, path: &[Name], st: &Stat) -> bool {
        if st.nlink < 2 {
            return false;
        }
        match self.linked.get(&(st.dev, st.ino)) {
            Some(&i) => {
                self.nodes[i].paths.push(path.to_vec());
                true
            }
            None => {
                self.linked.insert((st.dev, st.ino), self.nodes.len());
                false
            }
        }
    }

    fn capture_meta(
        &mut self,
        dir: &Dir,
        path: &[Name],
        raw: &[u8],
        st: &Stat,
    ) -> (Metadata, bool) {
        let mut problems = Vec::new();
        let m = capture(
            dir,
            raw,
            st,
            self.opts.accounts,
            self.precision_ns,
            &mut problems,
        );
        let incomplete = !problems.is_empty();
        if incomplete {
            self.skip(path, SkipKind::ReadError, Some(problems.join("; ")));
        }
        (m, incomplete)
    }

    fn file(&mut self, dir: &Dir, path: &[Name], st: Stat) {
        if self.seen_link(path, &st) {
            return;
        }
        let raw = path.last().expect("an entry has a name").as_bytes();
        let fp = fingerprint(&st);
        if let Some(row) = self.opts.rows.get(&(st.dev, st.ino)) {
            if row.fp == fp
                && !row.racy
                && !row.incomplete
                && matches!(row.content, Content::File { .. })
            {
                self.nodes.push(Scanned {
                    paths: vec![path.to_vec()],
                    fp,
                    racy: false,
                    content: row.content.clone(),
                    metadata: row.scan.clone(),
                    metadata_incomplete: false,
                });
                return;
            }
        }
        for attempt in 0..READ_ATTEMPTS {
            let settling = dir.stat(raw).unwrap_or_else(|_| st.clone());
            wait_until_stable(&settling);
            let fd = match dir.open_read(raw) {
                Ok(fd) => fd,
                Err(e) if is_permission(&e) => {
                    self.forget_link(&st);
                    return self.unobservable(
                        path,
                        SkipKind::PermissionDenied,
                        Some(e.to_string()),
                    );
                }
                Err(e) if is_not_found(&e) => {
                    self.forget_link(&st);
                    return self.unobservable(path, SkipKind::VanishedDuringScan, None);
                }
                Err(e) => {
                    self.forget_link(&st);
                    return self.unobservable(path, SkipKind::ReadError, Some(e.to_string()));
                }
            };
            let before = match dir.stat(raw) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if before.file_type != FileType::RegularFile || before.ctime != settling.ctime {
                continue;
            }
            hit("scan.file.reading");
            let manifest = match manifest_of(&fd, before.size) {
                Ok(m) => m,
                Err(e) => {
                    // A file shrinking under the read ends the read early: that is a change, not damage.
                    if attempt + 1 < READ_ATTEMPTS {
                        continue;
                    }
                    self.forget_link(&st);
                    return self.unobservable(path, SkipKind::ReadError, Some(e.to_string()));
                }
            };
            let after = match rustix::fs::fstat(&fd) {
                Ok(a) => a,
                Err(_) => continue,
            };
            let same_inode = after.st_ino == before.ino;
            let (mtime, ctime) = dir
                .stat(raw)
                .map(|s| (s.mtime, s.ctime))
                .unwrap_or_default();
            if !same_inode
                || after.st_size as u64 != before.size
                || mtime != before.mtime
                || ctime != before.ctime
            {
                continue;
            }
            let (metadata, metadata_incomplete) = self.capture_meta(dir, path, raw, &before);
            let bytes = manifest.encode();
            let hash = Hash::of(&bytes);
            self.new_manifests.insert(hash, bytes);
            self.nodes.push(Scanned {
                paths: vec![path.to_vec()],
                fp: fingerprint(&before),
                racy: is_racy(&before, self.precision_ns),
                content: Content::File {
                    manifest: hash,
                    size: before.size,
                },
                metadata,
                metadata_incomplete,
            });
            return;
        }
        self.forget_link(&st);
        self.unobservable(path, SkipKind::KeepsChanging, None);
    }

    fn forget_link(&mut self, st: &Stat) {
        self.linked.remove(&(st.dev, st.ino));
    }

    fn symlink(&mut self, dir: &Dir, path: &[Name], st: Stat) {
        if self.seen_link(path, &st) {
            return;
        }
        let raw = path.last().expect("an entry has a name").as_bytes();
        match dir.read_link(raw) {
            Ok(target) => {
                let (metadata, metadata_incomplete) = self.capture_meta(dir, path, raw, &st);
                self.nodes.push(Scanned {
                    paths: vec![path.to_vec()],
                    fp: fingerprint(&st),
                    racy: is_racy(&st, self.precision_ns),
                    content: Content::Symlink {
                        target: Bytes(target),
                    },
                    metadata,
                    metadata_incomplete,
                });
            }
            Err(e) => {
                self.forget_link(&st);
                let kind = if is_not_found(&e) {
                    SkipKind::VanishedDuringScan
                } else if e.raw_os_error() == Some(rustix::io::Errno::INVAL.raw_os_error()) {
                    // Replaced by something else since the stat.
                    SkipKind::VanishedDuringScan
                } else {
                    SkipKind::ReadError
                };
                self.unobservable(path, kind, Some(e.to_string()));
            }
        }
    }

    fn subdir(&mut self, dir: &Dir, path: &mut Vec<Name>, st: Stat) {
        let raw = path.last().expect("an entry has a name").clone();
        let raw = raw.as_bytes();
        match dir.open_dir(raw) {
            Ok(child) => {
                let listed = self.walk_dir(&child, path, st.mnt_id);
                self.current_inode = Some((st.dev, st.ino));
                // Read after the walk: removing a leftover temporary moves the times.
                let st = match dir.stat(raw) {
                    Ok(s) if s.file_type == FileType::Directory => s,
                    _ => return self.unobservable(path, SkipKind::VanishedDuringScan, None),
                };
                let (metadata, incomplete) = self.capture_meta(dir, path, raw, &st);
                let metadata_incomplete = incomplete || !listed;
                self.nodes.push(Scanned {
                    paths: vec![path.clone()],
                    fp: fingerprint(&st),
                    racy: false,
                    content: Content::Dir,
                    metadata,
                    metadata_incomplete,
                });
            }
            Err(e) if is_permission(&e) => {
                self.skip(path, SkipKind::PermissionDenied, Some(e.to_string()));
                self.unlisted.push(path.clone());
                // A directory no one here can read must not push that state to devices that can:
                // like its contents, its record stands as it was until it can be observed.
                let (metadata, _) = self.capture_meta(dir, path, raw, &st);
                self.nodes.push(Scanned {
                    paths: vec![path.clone()],
                    fp: fingerprint(&st),
                    racy: false,
                    content: Content::Dir,
                    metadata,
                    metadata_incomplete: true,
                });
            }
            Err(e) if is_not_found(&e) => {
                self.unobservable(path, SkipKind::VanishedDuringScan, None)
            }
            // Replaced by a symlink or a file since the stat: never walked through.
            Err(e) => self.unobservable(path, SkipKind::VanishedDuringScan, Some(e.to_string())),
        }
    }

    /// Whether a directory still looks as it did when listed.
    fn dirs_unchanged(&self, root: &Dir) -> bool {
        self.dirs.iter().all(|(path, mtime, ctime)| {
            root.open_path(path.iter())
                .and_then(|d| d.stat_self())
                .is_ok_and(|s| s.mtime == *mtime && s.ctime == *ctime)
        })
    }
}

pub fn scan(root: &Dir, opts: &ScanOptions<'_>) -> std::io::Result<ScanResult> {
    let precision_ns = root.fstype_precision_ns();
    let mut walk = Walk {
        opts,
        precision_ns,
        nodes: Vec::new(),
        linked: HashMap::new(),
        skips: Vec::new(),
        unobserved: Vec::new(),
        unobserved_inodes: Vec::new(),
        unidentified: false,
        current_inode: None,
        unlisted: Vec::new(),
        new_manifests: BTreeMap::new(),
        stale_temps: Vec::new(),
        dirs: Vec::new(),
    };
    let st = root.stat_self()?;
    let mut path = Vec::new();
    walk.walk_dir(root, &mut path, st.mnt_id);
    hit("scan.done");
    let unstable = !walk.dirs_unchanged(root);

    let st = root.stat_self()?;
    let mut problems = Vec::new();
    let metadata = capture(root, b".", &st, opts.accounts, precision_ns, &mut problems);
    Ok(ScanResult {
        root: Scanned {
            paths: Vec::new(),
            fp: fingerprint(&st),
            racy: false,
            content: Content::Dir,
            metadata,
            metadata_incomplete: !problems.is_empty(),
        },
        nodes: walk.nodes,
        skips: walk.skips,
        unobserved: walk.unobserved,
        unobserved_inodes: walk.unobserved_inodes,
        unidentified: walk.unidentified,
        unlisted: walk.unlisted,
        unstable,
        new_manifests: walk.new_manifests.into_iter().collect(),
        stale_temps: walk.stale_temps,
    })
}
