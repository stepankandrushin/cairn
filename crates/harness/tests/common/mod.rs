//! What every failure test needs: real server and client processes in a throwaway directory, and an
//! exact picture of a tree to compare.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

pub const CAIRN: &str = env!("CARGO_BIN_EXE_h-cairn");
pub const SERVER: &str = env!("CARGO_BIN_EXE_h-cairn-server");

/// A directory on a real disk under the build directory, removed unless the test failed.
pub struct Sandbox {
    pub dir: PathBuf,
}

impl Sandbox {
    pub fn on_disk(label: &str) -> Self {
        Self::under(Path::new(env!("CARGO_TARGET_TMPDIR")), label)
    }

    /// In the system temp directory, for the matrices that spawn thousands of runs.
    pub fn fast(label: &str) -> Self {
        Self::under(&std::env::temp_dir(), label)
    }

    pub fn under(parent: &Path, label: &str) -> Self {
        let dir = parent.join(format!("cairn-{label}-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("kept for inspection: {}", self.dir.display());
        } else {
            // Trees under test may hold read-only directories.
            #[cfg(unix)]
            let _ = Command::new("chmod")
                .args(["-R", "u+rwx"])
                .arg(&self.dir)
                .output();
            // And on Windows read-only files, and what `chmod` made unreadable.
            #[cfg(windows)]
            if std::fs::remove_dir_all(&self.dir).is_err() {
                let _ = Command::new("icacls")
                    .arg(&self.dir)
                    .args(["/reset", "/T", "/C", "/Q"])
                    .output();
                let _ = Command::new("attrib")
                    .arg("-R")
                    .arg(self.dir.join("*"))
                    .args(["/S", "/D"])
                    .output();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

pub struct Run {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    fn from(out: Output) -> Self {
        Self {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Killed by a signal, or on Windows by a failpoint, which says so by its exit code.
    pub fn was_killed(&self) -> bool {
        if cfg!(windows) {
            self.code == Some(cairn_failpoint::KILLED_EXIT_CODE)
        } else {
            self.code.is_none()
        }
    }

    pub fn text(&self) -> String {
        format!("exit {:?}\n{}{}", self.code, self.stdout, self.stderr)
    }

    #[track_caller]
    pub fn expect_ok(self) -> Self {
        assert!(self.ok(), "{}", self.text());
        self
    }
}

pub struct Server {
    pub data: PathBuf,
    pub url: String,
    pub token_file: PathBuf,
    port: u16,
    child: Option<Child>,
    env: Vec<(String, String)>,
    /// More arguments to `serve`.
    args: Vec<String>,
}

/// Below the range the kernel hands out. A restarted server needs its port back, and one from that
/// range can go to any other test's connection or server the moment it is free.
///
/// Under `cargo test` the tests of a file share one process, and this counter keeps their ports apart.
/// Under cargo-nextest each test is a process of its own, running beside tests of every file, so one
/// test's stopped server could find its port taken by another's: there each test running at once has a
/// slot, NEXTEST_TEST_GLOBAL_SLOT, and the slot a block of the range to itself.
fn next_port() -> u16 {
    const FIRST: u32 = 20_000;
    const COUNT: u32 = 12_000;
    // 187 ports for each of 64 tests at once: no test has that many servers up together, and its
    // ports come round again only long after the servers that had them were stopped.
    const SLOTS: u32 = 64;
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let spread = std::process::id().wrapping_mul(31).wrapping_add(n);
    let slot = std::env::var("NEXTEST_TEST_GLOBAL_SLOT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    match slot {
        Some(slot) => {
            assert!(
                slot < SLOTS,
                "nextest slot {slot}: more tests at once than next_port has blocks for"
            );
            let block = COUNT / SLOTS;
            (FIRST + slot * block + spread % block) as u16
        }
        None => (FIRST + spread % COUNT) as u16,
    }
}

impl Server {
    pub fn start(data: PathBuf) -> Self {
        Self::start_with(data, Vec::new())
    }

    pub fn start_with(data: PathBuf, env: Vec<(String, String)>) -> Self {
        let out = Command::new(SERVER)
            .arg("init")
            .arg("--data")
            .arg(&data)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let token_file = data.join("token");
        let mut s = Self {
            data,
            url: String::new(),
            token_file,
            port: 0,
            child: None,
            env,
            args: Vec::new(),
        };
        s.spawn();
        s
    }

    fn spawn(&mut self) {
        let mut last = String::new();
        for _ in 0..50 {
            let port = if self.port == 0 {
                next_port()
            } else {
                self.port
            };
            let mut cmd = Command::new(SERVER);
            cmd.arg("serve")
                .arg("--data")
                .arg(&self.data)
                .arg("--listen")
                .arg(format!("127.0.0.1:{port}"))
                .args(&self.args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (k, v) in &self.env {
                cmd.env(k, v);
            }
            let mut child = cmd.spawn().unwrap();
            let stdout = child.stdout.take().unwrap();
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            if let Some(addr) = line.trim().strip_prefix("listening on ") {
                self.port = addr.rsplit(':').next().unwrap().parse().unwrap();
                self.url = format!("http://127.0.0.1:{}", self.port);
                self.child = Some(child);
                return;
            }
            // The port or the data directory's lock is not free yet.
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            last = String::from_utf8_lossy(&out.stderr).into_owned();
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("the server did not start: {last}");
    }

    pub fn set_env(&mut self, env: Vec<(String, String)>) {
        self.env = env;
    }

    /// More arguments to `serve`, from the next start on.
    pub fn set_args(&mut self, args: &[&str]) {
        self.args = args.iter().map(|a| a.to_string()).collect();
    }

    /// `kill -9`.
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn restart(&mut self) {
        self.kill();
        self.spawn();
    }

    /// SIGTERM, returning how long the server took to go.
    #[cfg(unix)]
    pub fn stop_gently(&mut self) -> std::time::Duration {
        let start = std::time::Instant::now();
        if let Some(mut c) = self.child.take() {
            Command::new("kill")
                .arg(c.id().to_string())
                .status()
                .unwrap();
            let _ = c.wait();
        }
        start.elapsed()
    }

    /// Whether the process is still up. A failpoint may have killed it.
    pub fn alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    /// Whether the process is still up once a kill, if there was one, has had time to end it. Windows
    /// says a process has ended only after its handles are closed, its sockets among them, so a
    /// client can see the connection reset while the process still reads as running.
    pub fn alive_after_a_kill(&mut self) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(10) {
            if !self.alive() {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }

    /// `fsck` on the data directory. The server is stopped for it, since it holds the lock.
    #[track_caller]
    pub fn fsck(&mut self) -> String {
        let was_up = self.child.is_some();
        self.kill();
        let out = Command::new(SERVER)
            .arg("fsck")
            .arg("--data")
            .arg(&self.data)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "fsck found damage:\n{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if was_up {
            self.spawn();
        }
        text
    }
}

impl Server {
    /// An administrator's command on this server's data directory, while it runs, as `cairn-server`
    /// takes them: `["user", "add", "alice"]`.
    pub fn admin(&self, args: &[&str]) -> Run {
        let out = Command::new(SERVER)
            .args(args)
            .arg("--data")
            .arg(&self.data)
            .output()
            .unwrap();
        Run::from(out)
    }

    /// A new user, with its token in a file of its own beside the data directory.
    #[track_caller]
    pub fn user(&self, name: &str) -> PathBuf {
        let file = self
            .data
            .parent()
            .unwrap()
            .join(format!("{name}-{:08x}.token", rand::random::<u32>()));
        self.admin(&["user", "add", name, "--token-file", file.to_str().unwrap()])
            .expect_ok();
        file
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

#[derive(Clone)]
pub struct Device {
    pub root: PathBuf,
    pub name: String,
}

impl Device {
    pub fn new(sandbox: &Sandbox, server: &Server, name: &str) -> Self {
        Self::with_flags(sandbox, server, name, &[])
    }

    pub fn with_flags(sandbox: &Sandbox, server: &Server, name: &str, flags: &[&str]) -> Self {
        let root = sandbox.path(name);
        std::fs::create_dir_all(&root).unwrap();
        let d = Self {
            root,
            name: name.into(),
        };
        let mut args: Vec<&str> = vec!["init", "--server", &server.url, "--device-name", name];
        let token = server.token_file.to_str().unwrap().to_string();
        args.extend(["--token-file", &token]);
        args.extend(flags);
        let r = root_str(&d.root);
        args.push(&r);
        run(&args, &[]).expect_ok();
        d
    }

    /// A root of `folder` with `token_file`'s token, or the run of `cairn init` that refused it.
    pub fn try_as_user(
        sandbox: &Sandbox,
        server: &Server,
        name: &str,
        token_file: &Path,
        folder: &str,
    ) -> Result<Self, Run> {
        let root = sandbox.path(name);
        std::fs::create_dir_all(&root).unwrap();
        let token = token_file.to_str().unwrap();
        let r = root_str(&root);
        let args = [
            "init",
            "--server",
            &server.url,
            "--device-name",
            name,
            "--token-file",
            token,
            "--folder",
            folder,
            &r,
        ];
        let run = run(&args, &[]);
        if !run.ok() {
            return Err(run);
        }
        Ok(Self {
            root,
            name: name.into(),
        })
    }

    #[track_caller]
    pub fn as_user(
        sandbox: &Sandbox,
        server: &Server,
        name: &str,
        token_file: &Path,
        folder: &str,
    ) -> Self {
        match Self::try_as_user(sandbox, server, name, token_file, folder) {
            Ok(d) => d,
            Err(run) => panic!("cairn init: {}", run.text()),
        }
    }

    pub fn cairn(&self, args: &[&str]) -> Run {
        self.cairn_env(args, &[])
    }

    pub fn cairn_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let r = root_str(&self.root);
        let mut all = vec!["--root", &r];
        all.extend(args);
        run(&all, env)
    }

    pub fn sync(&self) -> Run {
        self.cairn(&["sync"])
    }

    pub fn sync_env(&self, env: &[(&str, &str)]) -> Run {
        self.cairn_env(&["sync"], env)
    }

    #[track_caller]
    pub fn synced(&self) -> Run {
        self.sync().expect_ok()
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn write(&self, rel: &str, data: impl AsRef<[u8]>) {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, data).unwrap();
    }

    pub fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    pub fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.path(rel)).is_ok()
    }

    /// Names in a folder that look like conflict copies of `stem`.
    pub fn conflict_copies(&self, dir: &str, stem: &str) -> Vec<String> {
        std::fs::read_dir(self.path(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(stem) && n.contains("(conflict "))
            .collect()
    }
}

fn root_str(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

/// `cairn-server` with these arguments and environment, run to its end.
pub fn run_server(args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(SERVER);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    Run::from(cmd.output().unwrap())
}

pub fn run(args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(CAIRN);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    Run::from(cmd.output().unwrap())
}

/// Everything observable about one directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: char,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: (i64, i64),
    pub size: u64,
    pub content: Vec<u8>,
    pub target: Vec<u8>,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Entries sharing an inode share this number, assigned in path order.
    pub link_group: Option<usize>,
    pub data_runs: Vec<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Picture {
    pub entries: BTreeMap<Vec<u8>, Entry>,
}

/// What a picture leaves out when two trees legitimately differ in it.
#[derive(Clone, Copy, Default)]
pub struct Ignore {
    pub owner: bool,
    pub dir_mtime: bool,
    pub all_mtime: bool,
    pub xattrs: bool,
}

impl Picture {
    pub fn of(root: &Path) -> Self {
        let mut p = Picture::default();
        let mut inodes: BTreeMap<u64, usize> = BTreeMap::new();
        walk(root, Vec::new(), &mut p, &mut inodes, true);
        p
    }

    /// Every difference, as lines, empty when the trees match.
    pub fn diff(&self, other: &Picture, ignore: Ignore) -> Vec<String> {
        let mut out = Vec::new();
        let keys: std::collections::BTreeSet<_> =
            self.entries.keys().chain(other.entries.keys()).collect();
        for k in keys {
            let name = String::from_utf8_lossy(k);
            match (self.entries.get(k), other.entries.get(k)) {
                (None, _) => out.push(format!("{name}: only in the second tree")),
                (_, None) => out.push(format!("{name}: only in the first tree")),
                (Some(a), Some(b)) => {
                    let mut a = a.clone();
                    let mut b = b.clone();
                    if ignore.owner {
                        (a.uid, a.gid, b.uid, b.gid) = (0, 0, 0, 0);
                    }
                    if ignore.all_mtime || (ignore.dir_mtime && a.kind == 'd') {
                        (a.mtime, b.mtime) = ((0, 0), (0, 0));
                    }
                    if ignore.xattrs {
                        a.xattrs.clear();
                        b.xattrs.clear();
                    }
                    if a != b {
                        out.push(format!("{name}:\n  {}\n  {}", brief(&a), brief(&b)));
                    }
                }
            }
        }
        out
    }

    pub fn names(&self) -> Vec<String> {
        self.entries
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect()
    }
}

fn brief(e: &Entry) -> String {
    format!(
        "{} mode={:o} owner={}:{} mtime={:?} size={} content={}B target={:?} xattrs={:?} links={:?} runs={:?}",
        e.kind,
        e.mode,
        e.uid,
        e.gid,
        e.mtime,
        e.size,
        e.content.len(),
        String::from_utf8_lossy(&e.target),
        e.xattrs.keys().map(|k| String::from_utf8_lossy(k).into_owned()).collect::<Vec<_>>(),
        e.link_group,
        e.data_runs,
    )
}

#[cfg(unix)]
fn list_xattrs(path: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut buf = vec![0u8; 1 << 16];
    let Ok(n) = rustix::fs::llistxattr(path, &mut buf[..]) else {
        return out;
    };
    let names: Vec<Vec<u8>> = buf[..n]
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    for name in names {
        let mut val = vec![0u8; 1 << 16];
        if let Ok(len) = rustix::fs::lgetxattr(path, OsStr::from_bytes(&name), &mut val[..]) {
            val.truncate(len);
            out.insert(name, val);
        }
    }
    out
}

/// Windows holds none: a record from elsewhere stays stored, never applied.
#[cfg(windows)]
fn list_xattrs(_: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    BTreeMap::new()
}

/// A name's bytes as the server holds them: on Windows UTF-8, or WTF-8 for a lone surrogate.
pub fn name_bytes(name: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    return name.as_bytes().to_vec();
    #[cfg(windows)]
    return cairn_engine::fsops::os_name_bytes(name).unwrap();
}

/// What one `lstat` says, in the picture's terms. On Windows that is what Windows holds: a file's write
/// bits as its read-only attribute, which is all the engine reads of a mode there, no owner, and times
/// to NTFS's 100 ns.
struct Lstat {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: (i64, i64),
    nlink: u64,
    ino: u64,
}

#[cfg(unix)]
fn lstat(path: &Path, m: &std::fs::Metadata) -> Lstat {
    let _ = path;
    Lstat {
        mode: m.mode() & 0o7777,
        uid: m.uid(),
        gid: m.gid(),
        mtime: (m.mtime(), m.mtime_nsec()),
        nlink: m.nlink(),
        ino: m.ino(),
    }
}

#[cfg(windows)]
fn lstat(path: &Path, m: &std::fs::Metadata) -> Lstat {
    use std::os::windows::fs::MetadataExt;
    let ft = m.file_type();
    let mode = if ft.is_symlink() {
        0o777
    } else if ft.is_dir() {
        0o755
    } else if m.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    // FILETIME counts 100 ns from 1601.
    let ns = (i128::from(m.last_write_time()) - 116_444_736_000_000_000) * 100;
    let (nlink, ino) = file_links(path);
    Lstat {
        mode,
        uid: 0,
        gid: 0,
        mtime: (
            ns.div_euclid(1_000_000_000) as i64,
            ns.rem_euclid(1_000_000_000) as i64,
        ),
        nlink,
        ino,
    }
}

/// The link count and file ID of an entry, which std does not give on Windows yet. A link is opened as
/// itself, never followed.
#[cfg(windows)]
fn file_links(path: &Path) -> (u64, u64) {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    };
    let f = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    // SAFETY: the handle is open for the call, and the struct is what it fills.
    let info = unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        assert_ne!(
            GetFileInformationByHandle(f.as_raw_handle(), &mut info),
            0,
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        info
    };
    (
        u64::from(info.nNumberOfLinks),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    )
}

/// A file's mode other than 0644, for a change of metadata alone: this one, or on Windows, which holds
/// a file's write bits alone, read-only.
pub fn other_mode(unix: u32) -> u32 {
    if cfg!(windows) {
        0o444
    } else {
        unix
    }
}

/// A folder's mode as this platform holds it: on Windows always the same.
pub fn dir_mode(unix: u32) -> u32 {
    if cfg!(windows) {
        0o755
    } else {
        unix
    }
}

/// An entry's mode as the picture has it: on Windows a file's write bits alone.
pub fn mode_of(path: &Path) -> u32 {
    lstat(path, &std::fs::symlink_metadata(path).unwrap()).mode
}

/// An entry's modification time as the picture has it: on Windows to 100 ns.
pub fn mtime_of(path: &Path) -> (i64, i64) {
    lstat(path, &std::fs::symlink_metadata(path).unwrap()).mtime
}

/// How many names a file has.
pub fn nlink(path: &Path) -> u64 {
    lstat(path, &std::fs::symlink_metadata(path).unwrap()).nlink
}

/// A file's inode number, or on Windows its file ID.
pub fn inode(path: &Path) -> u64 {
    lstat(path, &std::fs::symlink_metadata(path).unwrap()).ino
}

/// One positional read of exactly this much.
fn read_exact_at(f: &std::fs::File, buf: &mut [u8], offset: u64) {
    #[cfg(unix)]
    std::os::unix::fs::FileExt::read_exact_at(f, buf, offset).unwrap();
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buf.len() {
            let n =
                std::os::windows::fs::FileExt::seek_read(f, &mut buf[done..], offset + done as u64)
                    .unwrap();
            assert!(n > 0, "the file ended early");
            done += n;
        }
    }
}

fn walk(
    dir: &Path,
    prefix: Vec<u8>,
    p: &mut Picture,
    inodes: &mut BTreeMap<u64, usize>,
    top: bool,
) {
    // The root's own record, under the empty name.
    if top {
        let m = std::fs::symlink_metadata(dir).unwrap();
        let st = lstat(dir, &m);
        p.entries.insert(
            Vec::new(),
            Entry {
                kind: 'd',
                mode: st.mode,
                uid: st.uid,
                gid: st.gid,
                mtime: st.mtime,
                size: 0,
                content: Vec::new(),
                target: Vec::new(),
                xattrs: list_xattrs(dir),
                link_group: None,
                data_runs: Vec::new(),
            },
        );
    }
    let mut names: Vec<(Vec<u8>, OsString)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let name = e.unwrap().file_name();
            (name_bytes(&name), name)
        })
        .collect();
    names.sort();
    for (bytes, name) in names {
        if top && name == ".cairn" {
            continue;
        }
        let path = dir.join(&name);
        let m = std::fs::symlink_metadata(&path).unwrap();
        let st = lstat(&path, &m);
        let mut rel = prefix.clone();
        if !rel.is_empty() {
            rel.push(b'/');
        }
        rel.extend_from_slice(&bytes);
        let ft = m.file_type();
        let mut e = Entry {
            kind: '?',
            mode: st.mode,
            uid: st.uid,
            gid: st.gid,
            mtime: st.mtime,
            size: 0,
            content: Vec::new(),
            target: Vec::new(),
            xattrs: list_xattrs(&path),
            link_group: None,
            data_runs: Vec::new(),
        };
        if ft.is_dir() {
            e.kind = 'd';
        } else if ft.is_symlink() {
            e.kind = 'l';
            e.target = name_bytes(std::fs::read_link(&path).unwrap().as_os_str());
        } else if ft.is_file() {
            e.kind = 'f';
            e.size = m.len();
            let f = std::fs::File::open(&path).unwrap();
            #[cfg(unix)]
            let fd = rustix::fd::AsFd::as_fd(&f);
            #[cfg(windows)]
            let fd = std::os::windows::io::AsHandle::as_handle(&f);
            e.data_runs = cairn_engine::chunker::data_runs(fd, m.len()).unwrap();
            // Data only: reading a huge hole as zeros would take hours.
            for (offset, len) in &e.data_runs {
                let mut buf = vec![0u8; *len as usize];
                read_exact_at(&f, &mut buf, *offset);
                e.content.extend_from_slice(&buf);
            }
        } else {
            e.kind = 's';
        }
        if !ft.is_dir() && st.nlink > 1 {
            let next = inodes.len();
            e.link_group = Some(*inodes.entry(st.ino).or_insert(next));
        }
        let is_dir = ft.is_dir();
        p.entries.insert(rel.clone(), e);
        if is_dir {
            walk(&path, rel, p, inodes, false);
        }
    }
}

/// A link to a folder: a symlink, or on Windows a junction, which needs no privilege to make and which
/// sync must not follow either.
pub fn dir_link(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let out = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "mklink {}: {}{}",
            link.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `cp -a`: a copy of a tree with its metadata. On Windows each file is copied by `CopyFileExW`, which
/// keeps its times and attributes.
pub fn copy_tree(from: &Path, to: &Path) {
    #[cfg(unix)]
    {
        let status = Command::new("cp")
            .arg("-a")
            .arg(from)
            .arg(to)
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[cfg(windows)]
    {
        let m = std::fs::symlink_metadata(from).unwrap();
        if m.is_dir() {
            std::fs::create_dir(to).unwrap();
            for e in std::fs::read_dir(from).unwrap() {
                let e = e.unwrap();
                copy_tree(&e.path(), &to.join(e.file_name()));
            }
        } else {
            std::fs::copy(from, to).unwrap();
        }
    }
}

/// Remove what `dir_link` made: on Windows a junction is a folder.
pub fn remove_dir_link(link: &Path) {
    #[cfg(unix)]
    std::fs::remove_file(link).unwrap();
    #[cfg(windows)]
    std::fs::remove_dir(link).unwrap();
}

/// Make a file writable before a person writes to it: on Windows a read-only one refuses a write, a
/// replacing rename and a delete, and editors ask to clear it first. Nothing to do on Unix, where the
/// modes used here all let their owner write.
// On Windows `set_readonly(false)` clears the attribute and nothing else.
#[allow(clippy::permissions_set_readonly_false)]
pub fn make_writable(path: &Path) {
    #[cfg(windows)]
    if let Ok(m) = std::fs::symlink_metadata(path) {
        let mut perms = m.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }
    #[cfg(unix)]
    let _ = path;
}

/// Remove a file as a person would: on Windows a read-only one too, as `del /f` does, since a plain
/// delete refuses it there.
pub fn remove_file(path: &Path) {
    make_writable(path);
    std::fs::remove_file(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

#[cfg(unix)]
pub fn set_xattr(path: &Path, name: &str, value: &[u8]) {
    rustix::fs::lsetxattr(path, name, value, rustix::fs::XattrFlags::empty()).unwrap();
}

#[cfg(unix)]
pub fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
}

/// A mode, as far as Windows holds one: a file's write bits as its read-only attribute, as the engine
/// reads them, and no read bits as reading denied by the ACL, which is how a file or folder is made
/// unreadable there. A folder's write bits mean nothing to Windows.
#[cfg(windows)]
pub fn chmod(path: &Path, mode: u32) {
    let m = std::fs::symlink_metadata(path).unwrap();
    if m.is_file() {
        let mut perms = m.permissions();
        perms.set_readonly(mode & 0o222 == 0);
        std::fs::set_permissions(path, perms).unwrap();
    }
    // What this process denied, by file ID, which follows a file through a rename. A probe cannot say:
    // opened for backup, as a folder has to be, an elevated session reads it anyway.
    static DENIED: std::sync::Mutex<std::collections::BTreeSet<u64>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let id = inode(path);
    let mut denied = DENIED.lock().unwrap();
    let deny = mode & 0o444 == 0;
    if deny == denied.contains(&id) {
        return;
    }
    // Everyone, which includes this user.
    let mut cmd = Command::new("icacls");
    cmd.arg(path);
    if deny {
        cmd.args(["/deny", "*S-1-1-0:(RD)"]);
    } else {
        cmd.args(["/remove:d", "*S-1-1-0"]);
    }
    let out = cmd.arg("/Q").output().unwrap();
    assert!(
        out.status.success(),
        "icacls {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stdout)
    );
    if deny {
        denied.insert(id);
    } else {
        denied.remove(&id);
    }
}

/// A specific modification time, to the nanosecond.
#[cfg(unix)]
pub fn set_mtime(path: &Path, sec: i64, nsec: i64) {
    let t = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: sec,
            tv_nsec: nsec as _,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: sec,
            tv_nsec: nsec as _,
        },
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &t,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

/// A specific modification time, to NTFS's 100 ns. A link is set itself, never followed.
#[cfg(windows)]
pub fn set_mtime(path: &Path, sec: i64, nsec: i64) {
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::{Duration, UNIX_EPOCH};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES,
    };
    let t = if sec >= 0 {
        UNIX_EPOCH + Duration::new(sec as u64, nsec as u32)
    } else {
        UNIX_EPOCH - Duration::from_secs(sec.unsigned_abs()) + Duration::from_nanos(nsec as u64)
    };
    let f = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap();
    f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

#[cfg(unix)]
pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// No Windows account passes every check the way root does.
#[cfg(windows)]
pub fn is_root() -> bool {
    false
}

/// Set to 1 by GitHub's CI (.github/workflows/ci.yml), where every test must run.
pub const NO_SKIP: &str = "CAIRN_NO_SKIP";

/// Whether `NO_SKIP` is set here, to anything but empty or 0.
pub fn no_skip() -> bool {
    std::env::var_os(NO_SKIP).is_some_and(|v| !v.is_empty() && v != "0")
}

/// This test cannot run here, for `why`: on a contributor's machine it says so and passes, and the
/// caller returns. Where `NO_SKIP` is set it fails instead. libtest hides a passing test's output, so a
/// skip in CI would read as a pass of a test that never ran.
#[track_caller]
pub fn skip(why: &str) {
    if no_skip() {
        panic!("{why}, and {NO_SKIP} says every test must run here");
    }
    eprintln!("{why}: skipped");
}

/// For a test that needs a permission refused: as root none is, so it skips, and this says true.
#[track_caller]
pub fn skip_as_root() -> bool {
    let root = is_root();
    if root {
        skip("root is refused nothing, and this test needs a refusal");
    }
    root
}

/// Re-runs the `#[ignore]`d test `test` of this test binary in a private user and mount namespace, after
/// mounting a tmpfs with `options` on `dir` when `tmpfs` is given, with `CAIRN_INNER` naming `dir`. None,
/// after `skip`, where this user may not make the namespace: Ubuntu's AppArmor forbids it by default, which
/// GitHub's workflow turns off. Otherwise whether the inner test passed, with its output printed when it
/// did not.
#[track_caller]
pub fn in_namespace(test: &str, tmpfs: Option<(&Path, &str)>) -> Option<bool> {
    match Command::new("unshare").args(["-Urm", "true"]).output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let said = String::from_utf8_lossy(&o.stderr);
            skip(&format!(
                "no unprivileged namespaces here: `unshare -Urm true` exited with {}: {}",
                o.status,
                said.trim()
            ));
            return None;
        }
        Err(e) => {
            skip(&format!("no unprivileged namespaces here: unshare: {e}"));
            return None;
        }
    }
    let exe = std::env::current_exe().unwrap();
    let mut script = String::new();
    let mut cmd = Command::new("unshare");
    if let Some((dir, options)) = tmpfs {
        script = format!("mount -t tmpfs -o {options} tmpfs '{}' && ", dir.display());
        cmd.env("CAIRN_INNER", dir);
    }
    script += &format!(
        "exec '{}' --exact {test} --ignored --nocapture --test-threads=1",
        exe.display()
    );
    let out = cmd.args(["-Urm", "sh", "-c", &script]).output().unwrap();
    if !out.status.success() {
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Some(out.status.success())
}

/// Self-checking file content: a half-written file cannot pass for a whole one, and each version is
/// unique, so "this version survived" is a plain search.
pub mod token {
    pub fn make(id: u64, size: usize) -> Vec<u8> {
        let head = format!("TOKEN {id} {size}\n");
        let head_len = head.len();
        let mut out = head.into_bytes();
        let mut x = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        while out.len() < size {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(size.max(head_len));
        out.extend_from_slice(format!("\nEND {id}\n").as_bytes());
        out
    }

    /// The token ID of a whole file, or `None` for anything else.
    pub fn parse(data: &[u8]) -> Option<u64> {
        let line_end = data.iter().position(|b| *b == b'\n')?;
        let head = std::str::from_utf8(&data[..line_end]).ok()?;
        let mut parts = head.split(' ');
        if parts.next()? != "TOKEN" {
            return None;
        }
        let id: u64 = parts.next()?.parse().ok()?;
        let size: usize = parts.next()?.parse().ok()?;
        (make(id, size) == data).then_some(id)
    }

    /// Every token in a picture, panicking on anything that is not a whole token file.
    pub fn all_in(picture: &super::Picture) -> std::collections::BTreeSet<u64> {
        let mut out = std::collections::BTreeSet::new();
        for (path, e) in &picture.entries {
            if e.kind == 'f' {
                let name = String::from_utf8_lossy(path);
                if name
                    .rsplit('/')
                    .next()
                    .is_some_and(|n| n.starts_with(".cairn-tmp-"))
                {
                    continue;
                }
                let id = parse(&e.content).unwrap_or_else(|| {
                    panic!("{name} is not a whole file: {} bytes", e.content.len())
                });
                out.insert(id);
            }
        }
        out
    }
}

/// Files a client left behind under its temporary prefix.
pub fn leftover_temps(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(list) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in list.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if d == root && name == ".cairn" {
                continue;
            }
            if name.starts_with(".cairn-tmp-") {
                out.push(e.path().display().to_string());
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            }
        }
    }
    out
}

/// Sync every device in turn until a whole pass changes nothing, then require identical trees.
#[track_caller]
pub fn converge(devices: &[&Device]) -> Picture {
    for pass in 0..12 {
        let mut quiet = true;
        for d in devices {
            let run = d.cairn(&["sync", "--allow-mass-delete"]);
            assert!(run.ok(), "pass {pass}, device {}: {}", d.name, run.text());
            if !run.stdout.contains(" 0 steps applied here, 0 commits") {
                quiet = false;
            }
        }
        if quiet {
            let first = Picture::of(&devices[0].root);
            for d in &devices[1..] {
                let other = Picture::of(&d.root);
                let diff = first.diff(
                    &other,
                    Ignore {
                        owner: true,
                        ..Ignore::default()
                    },
                );
                assert_eq!(
                    diff,
                    Vec::<String>::new(),
                    "{} and {} differ",
                    devices[0].name,
                    d.name
                );
            }
            return first;
        }
    }
    panic!("the devices did not converge in 12 passes");
}

// --- A client's state file, read from outside ---

fn state_read_only(state: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(state, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
}

/// (page count, free pages).
pub fn state_pages(state: &Path) -> (u64, u64) {
    let db = state_read_only(state);
    let get = |p: &str| -> u64 {
        db.pragma_query_value(None, p, |r| r.get::<_, i64>(0))
            .unwrap() as u64
    };
    (get("page_count"), get("freelist_count"))
}

/// Every table's rows, and the schema with its triggers but not where each table starts, which is
/// what a rewrite moves: what a compaction, or one killed, must leave as it was.
pub fn state_everything(state: &Path) -> Vec<(String, Vec<String>)> {
    let db = state_read_only(state);
    let check: String = db
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok", "{}", state.display());
    let tables: Vec<String> = {
        let mut stmt = db
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let names = stmt.query_map([], |r| r.get(0)).unwrap();
        names.map(Result::unwrap).collect()
    };
    let mut all = Vec::new();
    for table in ["sqlite_master".to_string()].into_iter().chain(tables) {
        let sql = match table.as_str() {
            "sqlite_master" => "SELECT type, name, tbl_name, sql FROM sqlite_master".to_string(),
            t => format!("SELECT * FROM \"{t}\""),
        };
        let mut stmt = db.prepare(&sql).unwrap();
        let n = stmt.column_count();
        let mut rows: Vec<String> = stmt
            .query_map([], |r| {
                (0..n)
                    .map(|i| r.get::<_, rusqlite::types::Value>(i))
                    .collect::<rusqlite::Result<Vec<rusqlite::types::Value>>>()
            })
            .unwrap()
            .map(|r| format!("{:?}", r.unwrap()))
            .collect();
        rows.sort();
        all.push((table, rows));
    }
    all
}

#[track_caller]
pub fn same_state(got: &[(String, Vec<String>)], want: &[(String, Vec<String>)], context: &str) {
    let counts = |all: &[(String, Vec<String>)]| -> Vec<String> {
        all.iter()
            .map(|(t, rows)| format!("{t}: {}", rows.len()))
            .collect()
    };
    let differ: Vec<&String> = got
        .iter()
        .zip(want)
        .filter(|(g, w)| g != w)
        .map(|(g, _)| &g.0)
        .collect();
    assert!(
        got.len() == want.len() && differ.is_empty(),
        "{context}: differ: {differ:?}; got {:?}, wanted {:?}",
        counts(got),
        counts(want)
    );
}

/// Each commit's operations in a server's journal, as the server sends them: read beside it.
pub fn journal_json(data: &Path) -> Vec<String> {
    use cairn_store::MetaStore;
    let mut store = cairn_store::SqliteStore::open_with(
        &data.join("meta.sqlite"),
        cairn_store::sqlite::Opening {
            first_token: None,
            beside_server: true,
        },
    )
    .unwrap();
    let mut out = Vec::new();
    for ns in store.namespaces().unwrap() {
        let changes = store.changes(&ns, 0, u32::MAX, usize::MAX).unwrap();
        out.extend(
            changes
                .iter()
                .map(|c| serde_json::to_string(&c.ops).unwrap()),
        );
    }
    out
}

pub fn wal_len(state: &Path) -> u64 {
    let mut wal = state.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(PathBuf::from(wal)).map_or(0, |m| m.len())
}

// --- The state's records, read and written from outside with SQL alone, as another build might ---

/// The base node and the row of every object in the state, each as stored and decoded.
fn state_records(
    db: &rusqlite::Connection,
) -> Vec<(
    Vec<u8>,
    Option<cairn_protocol::Node>,
    Option<cairn_engine::state::Row>,
)> {
    let mut stmt = db.prepare("SELECT object, node, row FROM objects").unwrap();
    let all = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Option<Vec<u8>>>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .map(|(id, node, row)| {
            let node = node.map(|n| cairn_engine::record::decode_node(&n).unwrap());
            let row = row.map(|r| cairn_engine::record::decode_row(&r, node.as_ref()).unwrap());
            (id, node, row)
        })
        .collect();
    all
}

/// Whether a base node has an entry of this name.
pub fn named(node: Option<&cairn_protocol::Node>, name: &[u8]) -> bool {
    node.is_some_and(|n| n.links.iter().any(|l| l.name.as_bytes() == name))
}

/// The objects whose base node has an entry named `name`.
pub fn objects_named(state: &Path, name: &[u8]) -> Vec<Vec<u8>> {
    let db = rusqlite::Connection::open(state).unwrap();
    state_records(&db)
        .into_iter()
        .filter(|(_, n, _)| named(n.as_ref(), name))
        .map(|(id, _, _)| id)
        .collect()
}

/// The row of the one object whose base node has an entry named `name`.
pub fn row_named(state: &Path, name: &[u8]) -> cairn_engine::state::Row {
    let db = rusqlite::Connection::open(state).unwrap();
    let mut found: Vec<_> = state_records(&db)
        .into_iter()
        .filter(|(_, n, _)| named(n.as_ref(), name))
        .filter_map(|(_, _, r)| r)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "rows named {:?}",
        String::from_utf8_lossy(name)
    );
    found.remove(0)
}

/// Each row whose base node `pick` takes, changed by `edit` and written back with an UPDATE of its own,
/// as another build or a person would: how many changed.
pub fn edit_rows(
    state: &Path,
    pick: impl Fn(Option<&cairn_protocol::Node>) -> bool,
    edit: impl Fn(&mut cairn_engine::state::Row),
) -> usize {
    let db = rusqlite::Connection::open(state).unwrap();
    let mut n = 0;
    for (id, node, row) in state_records(&db) {
        let Some(mut row) = row.filter(|_| pick(node.as_ref())) else {
            continue;
        };
        let was = row.clone();
        edit(&mut row);
        if row != was {
            let bytes = cairn_engine::record::encode_row(&row, node.as_ref());
            n += db
                .execute(
                    "UPDATE objects SET row = ?1 WHERE object = ?2",
                    rusqlite::params![bytes, id],
                )
                .unwrap();
        }
    }
    n
}
