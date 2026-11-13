//! What the handler knows of the badges in each root, folder by folder, and which folders to ask for
//! again when the daemon says something changed. Nothing here does any I/O, so it is tested everywhere.
//!
//! Explorer asks about one entry at a time, for each entry it shows, from its own threads, and waits for
//! the answer: so an entry of a folder not known yet has no badge for now, its folder is asked for as a
//! whole on another thread, and Explorer is told to look at the entry again once it is known.

use cairn_protocol::local::{Badge, Badges, RootEntry};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Folders kept: past this, the least used half goes, to be asked for again if shown again.
pub const MAX_FOLDERS: usize = 2048;
/// Entries of a folder asked about before it is known, each told when it is: Explorer shows a few
/// thousand at the most at once.
const MAX_ASKED: usize = 20_000;
/// A folder the daemon did not answer for is asked for again on the next look after this, or as soon
/// as the daemon is watched again.
pub const RETRY: Duration = Duration::from_secs(10);

/// A root as Explorer names paths in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub id: String,
    /// Its folder, `\` between names, none at the end, without `\\?\`.
    pub path: String,
}

/// A path written the one way: `\` between names, none at the end, without `\\?\` (`\\?\UNC\` back to
/// `\\`).
pub fn plain(path: &str) -> String {
    let p = match path.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => path.strip_prefix(r"\\?\").unwrap_or(path).to_string(),
    };
    p.replace('/', "\\").trim_end_matches('\\').to_string()
}

/// The roots of the registry, the deepest first, so that a root inside another is found as itself.
pub fn roots(entries: &[RootEntry]) -> Vec<Root> {
    let mut out: Vec<Root> = entries
        .iter()
        .filter_map(|e| {
            let path = plain(e.path.to_str()?);
            (!path.is_empty()).then(|| Root {
                id: e.root_id.clone(),
                path,
            })
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.path.chars().count()));
    out
}

/// Two names alike to NTFS, near enough: case folded as Unicode lowers it.
fn same_char(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// What is left of `path` under `root`, `""` for the root itself, or `None` if it is not under it.
fn under<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let mut chars = path.char_indices();
    for r in root.chars() {
        let (_, c) = chars.next()?;
        if !same_char(c, r) {
            return None;
        }
    }
    let rest = match chars.next() {
        None => return Some(""),
        Some((at, _)) => &path[at..],
    };
    rest.strip_prefix('\\').filter(|r| !r.is_empty())
}

/// The root a path is in, and the path relative to it as the local API takes one: names joined by `/`,
/// the root itself `""`.
pub fn locate<'a>(roots: &'a [Root], path: &str) -> Option<(&'a Root, String)> {
    let path = plain(path);
    roots
        .iter()
        .find_map(|r| under(&path, &r.path).map(|rel| (r, rel.replace('\\', "/"))))
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// A printed path's folder and last name; the root's folder is `""`, and the root itself has neither.
fn split(rel: &str) -> Option<(&str, &str)> {
    if rel.is_empty() {
        return None;
    }
    Some(rel.rsplit_once('/').unwrap_or(("", rel)))
}

/// The full path of an entry of a root, as Explorer names it, to tell it to look again.
pub fn full(root: &Root, rel: &str) -> String {
    if rel.is_empty() {
        root.path.clone()
    } else {
        format!("{}\\{}", root.path, rel.replace('/', "\\"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Have {
    /// Asked for, not answered yet.
    Wanted,
    Known,
    /// Not answered, at this time.
    Failed(Instant),
}

#[derive(Debug)]
struct Folder {
    /// As asked for, in the case Explorer gave.
    rel: String,
    have: Have,
    own: Option<Badge>,
    /// By name in lower case.
    entries: HashMap<String, Badge>,
    /// Paths Explorer asked about while this was not known.
    asked: HashSet<String>,
    used: u64,
}

/// One process's knowledge of the badges, as Explorer and every program showing a file dialog has.
#[derive(Default)]
pub struct Cache {
    /// By root ID and the folder's path in lower case.
    folders: HashMap<(String, String), Folder>,
    tick: u64,
}

impl Cache {
    /// The badge of a path under a root, if it is known; and the folder to ask the daemon for, if that
    /// is to be asked for now.
    pub fn look(&mut self, root: &Root, rel: &str, path: &str) -> (Option<Badge>, Option<String>) {
        let (folder, name) = split(rel).unwrap_or(("", ""));
        self.tick += 1;
        let tick = self.tick;
        let key = (root.id.clone(), lower(folder));
        let f = self.folders.entry(key).or_insert_with(|| Folder {
            rel: folder.to_string(),
            have: Have::Wanted,
            own: None,
            entries: HashMap::new(),
            asked: HashSet::new(),
            used: 0,
        });
        let fresh = f.used == 0;
        f.used = tick;
        let ask = match f.have {
            Have::Known => {
                let badge = match rel.is_empty() {
                    true => f.own,
                    false => f.entries.get(&lower(name)).copied(),
                };
                return (badge, None);
            }
            // Asked for already, unless just now.
            Have::Wanted => fresh,
            Have::Failed(at) if at.elapsed() >= RETRY => {
                f.have = Have::Wanted;
                true
            }
            Have::Failed(_) => false,
        };
        if f.asked.len() < MAX_ASKED {
            f.asked.insert(path.to_string());
        }
        let ask = ask.then(|| f.rel.clone());
        if fresh {
            self.trim();
        }
        (None, ask)
    }

    /// The daemon's answer for a folder: what Explorer is to look at again, by full path. That is what
    /// it asked about meanwhile, and every entry whose badge is not what it was.
    pub fn answered(&mut self, root: &Root, reply: Badges) -> Vec<String> {
        let key = (root.id.clone(), lower(&reply.folder));
        let entries: HashMap<String, Badge> = reply
            .entries
            .iter()
            .map(|e| (lower(&e.name), e.badge))
            .collect();
        let mut tell: Vec<String> = Vec::new();
        let f = self.folders.entry(key).or_insert_with(|| Folder {
            rel: reply.folder.clone(),
            have: Have::Wanted,
            own: None,
            entries: HashMap::new(),
            asked: HashSet::new(),
            used: 0,
        });
        tell.extend(f.asked.drain());
        if f.have == Have::Known {
            let join = |name: &str| match reply.folder.is_empty() {
                true => name.to_string(),
                false => format!("{}/{name}", reply.folder),
            };
            for e in &reply.entries {
                if f.entries.get(&lower(&e.name)) != Some(&e.badge) {
                    tell.push(full(root, &join(&e.name)));
                }
            }
            for (name, _) in f.entries.iter().filter(|(n, _)| !entries.contains_key(*n)) {
                tell.push(full(root, &join(name)));
            }
            if f.own != reply.badge {
                tell.push(full(root, &reply.folder));
            }
        }
        f.own = reply.badge;
        f.entries = entries;
        f.have = Have::Known;
        tell.sort();
        tell.dedup();
        tell
    }

    /// The daemon did not answer for a folder: it has no badges until it does.
    pub fn failed(&mut self, root: &Root, folder: &str) {
        if let Some(f) = self.folders.get_mut(&(root.id.clone(), lower(folder))) {
            if f.have != Have::Known {
                f.have = Have::Failed(Instant::now());
            }
        }
    }

    /// The folders of a root to ask for again, as the daemon says what changed: those that list one of
    /// `paths` or a folder above it, and those under one; with `all`, every one.
    pub fn changed(&self, root: &Root, all: bool, paths: &[String]) -> Vec<String> {
        let paths: Vec<String> = paths.iter().map(|p| lower(p)).collect();
        let related = |folder: &str| {
            paths.iter().any(|p| {
                folder.is_empty()
                    || p.is_empty()
                    || p == folder
                    || p.strip_prefix(folder).is_some_and(|r| r.starts_with('/'))
                    || folder
                        .strip_prefix(p.as_str())
                        .is_some_and(|r| r.starts_with('/'))
            })
        };
        let mut out: Vec<String> = self
            .folders
            .iter()
            .filter(|((id, folder), _)| *id == root.id && (all || related(folder)))
            .map(|(_, f)| f.rel.clone())
            .collect();
        out.sort();
        out
    }

    /// The least used half, once there are too many.
    fn trim(&mut self) {
        if self.folders.len() <= MAX_FOLDERS {
            return;
        }
        let mut used: Vec<u64> = self.folders.values().map(|f| f.used).collect();
        used.sort_unstable();
        let cut = used[used.len() / 2];
        self.folders
            .retain(|_, f| f.used >= cut || f.have == Have::Wanted);
    }

    pub fn len(&self) -> usize {
        self.folders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::local::BadgeEntry;

    fn root() -> Root {
        Root {
            id: "r".into(),
            path: r"D:\Sync".into(),
        }
    }

    fn reply(folder: &str, own: Option<Badge>, entries: &[(&str, Badge)]) -> Badges {
        Badges {
            folder: folder.into(),
            badge: own,
            entries: entries
                .iter()
                .map(|(n, b)| BadgeEntry {
                    name: n.to_string(),
                    badge: *b,
                })
                .collect(),
        }
    }

    #[test]
    fn a_path_is_found_in_its_root_whatever_its_case_or_form() {
        let entries = |paths: &[&str]| -> Vec<RootEntry> {
            paths
                .iter()
                .enumerate()
                .map(|(i, p)| RootEntry {
                    path: p.into(),
                    root_id: format!("id{i}"),
                    server: String::new(),
                    added: 0,
                })
                .collect()
        };
        let roots = roots(&entries(&[
            r"D:\Sync",
            r"\\?\D:\Sync\inner\",
            r"C:\Users\you\Cairn",
        ]));
        let at = |p: &str| locate(&roots, p).map(|(r, rel)| (r.id.clone(), rel));
        assert_eq!(at(r"D:\Sync"), Some(("id0".into(), String::new())));
        assert_eq!(
            at(r"d:\SYNC\Docs\a.txt"),
            Some(("id0".into(), "Docs/a.txt".into()))
        );
        assert_eq!(at(r"D:\Sync\inner\x"), Some(("id1".into(), "x".into())));
        assert_eq!(
            at(r"\\?\D:\Sync\inner"),
            Some(("id1".into(), String::new()))
        );
        assert_eq!(at(r"D:\Syncy\x"), None);
        assert_eq!(at(r"E:\"), None);
        assert_eq!(
            at(r"C:\Users\you\cairn\ü"),
            Some(("id2".into(), "ü".into()))
        );
        let users = roots.iter().find(|r| r.id == "id2").unwrap();
        assert_eq!(full(users, "a/b"), r"C:\Users\you\Cairn\a\b");
    }

    #[test]
    fn an_entry_of_a_folder_not_known_asks_for_the_folder_once_and_is_told_when_it_is() {
        let r = root();
        let mut c = Cache::default();
        assert_eq!(
            c.look(&r, "Docs/a.txt", r"D:\Sync\Docs\a.txt"),
            (None, Some("Docs".into()))
        );
        assert_eq!(
            c.look(&r, "Docs/b.txt", r"D:\Sync\Docs\b.txt"),
            (None, None)
        );
        let tell = c.answered(
            &r,
            reply(
                "Docs",
                Some(Badge::Syncing),
                &[("a.txt", Badge::Synced), ("B.txt", Badge::Syncing)],
            ),
        );
        assert_eq!(tell, [r"D:\Sync\Docs\a.txt", r"D:\Sync\Docs\b.txt"]);
        assert_eq!(c.look(&r, "Docs/a.txt", "x"), (Some(Badge::Synced), None));
        assert_eq!(c.look(&r, "docs/b.TXT", "x"), (Some(Badge::Syncing), None));
        assert_eq!(c.look(&r, "Docs/excluded.log", "x"), (None, None));
        // The root itself is its own folder's badge.
        assert_eq!(c.look(&r, "", r"D:\Sync"), (None, Some(String::new())));
        let tell = c.answered(
            &r,
            reply("", Some(Badge::Warning), &[("Docs", Badge::Syncing)]),
        );
        assert_eq!(tell, [r"D:\Sync"]);
        assert_eq!(c.look(&r, "", "x"), (Some(Badge::Warning), None));
    }

    #[test]
    fn an_answer_again_tells_only_what_differs() {
        let r = root();
        let mut c = Cache::default();
        c.look(&r, "d/a", "x");
        c.answered(
            &r,
            reply(
                "d",
                Some(Badge::Synced),
                &[
                    ("a", Badge::Synced),
                    ("b", Badge::Synced),
                    ("c", Badge::Synced),
                ],
            ),
        );
        let tell = c.answered(
            &r,
            reply(
                "d",
                Some(Badge::Syncing),
                &[
                    ("a", Badge::Synced),
                    ("b", Badge::Syncing),
                    ("n", Badge::Syncing),
                ],
            ),
        );
        assert_eq!(
            tell,
            [r"D:\Sync\d", r"D:\Sync\d\b", r"D:\Sync\d\c", r"D:\Sync\d\n"]
        );
    }

    #[test]
    fn a_folder_not_answered_for_is_asked_for_again_after_a_while() {
        let r = root();
        let mut c = Cache::default();
        assert!(c.look(&r, "d/a", "x").1.is_some());
        c.failed(&r, "d");
        assert_eq!(c.look(&r, "d/a", "x"), (None, None));
        // As the watch connecting again asks for every folder known.
        assert_eq!(c.changed(&r, true, &[]), ["d"]);
    }

    #[test]
    fn a_change_asks_again_for_the_folders_that_list_it_or_are_above_or_under_it() {
        let r = root();
        let mut c = Cache::default();
        for f in ["", "a", "a/b", "a/b/c", "x", "A/bee"] {
            c.look(&r, format!("{f}/e").trim_start_matches('/'), "x");
        }
        let other = Root {
            id: "other".into(),
            path: r"D:\o".into(),
        };
        c.look(&other, "a/e", "x");
        assert_eq!(c.changed(&r, false, &["a/b/f".into()]), ["", "a", "a/b"]);
        assert_eq!(
            c.changed(&r, false, &["a/B".into()]),
            ["", "a", "a/b", "a/b/c"]
        );
        assert_eq!(c.changed(&r, false, &[String::new()]).len(), 6);
        assert_eq!(c.changed(&r, true, &[]).len(), 6);
        assert!(c.changed(&r, false, &[]).is_empty());
    }

    #[test]
    fn past_the_most_folders_the_least_used_half_goes() {
        let r = root();
        let mut c = Cache::default();
        for i in 0..=MAX_FOLDERS {
            let rel = format!("f{i}/e");
            c.look(&r, &rel, "x");
            c.answered(
                &r,
                reply(
                    &format!("f{i}"),
                    Some(Badge::Synced),
                    &[("e", Badge::Synced)],
                ),
            );
        }
        c.look(&r, "new/e", "x");
        assert!(c.len() <= MAX_FOLDERS / 2 + 2, "{}", c.len());
        assert_eq!(
            c.look(&r, &format!("f{MAX_FOLDERS}/e"), "x").0,
            Some(Badge::Synced)
        );
    }
}
