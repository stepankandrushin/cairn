//! A daemon's syncs list the folders its watcher marked, and what each round must see besides, and take
//! the rest of the tree from the last agreement (engine scope.rs): so a steady sync costs what changed,
//! not a walk of the tree. And each way that narrowing could miss something does not: an overflow of the
//! event queue, a folder that is not watched, a watcher that has stopped, an event the kernel never sent,
//! a change while a round walks. A lost event only delays a change: the reconcile lists the whole tree
//! within the hour, and the daily check finds it too. Every sync here runs under the incremental oracle,
//! which decides each round the full way on the same scan as well.

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const DAY: &str = "86400";
const SOON: Duration = Duration::from_secs(60);

struct Daemon {
    child: Child,
    root: PathBuf,
    log: PathBuf,
}

impl Daemon {
    fn start(d: &Device, args: &[&str], env: &[(&str, &str)]) -> Self {
        let log = d
            .root
            .with_extension(format!("daemon-{:08x}.log", rand::random::<u32>()));
        let out = std::fs::File::create(&log).unwrap();
        let mut cmd = Command::new(CAIRN);
        cmd.arg("--root").arg(&d.root).arg("daemon");
        cmd.args(["--retry-secs", "1"]);
        cmd.args(args).stdout(out.try_clone().unwrap()).stderr(out);
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

    fn syncs(&self) -> u64 {
        std::fs::read(self.root.join(".cairn/daemon.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["syncs"].as_u64())
            .unwrap_or(0)
    }

    /// Until it has been resting for a while.
    #[track_caller]
    fn wait_rest(&self) {
        let start = Instant::now();
        let mut last = self.syncs();
        let mut since = Instant::now();
        while since.elapsed() < Duration::from_millis(2500) || last == 0 {
            std::thread::sleep(Duration::from_millis(50));
            let now = self.syncs();
            if now != last {
                last = now;
                since = Instant::now();
            }
            assert!(start.elapsed() < SOON, "{}", self.log());
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!("daemon log of {}:\n{}", self.root.display(), self.log());
        }
    }
}

#[track_caller]
fn wait_for(what: &str, d: &Daemon, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < SOON {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("waited {SOON:?} for {what}; the log: {}", d.log());
}

#[track_caller]
fn wait_file(d: &Daemon, path: &Path, want: &[u8]) {
    wait_for(&format!("{} to hold {want:?}", path.display()), d, || {
        std::fs::read(path).is_ok_and(|have| have == want)
    });
}

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

/// A tree of folders of files, synced to both devices.
fn tree(a: &Device, b: &Device, folders: usize, files: usize) {
    for d in 0..folders {
        for f in 0..files {
            a.write(&format!("d{d}/e/f{f}"), format!("{d} {f}"));
        }
    }
    a.write("top", "top");
    for d in [a, b, a, b] {
        d.synced();
    }
}

fn steps(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn count(steps: &[String], step: &str) -> usize {
    steps.iter().filter(|s| *s == step).count()
}

/// What the daemon's steps say since the mark: the steps file read from where it ended then.
struct Steps {
    path: PathBuf,
    from: usize,
}

impl Steps {
    fn new(path: PathBuf) -> Self {
        Self { path, from: 0 }
    }

    fn mark(&mut self) {
        self.from = steps(&self.path).len();
    }

    fn since(&self) -> Vec<String> {
        steps(&self.path).split_off(self.from)
    }
}

/// The note's measure: one file edited in a tree of thousands, and the daemon's sync lists the folders
/// above it and its own, a few, not the thousands. Then the same tree with the scope out of the picture
/// (the reconcile, every sync), for the count it saves.
#[test]
fn one_edit_in_a_big_tree_lists_a_few_folders_not_the_tree() {
    let (sb, _s, a, b) = pair("scope-cost");
    tree(&a, &b, 60, 100);
    let log = sb.path("a-steps");
    let da = Daemon::start(
        &a,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_LOG", log.to_str().unwrap())],
    );
    da.wait_rest();
    let mut s = Steps::new(log.clone());
    s.mark();
    a.write("d42/e/f7", "edited");
    let db = Daemon::start(&b, &["--reconcile-secs", DAY], &[]);
    wait_file(&db, &b.path("d42/e/f7"), b"edited");
    da.wait_rest();
    let got = s.since();
    let listed = count(&got, "scan.dir");
    let through = count(&got, "scan.dir.through");
    let rounds = count(&got, "sync.round");
    eprintln!("one edit: {rounds} round(s), {listed} folders listed, {through} gone through");
    assert!(
        count(&got, "sync.scoped") >= 1,
        "no round had a scope: {got:?}"
    );
    assert!(
        listed <= 3 * rounds,
        "{listed} folders listed in {rounds} round(s) for one edit, of 121: {}",
        da.log()
    );
    assert!(count(&got, "scan.file.reading") <= rounds, "{got:?}");
    // The edit went up whole, and nothing else moved.
    assert!(da.log().contains("synced to sequence"), "{}", da.log());
    drop(da);
    drop(db);

    // The whole tree, for the record: a one-shot sync lists every folder.
    let log2 = sb.path("a-steps-whole");
    a.write("d43/e/f7", "edited too");
    a.sync_env(&[("CAIRN_FP_LOG", log2.to_str().unwrap())])
        .expect_ok();
    let got = steps(&log2);
    let whole = count(&got, "scan.dir") / count(&got, "scan.done").max(1);
    eprintln!("a scan of the whole tree lists {whole} folders");
    assert!(whole >= 121, "{whole}");
}

/// A folder renamed, one made deep down, a file moved between folders, a hard link made in another
/// folder (which tells the file's own folder nothing), a symlink, a folder's mode, a file's xattr and
/// mode, deletes of a file and of a folder full of them: each reaches the other device by the daemon's
/// scoped syncs alone, and the trees end the same.
///
/// Then the same with nothing listed ahead of what the watcher marked: what the journal, the state's
/// marks and this sync's own writes name is found by the rounds' widening alone, as nothing a round
/// loads may be the ledger's word alone.
#[cfg(unix)]
#[test]
fn every_kind_of_change_reaches_the_other_device_through_scoped_syncs() {
    every_kind_through_scoped_syncs("scope-kinds", &[]);
}

#[cfg(unix)]
#[test]
fn every_kind_of_change_reaches_the_other_device_by_widening_alone() {
    every_kind_through_scoped_syncs(
        "scope-kinds-widening",
        &[("CAIRN_FP_UNSUPPORTED", "sync.scope.ahead")],
    );
}

#[cfg(unix)]
fn every_kind_through_scoped_syncs(label: &str, env: &[(&str, &str)]) {
    let (sb, _s, a, b) = pair(label);
    tree(&a, &b, 6, 5);
    let log = sb.path("a-steps");
    let mut env_a = env.to_vec();
    env_a.push(("CAIRN_FP_LOG", log.to_str().unwrap()));
    let da = Daemon::start(&a, &["--reconcile-secs", DAY], &env_a);
    let db = Daemon::start(&b, &["--reconcile-secs", DAY], env);
    da.wait_rest();
    db.wait_rest();
    let mut s = Steps::new(log);
    s.mark();
    let diff = || {
        Picture::of(&a.root).diff(
            &Picture::of(&b.root),
            Ignore {
                owner: true,
                ..Ignore::default()
            },
        )
    };
    let settle = |what: &str| {
        // A rest is no sync for a while, not the end of one: under load a sync of the tree, and the
        // pause after it, outlast it. So the trees are compared until they agree, and only a
        // difference that lasts fails. A walk that a sync's write moved a file under is taken again.
        let start = Instant::now();
        loop {
            da.wait_rest();
            db.wait_rest();
            let agree =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| diff().is_empty()));
            if agree.unwrap_or(false) || start.elapsed() > SOON {
                break;
            }
        }
        // An oracle that found a round deciding otherwise than the full sync fails the sync, which the
        // daemon says and tries again whole: the trees would end the same, and the log would not.
        for d in [&da, &db] {
            assert!(!d.log().contains("error:"), "after {what}: {}", d.log());
        }
        assert_eq!(diff(), Vec::<String>::new(), "after {what}");
    };
    std::fs::rename(a.path("d0"), a.path("d0-renamed")).unwrap();
    settle("a folder renamed");
    std::fs::create_dir_all(a.path("d1/e/x/y/z")).unwrap();
    a.write("d1/e/x/y/z/deep", "deep");
    settle("a folder made deep down");
    std::fs::rename(a.path("d2/e/f1"), a.path("d3/e/f1-moved")).unwrap();
    settle("a file moved between folders");
    std::fs::hard_link(a.path("d4/e/f2"), a.path("d5/e/linked")).unwrap();
    settle("a hard link in another folder");
    std::os::unix::fs::symlink("e/f3", a.path("d4/link")).unwrap();
    settle("a symlink");
    chmod(&a.path("d3/e"), 0o750);
    settle("a folder's mode");
    set_xattr(&a.path("d5/e/f4"), "user.scope", b"tag");
    chmod(&a.path("d5/e/f0"), 0o600);
    settle("a file's xattr and mode");
    std::fs::remove_file(a.path("d1/e/f0")).unwrap();
    std::fs::remove_dir_all(a.path("d2")).unwrap();
    settle("deletes");
    // From b's side, into folders a has not touched: each arrives at a through scoped syncs.
    b.write("d4/e/f1", "edited on b");
    settle("an edit on the other device");
    chmod(&b.path("d5"), 0o700);
    b.write("d5/e/new-on-b", "new on b");
    settle("a mode and a new file on the other device");
    std::fs::remove_file(b.path("d4/e/f0")).unwrap();
    std::fs::create_dir_all(b.path("top-from-b/x")).unwrap();
    b.write("top-from-b/x/y", "a folder made on b");
    settle("a delete and a new folder on the other device");
    // A folder renamed with a change inside it as it goes.
    std::fs::rename(b.path("d3"), b.path("d3-from-b")).unwrap();
    b.write("d3-from-b/e/f2", "changed with the rename");
    settle("a folder renamed with an edit in it, on the other device");
    let got = s.since();
    eprintln!(
        "{label}: {} scoped rounds, {} widened, {} rounds",
        count(&got, "sync.scoped"),
        count(&got, "sync.scope.widened"),
        count(&got, "sync.round")
    );
    assert!(count(&got, "sync.scoped") >= 8, "{got:?}");
}

/// What was edited while the queue overflowed, in a folder nothing else marks, is found: an overflow
/// is a loss of track, and the next sync lists the whole tree.
#[cfg(target_os = "linux")]
#[test]
fn an_edit_lost_in_an_overflow_is_found_by_the_next_sync() {
    let max: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(16384);
    if max > 100_000 {
        skip(&format!("the queue here takes {max} events"));
        return;
    }
    let (sb, _s, a, b) = pair("scope-overflow");
    tree(&a, &b, 3, 3);
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("daemon.watch.read:{}", gate.display());
    let log = sb.path("a-steps");
    let da = Daemon::start(
        &a,
        &["--reconcile-secs", DAY, "--no-feed"],
        &[
            ("CAIRN_FP_PAUSE", &spec),
            ("CAIRN_FP_LOG", log.to_str().unwrap()),
        ],
    );
    da.wait_rest();
    // Wakes the reader, which stops at the gate.
    a.write("first", "wakes the reader, which stops");
    wait_for("the reader to stop", &da, || gate.join("reached").exists());
    // Enough events to overflow the queue, from two files written in turn, so that the kernel cannot
    // fold them into one and the round after has little else to do: then an edit in a folder nothing
    // else marks, whose events the full queue drops.
    for i in 0..max {
        std::fs::write(a.path(&format!("churn{}", i % 2)), format!("{i}")).unwrap();
    }
    a.write("d1/e/f1", "edited as the queue overflowed");
    std::fs::write(gate.join("go"), "").unwrap();
    let db = Daemon::start(&b, &["--reconcile-secs", DAY], &[]);
    wait_file(&db, &b.path("d1/e/f1"), b"edited as the queue overflowed");
    assert!(steps(&log).iter().any(|s| s == "daemon.watch.lost"));
}

/// A folder the watcher could not watch, as past the kernel's limit: an edit there wakes nothing, but
/// the next sync, woken by anything, lists the whole tree and sends it.
#[cfg(target_os = "linux")]
#[test]
fn an_edit_in_a_folder_not_watched_goes_with_the_next_sync() {
    let (_sb, _s, a, b) = pair("scope-unwatched");
    tree(&a, &b, 3, 3);
    a.write("dark/f", "first");
    a.synced();
    b.synced();
    let da = Daemon::start(
        &a,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_UNWATCHED", "dark")],
    );
    da.wait_rest();
    assert!(da.log().contains("could not be watched"), "{}", da.log());
    let db = Daemon::start(&b, &["--reconcile-secs", DAY], &[]);
    a.write("dark/f", "edited where nothing watches");
    std::thread::sleep(Duration::from_millis(500));
    a.write("d0/e/f0", "wakes the daemon");
    wait_file(&db, &b.path("d0/e/f0"), b"wakes the daemon");
    wait_file(&db, &b.path("dark/f"), b"edited where nothing watches");
}

/// The watcher stops reading, as one whose descriptor failed: every sync after it lists the whole tree,
/// so an edit with no event, which a remote change wakes the daemon for, goes too.
#[cfg(target_os = "linux")]
#[test]
fn after_the_watcher_stops_every_sync_lists_the_whole_tree() {
    let (_sb, _s, a, b) = pair("scope-died");
    tree(&a, &b, 3, 3);
    let da = Daemon::start(
        &a,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_UNSUPPORTED", "daemon.watch.reader")],
    );
    let db = Daemon::start(&b, &["--reconcile-secs", DAY], &[]);
    da.wait_rest();
    db.wait_rest();
    // The reader's first read is its last.
    a.write("d0/e/f0", "the last event read");
    wait_for("the watcher to stop", &da, || {
        da.log().contains("the watcher has stopped")
    });
    a.write("d1/e/f1", "no event for this");
    std::thread::sleep(Duration::from_millis(300));
    b.write("from-b", "wakes a by the feed");
    wait_file(&da, &a.path("from-b"), b"wakes a by the feed");
    wait_file(&db, &b.path("d1/e/f1"), b"no event for this");
}

/// An event the kernel never sent, as for a write through a mapping, here by a failpoint: syncs with a
/// scope, woken by edits elsewhere, do not see it, and the daily check, which lists the whole tree, does.
/// Then the same with the reconcile.
#[cfg(target_os = "linux")]
#[test]
fn an_edit_whose_event_was_lost_waits_for_the_check_or_the_reconcile() {
    for by in ["check", "reconcile"] {
        let (_sb, _s, a, b) = pair(&format!("scope-lost-{by}"));
        tree(&a, &b, 3, 3);
        a.write("d2/secret", "first");
        a.synced();
        b.synced();
        let args: &[&str] = match by {
            "check" => &["--reconcile-secs", DAY, "--full-sync-secs", "20"],
            _ => &["--reconcile-secs", "20", "--full-sync-secs", DAY],
        };
        let started = Instant::now();
        let da = Daemon::start(&a, args, &[("CAIRN_FP_LOST_EVENT", "secret")]);
        let db = Daemon::start(&b, &["--reconcile-secs", DAY], &[]);
        da.wait_rest();
        db.wait_rest();
        a.write("d2/secret", "edited, and no event came");
        std::thread::sleep(Duration::from_millis(300));
        // An edit elsewhere wakes a scoped sync, which does not list d2.
        a.write("d0/e/f0", "an edit elsewhere");
        wait_file(&db, &b.path("d0/e/f0"), b"an edit elsewhere");
        da.wait_rest();
        db.wait_rest();
        assert!(
            started.elapsed() < Duration::from_secs(18),
            "too slow to tell the check from a sync before it"
        );
        assert!(
            da.log().contains("listed "),
            "the sync of the edit elsewhere had no scope: {}",
            da.log()
        );
        assert_eq!(
            b.read("d2/secret"),
            b"first",
            "a scoped sync saw what no event said: {}",
            da.log()
        );
        // The check is due at the first sync 20 seconds after the last; the reconcile wakes by itself.
        if by == "check" {
            while started.elapsed() < Duration::from_secs(21) {
                std::thread::sleep(Duration::from_millis(100));
            }
            a.write("d1/e/f1", "wakes the sync the check is due at");
        }
        wait_file(&db, &b.path("d2/secret"), b"edited, and no event came");
        if by == "check" {
            wait_for("the check", &da, || {
                da.log().contains("checked the ledger against the state")
            });
        }
    }
}

/// A root named through the data volume, `/System/Volumes/Data/Users/...`, which `canonicalize` leaves
/// so, gets its events by the path the system volume shows it at, `/Users/...`. They are under the root
/// all the same: an edit is a scoped sync of a few folders, not a loss of track and a walk of the tree.
#[cfg(target_os = "macos")]
#[test]
fn a_root_named_through_the_data_volume_has_scoped_syncs() {
    let (sb, _s, a, b) = pair("scope-data-volume");
    tree(&a, &b, 20, 5);
    let real = std::fs::canonicalize(&a.root).unwrap();
    let mut long = a.clone();
    long.root = Path::new("/System/Volumes/Data").join(real.strip_prefix("/").unwrap());
    assert_eq!(
        std::fs::canonicalize(&long.root).unwrap(),
        long.root,
        "the data volume's name is the case here only if canonicalize keeps it"
    );
    let log = sb.path("a-steps");
    let da = Daemon::start(
        &long,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_LOG", log.to_str().unwrap())],
    );
    da.wait_rest();
    let mut s = Steps::new(log);
    s.mark();
    a.write("d7/e/f3", "edited");
    let db = Daemon::start(&b, &["--reconcile-secs", DAY], &[]);
    wait_file(&db, &b.path("d7/e/f3"), b"edited");
    da.wait_rest();
    let got = s.since();
    let listed = count(&got, "scan.dir");
    let rounds = count(&got, "sync.round");
    assert!(
        count(&got, "sync.scoped") >= 1,
        "no round had a scope: {got:?}"
    );
    assert!(
        listed <= 3 * rounds,
        "{listed} folders listed in {rounds} round(s) for one edit, of 41: {}",
        da.log()
    );
}

/// An edit after a scoped round's scan, in a folder the scope did not list, is not taken for agreed: its
/// event is stamped after that scan began, so the next sync lists its folder.
#[cfg(unix)]
#[test]
fn an_edit_as_a_scoped_round_walks_goes_with_the_next_sync() {
    let (sb, _s, a, b) = pair("scope-during");
    tree(&a, &b, 4, 3);
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    // The daemon's first sync lists the whole tree. The sync of the edit below has two scoped rounds:
    // the first pushes it, the second reads the journal as far as that commit and agrees. The pause is
    // in the second, which the agreement rests on, once its scope is taken and before it walks.
    let spec = format!("sync.scoped#2:{}", gate.display());
    let da = Daemon::start(&a, &["--reconcile-secs", DAY], &[("CAIRN_FP_PAUSE", &spec)]);
    let db = Daemon::start(&b, &["--reconcile-secs", DAY], &[]);
    da.wait_rest();
    a.write("d0/e/f0", "wakes a scoped sync");
    wait_for("the scoped round", &da, || gate.join("reached").exists());
    a.write("d3/e/f2", "edited as the scoped round began");
    std::fs::write(gate.join("go"), "").unwrap();
    wait_file(&db, &b.path("d0/e/f0"), b"wakes a scoped sync");
    wait_file(&db, &b.path("d3/e/f2"), b"edited as the scoped round began");
}
