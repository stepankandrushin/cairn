//! A sync with nothing to do, told for what it is without the whole tree in memory (engine steady.rs),
//! and never one with something to do taken for it. Every harness build runs the full sync after each
//! fast one and compares the two, failing the sync where they differ; `CAIRN_FP_FAST_ORACLE=0` turns
//! that off, where the point is what a release build does on its own.
//!
//! And what the daemon makes of a tree that size: no wake for what a device did itself, a pause that
//! counts what every sync repeats, a watcher that holds no folder open while it waits its turn, and
//! a check of the server's history that does not download the commit at the cursor.

mod common;
use common::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// As a release build runs: the fast path's answer is the answer.
const RELEASE: (&str, &str) = ("CAIRN_FP_FAST_ORACLE", "0");
#[cfg(unix)]
const DAY: &str = "86400";
const SOON: Duration = Duration::from_secs(30);

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

/// A sync with every failpoint it passes written down.
fn logged(d: &Device, env: &[(&str, &str)]) -> (Run, Vec<String>) {
    let log = d
        .root
        .with_extension(format!("steps-{:016x}", rand::random::<u64>()));
    let mut all: Vec<(&str, &str)> = env.to_vec();
    let path = log.to_str().unwrap().to_string();
    all.push(("CAIRN_FP_LOG", &path));
    let run = d.sync_env(&all);
    let steps = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let _ = std::fs::remove_file(&log);
    (run, steps)
}

fn took(steps: &[String], step: &str) -> bool {
    steps.iter().any(|s| s == step)
}

fn count(steps: &[String], step: &str) -> usize {
    steps.iter().filter(|s| *s == step).count()
}

/// Until a sync is a fast one: the sync after a change writes down what it agreed on, and the one
/// after that has nothing to do.
#[track_caller]
fn until_fast(d: &Device) {
    for _ in 0..4 {
        let (run, steps) = logged(d, &[]);
        assert!(run.ok(), "{}", run.text());
        if took(&steps, "sync.fast") {
            assert!(took(&steps, "sync.fast.confirmed"), "{steps:?}");
            return;
        }
    }
    panic!("{}: no sync was taken for one with nothing to do", d.name);
}

#[track_caller]
fn same_trees(a: &Device, b: &Device, context: &str) {
    let diff = Picture::of(&a.root).diff(
        &Picture::of(&b.root),
        Ignore {
            owner: true,
            ..Ignore::default()
        },
    );
    assert_eq!(diff, Vec::<String>::new(), "{context}");
}

/// A modification time to the nanosecond, as `set_mtime` takes it.
fn mtime_of(path: &Path) -> (i64, i64) {
    let t = std::fs::symlink_metadata(path).unwrap().modified().unwrap();
    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap();
    (d.as_secs() as i64, i64::from(d.subsec_nanos()))
}

/// Something of every kind, so that each change below has something to change.
fn everything(a: &Device) {
    a.write("f1", "first file");
    a.write("d/f2", "second file");
    a.write("d/e/f3", "third file");
    std::fs::create_dir_all(a.path("d/sub")).unwrap();
    a.write("h1", "hard linked");
    std::fs::hard_link(a.path("h1"), a.path("h2")).unwrap();
    // Windows holds no symlinks.
    #[cfg(unix)]
    symlink("f1", a.path("l")).unwrap();
    for i in 0..12 {
        a.write(&format!("fill/{i}"), format!("{i}"));
    }
}

#[test]
fn a_sync_with_nothing_to_do_reads_no_file_and_agrees_with_the_full_one() {
    let (_sb, _s, a, b) = pair("steady-basic");
    everything(&a);
    a.write("big", vec![7u8; 3_000_000]);
    a.synced();
    b.synced();
    // The sync that pushed wrote what it pushed; the next agrees and writes nothing, and records so.
    let (run, steps) = logged(&a, &[]);
    assert!(run.ok() && !took(&steps, "sync.fast"), "{steps:?}");
    assert!(took(&steps, "sync.steady.recorded"), "{steps:?}");
    let (run, steps) = logged(&a, &[]);
    assert!(run.ok(), "{}", run.text());
    assert!(took(&steps, "sync.fast.confirmed"), "{steps:?}");
    let (run, steps) = logged(&a, &[RELEASE]);
    assert!(run.ok(), "{}", run.text());
    assert!(
        took(&steps, "sync.fast") && !took(&steps, "sync.round"),
        "{steps:?}"
    );
    assert!(!took(&steps, "scan.file.reading"), "{steps:?}");
    assert!(run.stdout.contains(" 0 steps applied here, 0 commits"));
    // And on the device that received it all, once what arrived has been read again.
    until_fast(&b);
}

/// The report's failure test: every kind of change, one at a time, after a sync that had nothing to
/// do. The fast path must leave each to the full sync, and each must reach the other device. Run as a
/// release build runs, with nothing checking the fast path but the outcome.
fn every_kind_of_change_is_seen_past_the_fast_path(shard: Shard) {
    type Make = fn(&Device, &Device);
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut changes: Vec<(&str, Make, bool)> = vec![
        (
            "an edit that keeps the size and the time",
            |a, _| {
                let (sec, nsec) = mtime_of(&a.path("f1"));
                a.write("f1", "FIRST FILE");
                set_mtime(&a.path("f1"), sec, nsec);
            },
            true,
        ),
        (
            "a folder's time",
            |a, _| set_mtime(&a.path("d"), 1_000_000, 5),
            true,
        ),
        ("a new file", |a, _| a.write("d/e/new", "new"), true),
        ("a new empty file", |a, _| a.write("d/e/empty", ""), true),
        (
            "a new folder",
            |a, _| std::fs::create_dir(a.path("d/new")).unwrap(),
            true,
        ),
        (
            "a removed file",
            |a, _| std::fs::remove_file(a.path("d/f2")).unwrap(),
            true,
        ),
        (
            "a removed folder",
            |a, _| std::fs::remove_dir(a.path("d/sub")).unwrap(),
            true,
        ),
        (
            "a rename",
            |a, _| std::fs::rename(a.path("f1"), a.path("f1-renamed")).unwrap(),
            true,
        ),
        (
            "a move",
            |a, _| std::fs::rename(a.path("d/e/f3"), a.path("f3")).unwrap(),
            true,
        ),
        (
            "a hard link more",
            |a, _| std::fs::hard_link(a.path("h1"), a.path("h3")).unwrap(),
            true,
        ),
        (
            "a hard link less",
            |a, _| std::fs::remove_file(a.path("h2")).unwrap(),
            true,
        ),
        (
            "a change on the server",
            |_, b| {
                b.write("from-b", "b");
                b.synced();
            },
            true,
        ),
        (
            "a file's time set to what it was",
            |a, _| {
                let (sec, nsec) = mtime_of(&a.path("f1"));
                set_mtime(&a.path("f1"), sec, nsec);
            },
            false,
        ),
        (
            "a rule",
            |a, _| std::fs::write(a.path(".cairn/ignore"), "d/sub/\n").unwrap(),
            false,
        ),
        (
            "the device's name",
            |a, _| {
                let path = a.path(".cairn/config.json");
                let mut config: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                config["device_name"] = "renamed".into();
                std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
            },
            false,
        ),
    ];
    // What Windows does not hold: modes beyond the write bits, xattrs, symlinks.
    #[cfg(unix)]
    changes.extend::<[(&str, Make, bool); 5]>([
        ("a mode", |a, _| chmod(&a.path("f1"), 0o600), true),
        (
            "an xattr",
            |a, _| set_xattr(&a.path("f1"), "user.cairn", b"x"),
            true,
        ),
        (
            "an xattr on a folder",
            |a, _| set_xattr(&a.path("d"), "user.cairn", b"x"),
            true,
        ),
        ("a folder's mode", |a, _| chmod(&a.path("d"), 0o700), true),
        (
            "a symlink made to point elsewhere",
            |a, _| {
                std::fs::remove_file(a.path("l")).unwrap();
                symlink("d/f2", a.path("l")).unwrap();
            },
            true,
        ),
    ]);
    for (what, make, travels) in shard.share(&changes) {
        let (_sb, _s, a, b) = pair("steady-changes");
        everything(&a);
        a.synced();
        b.synced();
        until_fast(&a);
        make(&a, &b);
        let (run, steps) = logged(&a, &[RELEASE]);
        assert!(run.ok(), "{what}: {}", run.text());
        assert!(
            !took(&steps, "sync.fast"),
            "{what}: taken for a sync with nothing to do"
        );
        // A probe reads no file: whatever it would read, the full sync reads, once.
        assert!(
            count(&steps, "scan.file.reading") <= 1,
            "{what}: read twice: {steps:?}"
        );
        if *travels {
            b.synced();
            a.synced();
            same_trees(&a, &b, what);
        }
    }
}

shards!(every_kind_of_change_is_seen_past_the_fast_path: s01 s02 s03 s04 s05 s06 s07 s08 s09 s10
    s11 s12 s13 s14 s15 s16);

/// What changes once the probe has passed it is the next sync's, as with any scan.
#[test]
fn an_edit_while_the_fast_path_looks_goes_up_with_the_next_sync() {
    let (sb, _s, a, b) = pair("steady-during");
    everything(&a);
    a.synced();
    b.synced();
    until_fast(&a);
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("scan.done:{}", gate.display());
    let root = a.clone();
    let running = std::thread::spawn(move || root.sync_env(&[("CAIRN_FP_PAUSE", &pause), RELEASE]));
    let start = Instant::now();
    while !gate.join("reached").exists() {
        assert!(start.elapsed() < SOON, "the scan never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
    a.write("d/f2", "edited while the probe was done with it");
    std::fs::write(gate.join("go"), b"").unwrap();
    running.join().unwrap().expect_ok();
    a.synced();
    b.synced();
    assert_eq!(b.read("d/f2"), b"edited while the probe was done with it");
}

/// The kill sweep, for the syncs this adds: the one that records what it agreed on, the fast one, and
/// one that the fast path leaves to the full sync. A kill at any of their steps changes nothing about
/// where things end up, and the syncs after it agree with the oracle.
fn a_kill_at_any_step_of_a_sync_with_nothing_to_do_changes_nothing(shard: Shard) {
    #[derive(Clone, Copy, Debug)]
    enum Phase {
        Records,
        Fast,
        Declines,
    }
    fn story(phase: Phase, kill: Option<usize>, log: Option<&Path>) -> (Picture, Picture) {
        let sb = Sandbox::fast("steady-sweep");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        everything(&a);
        a.synced();
        b.synced();
        match phase {
            Phase::Records => {}
            Phase::Fast => {
                a.synced();
            }
            Phase::Declines => {
                a.synced();
                a.write("d/e/f3", "edited");
                chmod(&a.path("f1"), 0o640);
            }
        }
        let step = kill.map(|n| n.to_string());
        let mut env = Vec::new();
        if let Some(n) = &step {
            env.push(("CAIRN_FP_CRASH", n.as_str()));
        }
        let path = log.map(|p| p.to_str().unwrap().to_string());
        if let Some(p) = &path {
            env.push(("CAIRN_FP_LOG", p.as_str()));
        }
        let run = a.sync_env(&env);
        assert!(run.ok() || run.was_killed(), "{phase:?}: {}", run.text());
        for _ in 0..3 {
            converge(&[&a, &b]);
        }
        (Picture::of(&a.root), Picture::of(&b.root))
    }
    // Each phase a sweep of its own, in as many parts as each of the others.
    let phases = [Phase::Records, Phase::Fast, Phase::Declines];
    let (i, part) = shard.within(phases.len());
    let phase = phases[i];
    let probe = Sandbox::fast("steady-sweep-probe");
    let log = probe.path("steps");
    let reference = story(phase, None, Some(&log));
    let steps = std::fs::read_to_string(&log).unwrap();
    match phase {
        Phase::Records => assert!(steps.contains("sync.steady.recorded"), "{steps}"),
        Phase::Fast => assert!(steps.contains("sync.fast.confirmed"), "{steps}"),
        Phase::Declines => assert!(!steps.contains("sync.fast\n"), "{steps}"),
    }
    let jobs: Vec<(usize, String)> = steps
        .lines()
        .enumerate()
        .filter(|(_, step)| !["scan.dir", "scan.file"].contains(step))
        .map(|(n, step)| (n + 1, format!("{phase:?}: step {} ({step})", n + 1)))
        .collect();
    let failures = each_item(part, jobs.len(), |at| {
        let (n, context) = &jobs[at];
        let (a, b) = story(phase, Some(*n), None);
        let ignore = Ignore {
            owner: true,
            all_mtime: true,
            ..Ignore::default()
        };
        assert_eq!(
            reference.0.diff(&a, ignore),
            Vec::<String>::new(),
            "{context}"
        );
        assert_eq!(
            reference.1.diff(&b, ignore),
            Vec::<String>::new(),
            "{context}"
        );
    });
    assert!(
        failures.is_empty(),
        "{} of this part's kills of {} changed the outcome; first: {}",
        failures.len(),
        jobs.len(),
        failures[0].1
    );
}

shards!(a_kill_at_any_step_of_a_sync_with_nothing_to_do_changes_nothing: s01 s02 s03 s04 s05 s06
    s07 s08 s09 s10 s11 s12 s13 s14 s15 s16 s17 s18 s19 s20 s21 s22 s23 s24);

// --- The daemon ---

// Through `sh`, for the limits a session starts it with, and with Linux's watcher in mind where it says so.
#[cfg(unix)]
struct Daemon {
    child: Child,
    root: PathBuf,
    log: PathBuf,
}

#[cfg(unix)]
impl Daemon {
    /// Through `sh`, for the limits a session would start it with.
    fn start(d: &Device, shell: &str, env: &[(&str, &str)]) -> Self {
        let log = d
            .root
            .with_extension(format!("daemon-{:08x}.log", rand::random::<u32>()));
        let out = std::fs::File::create(&log).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("{shell} exec \"$0\" \"$@\""))
            .arg(CAIRN)
            .arg("--root")
            .arg(&d.root)
            .args(["daemon", "--reconcile-secs", DAY, "--retry-secs", "1"])
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .stdin(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        Self {
            child: cmd.spawn().unwrap(),
            root: d.root.clone(),
            log,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn status(&self) -> serde_json::Value {
        std::fs::read(self.root.join(".cairn/daemon.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(serde_json::Value::Null)
    }

    fn syncs(&self) -> u64 {
        self.status()["syncs"].as_u64().unwrap_or(0)
    }

    #[track_caller]
    fn wait(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let start = Instant::now();
        while start.elapsed() < SOON {
            if done(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("waited {SOON:?} for {what}; the log: {}", self.log());
    }

    #[track_caller]
    fn wait_synced(&self) {
        self.wait("the daemon to be synced", |d| {
            d.syncs() >= 1 && d.status()["state"] == "synced"
        });
    }

    /// The syncs it runs over a few quiet seconds.
    fn syncs_while_quiet(&self, secs: u64) -> u64 {
        let before = self.syncs();
        std::thread::sleep(Duration::from_secs(secs));
        self.syncs() - before
    }
}

#[cfg(unix)]
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!("daemon log of {}:\n{}", self.root.display(), self.log());
        }
    }
}

/// A folder of thousands of subfolders, as real trees have, under a limit on open files far below that:
/// the walk held every subfolder waiting its turn open, and past the limit left the rest unwatched.
#[cfg(target_os = "linux")]
#[test]
fn a_folder_of_thousands_of_subfolders_is_watched_whole_under_a_low_limit() {
    let (_sb, _s, a, b) = pair("steady-wide");
    for i in 0..2000 {
        std::fs::create_dir_all(a.path(&format!("wide/s{i:04}"))).unwrap();
    }
    a.synced();
    // Soft and hard alike, so that raising the soft one cannot be what passes this.
    let da = Daemon::start(&a, "ulimit -Sn 256 && ulimit -Hn 256 &&", &[]);
    da.wait_synced();
    assert_eq!(da.status()["unwatched"], 0, "{}", da.log());
    assert!(
        da.status()["watched"].as_u64().unwrap() > 2000,
        "{}",
        da.log()
    );
    let syncs = da.syncs();
    a.write("wide/s1999/new", "in the last one");
    da.wait("the write in the last subfolder to wake it", |d| {
        d.syncs() > syncs && d.status()["state"] == "synced"
    });
    b.synced();
    assert_eq!(b.read("wide/s1999/new"), b"in the last one");
}

/// The soft limit a session starts a daemon with is no limit of the tree's.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_raises_its_soft_limit_on_open_files_to_the_hard_one() {
    let (_sb, _s, a, _b) = pair("steady-limit");
    a.synced();
    let da = Daemon::start(&a, "ulimit -Sn 256 &&", &[]);
    da.wait_synced();
    let limits = std::fs::read_to_string(format!("/proc/{}/limits", da.child.id())).unwrap();
    let line = limits
        .lines()
        .find(|l| l.starts_with("Max open files"))
        .unwrap();
    let numbers: Vec<&str> = line.split_whitespace().skip(3).take(2).collect();
    assert_eq!(numbers[0], numbers[1], "{line}");
    assert_ne!(numbers[0], "256", "{line}");
}

/// Whatever it cannot watch, it names, with why.
#[cfg(target_os = "linux")]
#[test]
fn a_folder_that_cannot_be_watched_is_named() {
    if skip_as_root() {
        return;
    }
    let (_sb, _s, a, _b) = pair("steady-unwatched");
    std::fs::create_dir_all(a.path("open/locked")).unwrap();
    a.write("open/f", "f");
    a.synced();
    chmod(&a.path("open/locked"), 0o000);
    let da = Daemon::start(&a, "", &[]);
    da.wait("it to say what it cannot watch", |d| {
        d.log().contains("folders could not be watched")
    });
    chmod(&a.path("open/locked"), 0o755);
    let log = da.log();
    assert!(log.contains("open/locked ("), "{log}");
}

/// What a device writes applying a change wakes nothing: the scan that confirmed it began after those
/// writes, and saw them. It used to sync once more for each change it received.
#[cfg(unix)]
#[test]
fn a_device_does_not_wake_itself_with_what_it_applied() {
    let (_sb, _s, a, b) = pair("steady-own-writes");
    everything(&a);
    a.synced();
    b.synced();
    let db = Daemon::start(&b, "", &[]);
    db.wait_synced();
    assert!(db.syncs_while_quiet(3) == 0, "{}", db.log());
    let before = db.syncs();
    a.write("d/e/arrives", "from a");
    a.write("d/e/f3", "edited on a");
    a.synced();
    db.wait("it to apply what a sent", |d| {
        d.syncs() > before && std::fs::read(b.path("d/e/arrives")).is_ok_and(|x| x == b"from a")
    });
    let after = db.syncs();
    assert_eq!(after - before, 1, "{}", db.log());
    assert_eq!(db.syncs_while_quiet(4), 0, "{}", db.log());
}

/// Its own commits, seen by the feed partway through the sync that made them, wake nothing either,
/// however far the head has gone on since: here another device's commit, which the same sync took in.
#[cfg(unix)]
#[test]
fn a_device_does_not_wake_itself_with_its_own_commits() {
    let (sb, _s, a, b) = pair("steady-own-commits");
    everything(&a);
    a.synced();
    b.synced();
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("sync.committed:{}", gate.display());
    let da = Daemon::start(&a, "", &[("CAIRN_FP_PAUSE", &pause)]);
    da.wait_synced();
    let before = da.syncs();
    a.write("from-a", "a");
    let start = Instant::now();
    while !gate.join("reached").exists() {
        assert!(start.elapsed() < SOON, "{}", da.log());
        std::thread::sleep(Duration::from_millis(10));
    }
    // Committed and held there, while the server goes on.
    b.write("from-b", "b");
    b.synced();
    std::thread::sleep(Duration::from_secs(1));
    std::fs::write(gate.join("go"), b"").unwrap();
    da.wait("both changes here", |d| {
        d.syncs() > before && a.path("from-b").exists()
    });
    assert_eq!(da.syncs() - before, 1, "{}", da.log());
    assert_eq!(da.syncs_while_quiet(4), 0, "{}", da.log());
}

/// The pause after a sync is twice what every sync repeats, the merge included, not twice the scan, and
/// 30 s at the most: here a merge held for 20 s, as one of a big tree takes, which would make it 40 s.
#[cfg(unix)]
#[test]
fn the_pause_after_a_sync_counts_its_merge_up_to_30_s() {
    let (sb, _s, a, _b) = pair("steady-pause");
    everything(&a);
    a.synced();
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("sync.identified:{}", gate.display());
    let da = Daemon::start(&a, "", &[("CAIRN_FP_PAUSE", &pause)]);
    let start = Instant::now();
    while !gate.join("reached").exists() {
        assert!(start.elapsed() < SOON, "{}", da.log());
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_secs(20));
    std::fs::write(gate.join("go"), b"").unwrap();
    da.wait_synced();
    a.write("next", "n");
    da.wait("the wait in the status", |d| {
        !d.status()["waiting"].is_null()
    });
    let waiting = da.status()["waiting"].clone();
    assert!(
        waiting["why"]
            .as_str()
            .unwrap()
            .contains("the pause after the last sync"),
        "{waiting}"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let left = waiting["until"].as_i64().unwrap() - now;
    assert!((20..=31).contains(&left), "{left} s left: {waiting}");
}

// --- The history check ---

/// Between a client and the server: every request line, and the bytes of every answer.
struct Recorder {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    answered: Arc<AtomicU64>,
}

impl Recorder {
    fn start(backend: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let backend = backend.strip_prefix("http://").unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let answered = Arc::new(AtomicU64::new(0));
        let (seen, bytes) = (requests.clone(), answered.clone());
        std::thread::spawn(move || {
            for mut client in listener.incoming().flatten() {
                let (backend, seen, bytes) = (backend.clone(), seen.clone(), bytes.clone());
                std::thread::spawn(move || {
                    let Ok(mut upstream) = TcpStream::connect(&backend) else {
                        return;
                    };
                    let mut back = upstream.try_clone().unwrap();
                    let mut front = client.try_clone().unwrap();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 65536];
                        while let Ok(n) = back.read(&mut buf) {
                            if n == 0 || front.write_all(&buf[..n]).is_err() {
                                break;
                            }
                            bytes.fetch_add(n as u64, Ordering::SeqCst);
                        }
                        let _ = front.shutdown(std::net::Shutdown::Both);
                    });
                    let mut buf = [0u8; 65536];
                    while let Ok(n) = client.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let text = String::from_utf8_lossy(&buf[..n]);
                        if text.starts_with("GET ") || text.starts_with("POST ") {
                            let line = text.lines().next().unwrap_or("").to_string();
                            seen.lock().unwrap().push(line);
                        }
                        if upstream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Self {
            url,
            requests,
            answered,
        }
    }

    fn take(&self) -> (Vec<String>, u64) {
        let lines = std::mem::take(&mut *self.requests.lock().unwrap());
        (lines, self.answered.swap(0, Ordering::SeqCst))
    }
}

/// The check that the server still has this device's history asks for the entry at the cursor without
/// its operations: after a commit of thousands, it downloaded all of them on every sync.
#[test]
fn a_sync_with_nothing_to_do_does_not_download_the_last_commit() {
    let sb = Sandbox::on_disk("steady-history");
    let server = Server::start(sb.path("server"));
    let proxy = Recorder::start(&server.url);
    let root = sb.path("a");
    std::fs::create_dir_all(&root).unwrap();
    let token = server.token_file.to_str().unwrap().to_string();
    run(
        &[
            "init",
            "--server",
            &proxy.url,
            "--device-name",
            "a",
            "--token-file",
            &token,
            root.to_str().unwrap(),
        ],
        &[],
    )
    .expect_ok();
    let a = Device {
        root,
        name: "a".into(),
    };
    for i in 0..400 {
        a.write(
            &format!("many/file-{i:03}-with-a-long-enough-name"),
            format!("{i}"),
        );
    }
    a.synced();
    a.synced();
    for env in [vec![], vec![RELEASE]] {
        proxy.take();
        let run = a.sync_env(&env);
        assert!(run.ok(), "{}", run.text());
        let (lines, bytes) = proxy.take();
        // The cursor is at the one commit: the old check asked for everything after the one before.
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("changes?after=0&limit=1000")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("limit=1&ops=false")),
            "{lines:?}"
        );
        assert!(bytes < 16_000, "{bytes} bytes answered: {lines:?}");
    }
}

/// A leftover of a killed run is the full sync's to remove, so the fast path does not pass over it.
#[test]
fn a_leftover_temporary_file_is_left_to_the_full_sync() {
    let (_sb, _s, a, b) = pair("steady-leftover");
    everything(&a);
    a.synced();
    b.synced();
    until_fast(&a);
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(a.path(".cairn/config.json")).unwrap()).unwrap();
    let leftover = format!(
        "d/.cairn-tmp-{}-leftover",
        config["device"].as_str().unwrap()
    );
    a.write(&leftover, "half a download");
    let (run, steps) = logged(&a, &[RELEASE]);
    assert!(run.ok(), "{}", run.text());
    assert!(!took(&steps, "sync.fast"), "{steps:?}");
    assert!(!a.exists(&leftover), "the leftover stayed");
}

/// A change made once the scan that agreed has passed its folder wakes the daemon all the same.
#[cfg(unix)]
#[test]
fn a_change_after_the_scan_that_agreed_still_wakes_the_daemon() {
    let (sb, _s, a, b) = pair("steady-after-scan");
    everything(&a);
    a.synced();
    b.synced();
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("sync.scanned:{}", gate.display());
    let da = Daemon::start(&a, "", &[("CAIRN_FP_PAUSE", &pause)]);
    let start = Instant::now();
    while !gate.join("reached").exists() {
        assert!(start.elapsed() < SOON, "{}", da.log());
        std::thread::sleep(Duration::from_millis(10));
    }
    a.write("d/e/after-the-scan", "late");
    std::fs::write(gate.join("go"), b"").unwrap();
    da.wait("the late change to go up", |_| {
        b.synced();
        b.exists("d/e/after-the-scan")
    });
}

/// A change to the state alone, with the tree, the server and the rules as they were: here `cairn
/// include` of a folder excluded and removed here, which has to come down. The generation tells.
#[test]
fn a_folder_included_again_comes_down_past_the_fast_path() {
    let (_sb, _s, a, b) = pair("steady-include");
    everything(&a);
    a.synced();
    b.synced();
    b.cairn(&["exclude", "--remove-local", "d"]).expect_ok();
    assert!(!b.exists("d/f2"));
    until_fast(&b);
    b.cairn_env(&["include", "d"], &[RELEASE]).expect_ok();
    assert_eq!(b.read("d/e/f3"), b"third file");
    same_trees(&a, &b, "included again");
}

/// `cairn exclude` syncs at once, and that sync is the full one: the state changed, the tree did not.
/// Once it has, the fast path works with what is left out by identity too.
#[test]
fn a_folder_excluded_is_left_out_by_the_full_sync_and_then_the_fast_one() {
    let (_sb, _s, a, b) = pair("steady-exclude");
    everything(&a);
    a.synced();
    b.synced();
    until_fast(&a);
    let log = a.root.with_extension("exclude-steps");
    let run = a.cairn_env(
        &["exclude", "d"],
        &[RELEASE, ("CAIRN_FP_LOG", log.to_str().unwrap())],
    );
    assert!(run.ok(), "{}", run.text());
    let steps = std::fs::read_to_string(&log).unwrap();
    assert!(!steps.lines().any(|l| l == "sync.fast"), "{steps}");
    until_fast(&a);
    // Left out here, so an edit to it stays here, also past the fast path.
    a.write("d/e/f3", "edited where it is left out");
    let (run, steps) = logged(&a, &[RELEASE]);
    assert!(run.ok(), "{}", run.text());
    assert!(took(&steps, "sync.fast"), "{steps:?}");
    b.synced();
    assert_eq!(b.read("d/e/f3"), b"third file");
    // Another folder at its name is left out too, by the name, and the full sync notes its inode, so
    // that is no sync with nothing to do.
    until_fast(&a);
    std::fs::remove_dir_all(a.path("d")).unwrap();
    a.write("d/new", "made again where it is left out");
    let (run, steps) = logged(&a, &[]);
    assert!(run.ok(), "{}", run.text());
    assert!(!took(&steps, "sync.fast"), "{steps:?}");
    until_fast(&a);
    b.synced();
    assert!(!b.exists("d/new"));
}
