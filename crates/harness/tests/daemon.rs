//! `cairn daemon` as a real process: what it is for, and each way the things it relies on can fail.
//! Wherever the point is that the watcher or the feed did it, the reconcile is set to a day, so that the
//! safety net cannot be what passes the test.

mod common;
use common::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DAY: &str = "86400";
/// Long enough for a loaded machine, far shorter than any reconcile in these tests.
const SOON: Duration = Duration::from_secs(30);

struct Daemon {
    child: Child,
    root: PathBuf,
    log: PathBuf,
}

impl Daemon {
    fn start(d: &Device, args: &[&str]) -> Self {
        Self::start_env(d, args, &[])
    }

    fn start_env(d: &Device, args: &[&str], env: &[(&str, &str)]) -> Self {
        let log = d
            .root
            .with_extension(format!("daemon-{:08x}.log", rand::random::<u32>()));
        let out = std::fs::File::create(&log).unwrap();
        let mut cmd = Command::new(CAIRN);
        cmd.arg("--root").arg(&d.root).arg("daemon");
        if !args.contains(&"--retry-secs") {
            cmd.args(["--retry-secs", "1"]);
        }
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

    /// With the reconcile out of the picture.
    fn events_only(d: &Device) -> Self {
        Self::start(d, &["--reconcile-secs", DAY])
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

    /// Until it has been resting for a while. A file that has just arrived is still part of a sync,
    /// and an edit in the same instant is a real conflict: nothing can say the editor saw it.
    #[track_caller]
    fn wait_rest(&self) {
        let start = Instant::now();
        let mut last = self.syncs();
        let mut since = Instant::now();
        while since.elapsed() < Duration::from_millis(2500) {
            std::thread::sleep(Duration::from_millis(50));
            let now = self.syncs();
            if now != last {
                last = now;
                since = Instant::now();
            }
            assert!(start.elapsed() < SOON, "{}", self.log());
        }
    }

    fn state(&self) -> String {
        self.status()["state"].as_str().unwrap_or("").to_string()
    }

    #[track_caller]
    fn wait_state(&self, state: &str) {
        let log = self.log.clone();
        wait_for(&format!("the daemon to be {state}"), &log, || {
            self.state() == state
        });
    }

    /// Exited by itself, as a failpoint makes it.
    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// `kill -9`.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
        if std::thread::panicking() {
            eprintln!("daemon log of {}:\n{}", self.root.display(), self.log());
        }
    }
}

#[track_caller]
fn wait_for(what: &str, log: &Path, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < SOON {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "waited {SOON:?} for {what}; the log: {}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
}

#[track_caller]
fn wait_file(d: &Daemon, path: PathBuf, want: &[u8]) {
    let log = d.log.clone();
    wait_for(
        &format!("{} to hold {want:?}", path.display()),
        &log,
        || std::fs::read(&path).is_ok_and(|have| have == want),
    );
}

#[track_caller]
fn wait_gone(d: &Daemon, path: PathBuf) {
    let log = d.log.clone();
    wait_for(&format!("{} to go", path.display()), &log, || {
        std::fs::symlink_metadata(&path).is_err()
    });
}

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

/// One-shot `cairn sync` beside a running daemon, which may hold the lock for a moment.
#[track_caller]
fn sync_when_free(d: &Device) -> Run {
    let start = Instant::now();
    loop {
        let run = d.sync();
        if run.ok() || !run.stderr.contains("already working") || start.elapsed() > SOON {
            return run.expect_ok();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn every_kind_of_change_reaches_the_other_device_by_itself() {
    let (_sb, _s, a, b) = pair("d-all");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");

    a.write("new.txt", "one");
    wait_file(&db, b.path("new.txt"), b"one");
    db.wait_rest();
    b.write("new.txt", "two");
    wait_file(&da, a.path("new.txt"), b"two");
    da.wait_rest();

    // A new folder written at once: its contents come before any watch on it can.
    std::fs::create_dir_all(a.path("x/y/z")).unwrap();
    a.write("x/y/z/deep", "deep");
    wait_file(&db, b.path("x/y/z/deep"), b"deep");
    // And it is watched from then on.
    a.write("x/y/z/deeper", "deeper");
    wait_file(&db, b.path("x/y/z/deeper"), b"deeper");

    // A folder moved in from outside the root brings its subfolders along.
    let outside = a.root.with_extension("outside");
    std::fs::create_dir_all(outside.join("m/n")).unwrap();
    std::fs::write(outside.join("m/n/f"), "moved in").unwrap();
    std::fs::rename(&outside, a.path("came")).unwrap();
    wait_file(&db, b.path("came/m/n/f"), b"moved in");
    a.write("came/m/n/g", "after");
    wait_file(&db, b.path("came/m/n/g"), b"after");

    // A rename, then a change under the new name: the watches followed it.
    std::fs::rename(a.path("x"), a.path("renamed")).unwrap();
    wait_file(&db, b.path("renamed/y/z/deep"), b"deep");
    a.write("renamed/y/z/third", "third");
    wait_file(&db, b.path("renamed/y/z/third"), b"third");
    // A folder made under the new name is found by the new path, and watched too.
    std::fs::create_dir(a.path("renamed/y/z/newer")).unwrap();
    wait_file(&db, b.path("renamed/y/z/third"), b"third");
    da.wait_rest();
    a.write("renamed/y/z/newer/f", "in a new folder under a renamed one");
    wait_file(
        &db,
        b.path("renamed/y/z/newer/f"),
        b"in a new folder under a renamed one",
    );
    // The same when what was renamed is one level above whatever says so.
    std::fs::rename(a.path("renamed/y"), a.path("renamed/w")).unwrap();
    wait_file(&db, b.path("renamed/w/z/third"), b"third");
    da.wait_rest();
    std::fs::create_dir(a.path("renamed/w/z/newest")).unwrap();
    da.wait_rest();
    a.write("renamed/w/z/newest/f", "and again");
    wait_file(&db, b.path("renamed/w/z/newest/f"), b"and again");

    // Metadata alone.
    chmod(&a.path("new.txt"), 0o640);
    let log = db.log.clone();
    wait_for("the mode to arrive", &log, || {
        std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(b.path("new.txt")).unwrap().permissions(),
        ) & 0o777
            == 0o640
    });

    // A delete.
    db.wait_rest();
    std::fs::remove_file(b.path("new.txt")).unwrap();
    wait_gone(&da, a.path("new.txt"));
    std::fs::remove_dir_all(a.path("came")).unwrap();
    wait_gone(&db, b.path("came"));
}

/// The acceptance scenario, with the daemons doing it: one machine at a time.
#[test]
fn a_git_working_tree_stays_clean_with_daemons_running() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git is not installed: skipped");
        return;
    }
    let (_sb, _s, a, b) = pair("d-git");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    let git = |root: &Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&a.root, &["init", "-q"]);
    a.write("src/main.sh", "#!/bin/sh\necho hello\n");
    chmod(&a.path("src/main.sh"), 0o755);
    std::os::unix::fs::symlink("src/main.sh", a.path("run")).unwrap();
    a.write("node_modules/dep/index.js", "module.exports = 1;\n");
    a.write(".venv/bin/python", "#!/fake\n");
    a.write(".gitignore", ".cairn\nnode_modules\n.venv\n");
    git(&a.root, &["add", "."]);
    git(&a.root, &["commit", "-qm", "first"]);

    let log = db.log.clone();
    let log_has = |root: &Path, lines: usize| {
        Command::new("git")
            .current_dir(root)
            .args(["log", "--format=%s"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).lines().count() == lines)
    };
    wait_for("the other device's log to read", &log, || {
        log_has(&b.root, 1)
    });
    db.wait_rest();
    da.wait_rest();
    assert_eq!(git(&b.root, &["status", "--porcelain"]), "");
    assert!(b.exists("node_modules/dep/index.js") && b.exists(".venv/bin/python"));

    b.write("src/main.sh", "#!/bin/sh\necho changed\n");
    git(&b.root, &["commit", "-qam", "second"]);
    let log = da.log.clone();
    wait_for("the second commit to arrive", &log, || log_has(&a.root, 2));
    da.wait_rest();
    assert_eq!(git(&a.root, &["status", "--porcelain"]), "");
    let copies: Vec<String> = Picture::of(&a.root)
        .names()
        .into_iter()
        .filter(|n| n.contains("(conflict"))
        .collect();
    assert!(copies.is_empty(), "no conflict copies: {copies:?}");
}

/// Its own writes, its own state and reading files must not wake it, or it would never rest.
#[test]
fn an_idle_daemon_stays_idle() {
    let (_sb, _s, a, b) = pair("d-idle");
    a.write("file", "body");
    let db = Daemon::events_only(&b);
    let da = Daemon::events_only(&a);
    wait_file(&db, b.path("file"), b"body");
    std::thread::sleep(Duration::from_secs(4));
    let (before_a, before_b) = (da.syncs(), db.syncs());
    assert!(before_a > 0 && before_b > 0);
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(da.syncs(), before_a, "a kept syncing:\n{}", da.log());
    assert_eq!(db.syncs(), before_b, "b kept syncing:\n{}", db.log());
}

/// Every event lost: the reconcile alone gets a change out. And without it, nothing does, which is what
/// shows the watcher really is off in this test.
#[test]
fn a_change_with_every_event_lost_is_found_by_the_reconcile() {
    let (_sb, _s, a, b) = pair("d-lost");
    let lost = Daemon::start(&a, &["--no-watch", "--reconcile-secs", DAY]);
    lost.wait_state("synced");
    a.write("unseen", "1");
    std::thread::sleep(Duration::from_secs(3));
    sync_when_free(&b);
    assert!(
        !b.exists("unseen"),
        "with no watcher and no reconcile, who sent it?"
    );
    drop(lost);

    let net = Daemon::start(&a, &["--no-watch", "--reconcile-secs", "1"]);
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("unseen"), b"1");
    a.write("second", "2");
    wait_file(&db, b.path("second"), b"2");
    drop(net);
}

#[test]
fn a_change_on_the_server_with_no_feed_is_found_by_the_reconcile() {
    let (_sb, _s, a, b) = pair("d-nofeed");
    let off = Daemon::start(&a, &["--no-feed", "--reconcile-secs", DAY]);
    off.wait_state("synced");
    b.write("far", "1");
    b.synced();
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !a.exists("far"),
        "with no feed and no reconcile, who brought it?"
    );
    drop(off);

    let net = Daemon::start(&a, &["--no-feed", "--reconcile-secs", "1"]);
    wait_file(&net, a.path("far"), b"1");
}

/// A real overflow of the kernel's queue. Files written after it filled have no event at all, and
/// folders made after it have no watch: both have to come out of the overflow itself.
#[test]
fn an_overflow_of_the_event_queue_loses_nothing() {
    let max: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(16384);
    if max > 100_000 {
        eprintln!("the queue here takes {max} events: skipped");
        return;
    }
    let (sb, _s, a, b) = pair("d-overflow");
    // There first, so that it is watched while the reader is stopped.
    std::fs::create_dir(a.path("many")).unwrap();
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("daemon.watch.read:{}", gate.display());
    let hits = sb.path("hits");
    let da = Daemon::start_env(
        &a,
        &["--reconcile-secs", DAY, "--no-feed"],
        &[
            ("CAIRN_FP_PAUSE", &spec),
            ("CAIRN_FP_LOG", hits.to_str().unwrap()),
        ],
    );
    da.wait_state("synced");
    a.write("first", "wakes the reader, which stops at the gate");
    let log = da.log.clone();
    wait_for("the reader to stop", &log, || gate.join("reached").exists());

    // A file makes three events, so this overflows the queue well before the end.
    let files = max / 2;
    for i in 0..files {
        std::fs::write(a.path(&format!("many/f{i}")), "x").unwrap();
    }
    std::fs::create_dir(a.path("after-overflow")).unwrap();
    a.write("after-overflow/inner", "inner");
    std::fs::write(gate.join("go"), "").unwrap();

    // Downloading all of those is not the point, and takes a while on a real disk.
    std::fs::write(b.path(".cairn/ignore"), "many/\n").unwrap();
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("after-overflow/inner"), b"inner");
    let log = da.log.clone();
    wait_for("the overflow to be read", &log, || {
        std::fs::read_to_string(&hits).is_ok_and(|h| h.contains("daemon.watch.overflow"))
    });
    // A folder that the queue never told anyone about is watched now.
    a.write("after-overflow/later", "later");
    wait_file(&db, b.path("after-overflow/later"), b"later");
    // And all of it went up.
    let log = da.log.clone();
    wait_for("nothing to be left to do", &log, || {
        a.cairn(&["status"]).stdout.contains("nothing to do")
    });
}

/// An edit after the scan and before the sync ends, which that sync cannot see.
#[test]
fn a_change_during_a_sync_gets_a_sync_of_its_own() {
    let (sb, _s, a, b) = pair("d-during");
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("sync.scanned:{}", gate.display());
    let da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &[("CAIRN_FP_PAUSE", &spec)]);
    let log = da.log.clone();
    wait_for("the scan to end", &log, || gate.join("reached").exists());
    a.write("written-after-the-scan", "x");
    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(gate.join("go"), "").unwrap();
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("written-after-the-scan"), b"x");
}

/// The sync has returned and looks at nothing again: only the wake it kept can send this change on.
#[test]
fn a_change_as_a_sync_ends_gets_a_sync_of_its_own() {
    let (sb, _s, a, b) = pair("d-as-ends");
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("daemon.synced:{}", gate.display());
    let da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &[("CAIRN_FP_PAUSE", &spec)]);
    let log = da.log.clone();
    wait_for("the sync to end", &log, || gate.join("reached").exists());
    a.write("written-as-it-ended", "x");
    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(gate.join("go"), "").unwrap();
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("written-as-it-ended"), b"x");
}

/// A `kill -9` at step after step of the daemon's life, and a new one taking over each time.
#[test]
fn a_daemon_killed_at_any_step_loses_nothing() {
    let (sb, mut server, a, b) = pair("d-kill");
    let mut wanted = Vec::new();
    let mut killed = 0;
    for step in (1..120).step_by(3) {
        let name = format!("f{step}");
        let body = token::make(step, 300);
        a.write(&name, &body);
        wanted.push((name, body));
        let mut d = Daemon::start_env(
            &a,
            &["--reconcile-secs", DAY],
            &[("CAIRN_FP_CRASH", &step.to_string())],
        );
        let log = d.log.clone();
        // It dies at its step, or it finishes with room to spare.
        wait_for("the daemon to die or settle", &log, || {
            d.exited() || d.state() == "synced"
        });
        killed += u32::from(d.exited());
        drop(d);
    }
    assert!(killed > 5, "only {killed} kills happened");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    for (name, body) in &wanted {
        wait_file(&db, b.path(name), body);
    }
    da.wait_state("synced");
    assert!(leftover_temps(&a.root).is_empty());
    drop((da, db));
    let report = server.fsck();
    assert!(report.contains("clean"), "{report}");
    drop(sb);
}

/// The server goes away, with long polls in flight and edits waiting, and comes back.
#[test]
fn a_server_going_away_and_coming_back_loses_nothing_and_needs_no_reconcile() {
    let (_sb, mut server, a, b) = pair("d-server");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");
    std::thread::sleep(Duration::from_millis(300));

    server.kill();
    a.write("during", "written while the server was down");
    let log = da.log.clone();
    wait_for("the daemon to say so", &log, || da.state() == "error");
    assert!(da.log().contains("cannot reach"), "{}", da.log());
    assert!(a.exists("during"));

    server.restart();
    wait_file(&db, b.path("during"), b"written while the server was down");
    da.wait_state("synced");
    assert!(da.status()["feed_down"].is_null(), "{}", da.status());
    b.write("after", "after");
    wait_file(&da, a.path("after"), b"after");
}

/// A server restored from a backup, behind what this device has seen, is found at once: a waiting
/// request must answer, not sit there because its head is not ahead.
#[test]
fn a_server_behind_this_device_is_noticed_without_a_reconcile() {
    let (sb, mut server, a, _b) = pair("d-behind");
    a.write("first", "1");
    a.synced();
    let backup = sb.path("backup");
    server.kill();
    sh(
        "cp",
        &[
            "-a",
            server.data.to_str().unwrap(),
            backup.to_str().unwrap(),
        ],
    );
    server.restart();
    a.write("second", "2");
    a.synced();

    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    std::thread::sleep(Duration::from_secs(2));
    server.kill();
    sh("rm", &["-rf", server.data.to_str().unwrap()]);
    sh(
        "cp",
        &[
            "-a",
            backup.to_str().unwrap(),
            server.data.to_str().unwrap(),
        ],
    );
    server.restart();
    let log = da.log.clone();
    wait_for("the daemon to pause", &log, || da.log().contains("paused"));
    assert!(
        da.log().contains("no longer has the history"),
        "{}",
        da.log()
    );
    assert_eq!(a.read("second"), b"2");
}

/// With no feed to say the server is back, retrying is what finds out.
#[test]
fn a_failure_is_retried_with_nothing_else_to_wake_the_daemon() {
    let (_sb, mut server, a, b) = pair("d-retry");
    let da = Daemon::start(&a, &["--no-feed", "--reconcile-secs", DAY]);
    da.wait_state("synced");
    da.wait_rest();
    server.kill();
    a.write("during", "d");
    da.wait_state("error");
    server.restart();
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("during"), b"d");
}

/// A long sync is mostly moving data, which the next one does not repeat: the pause after it is twice
/// its scan, not twice its length. It was twice its length, and after a 159 s download a device did
/// not react for 318 s.
#[test]
fn a_long_sync_holds_the_next_change_back_only_for_its_scan() {
    let (sb, _s, a, b) = pair("d-gap");
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    // The first sync is held after its scan for 6 s, as one moving a lot of data would be.
    let pause = format!("sync.scanned:{}", gate.display());
    let da = Daemon::start_env(
        &a,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_PAUSE", &pause)],
    );
    let log = da.log.clone();
    wait_for("the first sync to be held", &log, || {
        gate.join("reached").exists()
    });
    std::thread::sleep(Duration::from_secs(6));
    std::fs::write(gate.join("go"), b"").unwrap();
    da.wait_state("synced");
    b.write("note", "x");
    b.synced();
    let start = Instant::now();
    wait_file(&da, a.path("note"), b"x");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}: {}",
        start.elapsed(),
        da.log()
    );
}

/// A change that waits says so in `cairn status`, with until when and why.
#[test]
fn status_says_when_changes_wait_and_why() {
    let (_sb, mut server, a, b) = pair("d-waiting");
    let da = Daemon::start(
        &a,
        &["--no-feed", "--reconcile-secs", DAY, "--retry-secs", "30"],
    );
    da.wait_state("synced");
    da.wait_rest();
    server.kill();
    a.write("first", "1");
    da.wait_state("error");
    // Each failure doubles the wait: 1, 2, 4, then 8 s.
    let failed = da.syncs();
    let log = da.log.clone();
    wait_for("three more failures", &log, || da.syncs() >= failed + 3);
    a.write("second", "2");
    wait_for("the wait in the status", &log, || {
        !da.status()["waiting"].is_null()
    });
    let waiting = &da.status()["waiting"];
    assert!(
        waiting["why"].as_str().unwrap().contains("after a failure"),
        "{waiting}"
    );
    let status = a.cairn(&["status"]);
    assert!(
        status.stdout.contains("daemon: changes wait until")
            && status.stdout.contains("after a failure"),
        "{}",
        status.text()
    );
    server.restart();
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("second"), b"2");
}

/// What the filesystem here refuses, such as a mode on a file another user owns, stays in the status
/// until it is put right or gone. It was said once in the log and then only the checker's comparison
/// showed it.
#[test]
fn metadata_refused_here_stays_in_the_status() {
    let (_sb, _s, a, b) = pair("d-not-applied");
    let da = Daemon::events_only(&a);
    let db = Daemon::start_env(
        &b,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_EPERM", "meta.mode")],
    );
    da.wait_state("synced");
    db.wait_state("synced");
    let held = |path: &str| {
        db.status()["not_applied"]
            .as_array()
            .is_some_and(|v| v.iter().any(|n| n["path"] == path))
    };
    a.write("script", "x");
    chmod(&a.path("script"), 0o755);
    wait_file(&db, b.path("script"), b"x");
    let log = db.log.clone();
    wait_for("the refusal in the status", &log, || held("script"));
    // Still there after syncs that have nothing to say about it.
    a.write("other", "y");
    wait_file(&db, b.path("other"), b"y");
    db.wait_rest();
    assert!(held("script"), "{}", db.status());
    let status = b.cairn(&["status"]);
    assert!(
        status
            .stdout
            .lines()
            .any(|l| l.starts_with("daemon: not applied here since")
                && l.contains("script: ")
                && l.contains("mode")),
        "{}",
        status.text()
    );
    // Gone with the file.
    std::fs::remove_file(a.path("script")).unwrap();
    wait_gone(&db, b.path("script"));
    wait_for("the refusal to go", &log, || !held("script"));
}

/// With nothing to sync and nothing to retry, the feed is the only thing that can notice.
#[test]
fn an_idle_daemon_says_when_the_server_goes_away() {
    let (_sb, mut server, a, _b) = pair("d-idle-down");
    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    da.wait_rest();
    server.kill();
    let log = da.log.clone();
    wait_for("the daemon to say so", &log, || {
        da.log().contains("change feed is down") && !da.status()["feed_down"].is_null()
    });
}

/// A folder moved out of the root is nothing to this root any more, whatever happens in it.
#[test]
fn a_folder_moved_out_of_the_root_stops_waking_the_daemon() {
    let (sb, _s, a, _b) = pair("d-moved-out");
    a.write("leaving/inner/f", "x");
    a.write("stays", "so that this is not most of the tree vanishing");
    a.write("stays2", "x");
    a.write("stays3", "x");
    a.write("stays4", "x");
    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    da.wait_rest();
    let outside = sb.path("outside");
    std::fs::rename(a.path("leaving"), &outside).unwrap();
    da.wait_rest();
    let before = da.syncs();
    for i in 0..5 {
        std::fs::write(outside.join(format!("inner/g{i}")), "y").unwrap();
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(da.syncs(), before, "{}", da.log());
}

/// Whoever reads the daemon's output going away must not stop the syncing.
#[test]
fn the_daemon_survives_its_output_going_away() {
    let (_sb, _s, a, b) = pair("d-pipe");
    let mut child = Command::new(CAIRN)
        .arg("--root")
        .arg(&a.root)
        .args(["daemon", "--reconcile-secs", DAY])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    drop(child.stdout.take());
    a.write("after", "the reader went away");
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("after"), b"the reader went away");
    assert!(matches!(child.try_wait(), Ok(None)), "the daemon died");
    let _ = child.kill();
    let _ = child.wait();
}

/// Written through a symlink, a status report would go wherever a swapped `.cairn` points.
#[test]
fn the_status_report_does_not_follow_a_symlinked_state_directory() {
    let (sb, _s, a, _b) = pair("d-state-link");
    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    da.wait_rest();
    let held = sb.path("state-held");
    let target = sb.path("elsewhere");
    std::fs::create_dir(&target).unwrap();
    std::fs::rename(a.path(".cairn"), &held).unwrap();
    std::os::unix::fs::symlink(&target, a.path(".cairn")).unwrap();
    a.write("wake", "up");
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        std::fs::read_dir(&target).unwrap().next().is_none(),
        "something was written through the symlink"
    );
}

/// The rules taken away by moving the file: nothing else says so.
#[test]
fn moving_the_rules_away_takes_effect_too() {
    let (sb, _s, a, b) = pair("d-rules-moved");
    std::fs::write(a.path(".cairn/ignore"), "held\n").unwrap();
    a.write("held", "h");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    da.wait_rest();
    std::fs::rename(a.path(".cairn/ignore"), sb.path("rules-elsewhere")).unwrap();
    wait_file(&db, b.path("held"), b"h");
}

/// A waiting request must not hold a stopping server up for its whole wait.
#[test]
fn a_server_stops_at_once_with_requests_waiting() {
    let (_sb, mut server, a, b) = pair("d-stop");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");
    // Past the moment both have gone back to waiting.
    std::thread::sleep(Duration::from_secs(2));
    let took = server.stop_gently();
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// The root marker gone, as when a mount has gone: it stops, says why, and carries on when it is back.
#[test]
fn a_root_that_goes_away_pauses_deletes_nothing_and_resumes() {
    let (sb, _s, a, b) = pair("d-gone");
    for i in 0..20 {
        a.write(&format!("f{i}"), "x");
    }
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("f19"), b"x");
    da.wait_rest();
    db.wait_rest();

    // With everything at rest, so that only the watcher can be what notices.
    let held = sb.path("held-away");
    std::fs::rename(&a.root, &held).unwrap();
    std::fs::create_dir(&a.root).unwrap();
    // Only its output can say so: it has nowhere of its own to write here, and must not make one.
    let log = da.log.clone();
    wait_for("the daemon to pause", &log, || {
        da.log().contains("paused") && da.log().contains("not mounted")
    });
    // It must not have made anything in the empty folder, such as a state directory.
    assert!(std::fs::read_dir(&a.root).unwrap().next().is_none());
    std::thread::sleep(Duration::from_secs(2));
    assert!(b.exists("f19"), "the other device lost it");

    std::fs::remove_dir(&a.root).unwrap();
    std::fs::rename(&held, &a.root).unwrap();
    da.wait_state("synced");
    assert!(a.exists("f19"));
    // The watches are on the root as it is now.
    a.write("back", "back");
    wait_file(&db, b.path("back"), b"back");
}

/// Most of a tree vanishing is not a delete until a person says so, however the daemon came to see it.
#[test]
fn most_of_a_tree_vanishing_pauses_the_daemon_and_a_person_can_lift_it() {
    let (_sb, _s, a, b) = pair("d-mass");
    for i in 0..30 {
        a.write(&format!("data/f{i}"), "x");
    }
    a.write("kept", "k");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("data/f29"), b"x");
    da.wait_state("synced");

    std::fs::remove_dir_all(a.path("data")).unwrap();
    da.wait_state("paused");
    assert!(da.log().contains("vanished"), "{}", da.log());
    std::thread::sleep(Duration::from_secs(2));
    assert!(b.exists("data/f29"), "a mass delete went through");

    let start = Instant::now();
    loop {
        let run = a.cairn(&["sync", "--allow-mass-delete"]);
        if run.ok() || start.elapsed() > SOON {
            run.expect_ok();
            break;
        }
    }
    wait_gone(&db, b.path("data"));
    da.wait_state("synced");
}

#[test]
fn a_second_daemon_on_a_root_is_refused_and_one_shot_commands_still_work() {
    let (_sb, _s, a, _b) = pair("d-second");
    let first = Daemon::events_only(&a);
    first.wait_state("synced");
    let mut second = Daemon::events_only(&a);
    let log = second.log.clone();
    wait_for("the second daemon to give up", &log, || second.exited());
    assert!(second.log().contains("already running"), "{}", second.log());
    assert!(!first.log().contains("error"), "{}", first.log());

    a.write("by-hand", "x");
    let run = sync_when_free(&a);
    assert!(run.stdout.contains("synced at sequence"), "{}", run.text());
    let status = a.cairn(&["status"]);
    assert!(
        status.stdout.contains("daemon: running"),
        "{}",
        status.text()
    );
    drop(first);
    let status = a.cairn(&["status"]);
    assert!(
        status.stdout.contains("daemon: not running"),
        "{}",
        status.text()
    );
}

// --- Rules, with a daemon ---

#[test]
fn rules_take_effect_as_soon_as_they_are_saved() {
    let (_sb, _s, a, b) = pair("d-rules");
    std::fs::write(a.path(".cairn/ignore"), "held-back\nheld-dir/\n").unwrap();
    a.write("held-back", "1");
    a.write("held-dir/inner", "2");
    a.write("plain", "3");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("plain"), b"3");
    assert!(!b.exists("held-back") && !b.exists("held-dir"));

    // The rules change: what they let go goes up, watched from then on.
    std::fs::write(a.path(".cairn/ignore"), "").unwrap();
    wait_file(&db, b.path("held-dir/inner"), b"2");
    wait_file(&db, b.path("held-back"), b"1");
    a.write("held-dir/more", "4");
    wait_file(&db, b.path("held-dir/more"), b"4");
    da.wait_state("synced");

    // And the other way, written as editors do: aside, then renamed over.
    std::fs::write(a.path(".cairn/ignore.tmp"), "later\n").unwrap();
    std::fs::rename(a.path(".cairn/ignore.tmp"), a.path(".cairn/ignore")).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    a.write("later", "secret");
    a.write("marker", "m");
    wait_file(&db, b.path("marker"), b"m");
    assert!(!b.exists("later"));
}

/// A log file the rules leave out, written all the time, must not keep a daemon scanning.
#[test]
fn a_busy_file_that_is_ignored_does_not_wake_the_daemon() {
    let (_sb, _s, a, _b) = pair("d-busy");
    std::fs::write(a.path(".cairn/ignore"), "*.log\n").unwrap();
    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    std::thread::sleep(Duration::from_secs(2));
    let before = da.syncs();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (path, stop) = (a.path("busy.log"), stop.clone());
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::SeqCst) {
                i += 1;
                std::fs::write(&path, i.to_string()).unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    std::thread::sleep(Duration::from_secs(4));
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    assert_eq!(da.syncs(), before, "{}", da.log());
}

/// A program writing one file for a long time, a download say, holds back nothing else, however long
/// it takes, and its file arrives whole once it is closed.
#[test]
fn a_file_being_written_holds_back_nothing_else() {
    use std::io::Write;
    let (_sb, _s, a, b) = pair("d-writing");
    let da = Daemon::start(&a, &["--reconcile-secs", DAY, "--retry-secs", "60"]);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (path, stop) = (a.path("download.part"), stop.clone());
        std::thread::spawn(move || {
            let mut f = std::fs::File::create(&path).unwrap();
            let mut written = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                let piece = vec![b'x'; 1024];
                f.write_all(&piece).unwrap();
                written.extend_from_slice(&piece);
                std::thread::sleep(Duration::from_millis(50));
            }
            f.write_all(b"end").unwrap();
            written.extend_from_slice(b"end");
            written
        })
    };
    // Long enough for a backoff to have grown to 16 seconds, if anything were backing off.
    std::thread::sleep(Duration::from_secs(20));
    for i in 0..3 {
        // Apart, so that no one sync takes them all.
        std::thread::sleep(Duration::from_secs(2));
        let name = format!("note-{i}");
        a.write(&name, "x");
        let start = Instant::now();
        wait_file(&db, b.path(&name), b"x");
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{name} took {:?}: {}",
            start.elapsed(),
            da.log()
        );
    }
    #[cfg(target_os = "linux")]
    assert!(!b.exists("download.part"), "a part of it went over");
    stop.store(true, Ordering::SeqCst);
    let whole = writer.join().unwrap();
    wait_file(&db, b.path("download.part"), &whole);
}

/// Rules that cannot be used are said out loud, and nothing goes up until they are fixed.
#[test]
fn a_daemon_with_bad_rules_says_so_and_uploads_nothing() {
    let (_sb, _s, a, b) = pair("d-badrules");
    std::fs::write(a.path(".cairn/ignore"), "[z-a\n").unwrap();
    a.write("secret", "s");
    let da = Daemon::events_only(&a);
    da.wait_state("error");
    assert!(da.log().contains(".cairn/ignore"), "{}", da.log());
    sync_when_free(&b);
    assert!(!b.exists("secret"));
    std::fs::write(a.path(".cairn/ignore"), "secret\n").unwrap();
    a.write("plain", "p");
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("plain"), b"p");
    assert!(!b.exists("secret"));
}

// --- Through a reverse proxy ---

/// Between a device and the server, as a reverse proxy is: answers `504` when the server has said
/// nothing for `cut` while a request is waiting, and counts requests.
struct Proxy {
    url: String,
    cuts: Arc<AtomicU64>,
}

impl Proxy {
    fn start(backend: &str, cut: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let backend = backend.strip_prefix("http://").unwrap().to_string();
        let cuts = Arc::new(AtomicU64::new(0));
        let count = cuts.clone();
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let (backend, count) = (backend.clone(), count.clone());
                std::thread::spawn(move || {
                    if let Ok(upstream) = TcpStream::connect(&backend) {
                        relay(client, upstream, cut, count);
                    }
                });
            }
        });
        Self { url, cuts }
    }
}

fn relay(client: TcpStream, upstream: TcpStream, cut: Duration, count: Arc<AtomicU64>) {
    let waiting_since = Arc::new(std::sync::Mutex::new(None::<Instant>));
    {
        let (mut from, mut to, waiting) = (
            client.try_clone().unwrap(),
            upstream.try_clone().unwrap(),
            waiting_since.clone(),
        );
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = from.read(&mut buf) {
                if n == 0 || to.write_all(&buf[..n]).is_err() {
                    break;
                }
                *waiting.lock().unwrap() = Some(Instant::now());
            }
            let _ = to.shutdown(std::net::Shutdown::Both);
        });
    }
    let mut from = upstream.try_clone().unwrap();
    let mut to = client.try_clone().unwrap();
    from.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut buf = [0u8; 65536];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                *waiting_since.lock().unwrap() = None;
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                let since = *waiting_since.lock().unwrap();
                if since.is_some_and(|t| t.elapsed() > cut) {
                    count.fetch_add(1, Ordering::SeqCst);
                    let body = "upstream timed out";
                    let _ = write!(
                        to,
                        "HTTP/1.1 504 Gateway Time-out\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Both);
    let _ = upstream.shutdown(std::net::Shutdown::Both);
}

fn device_through(sb: &Sandbox, server: &Server, url: &str, name: &str) -> Device {
    let root = sb.path(name);
    std::fs::create_dir_all(&root).unwrap();
    let token = server.token_file.to_str().unwrap().to_string();
    let r = root.to_str().unwrap().to_string();
    run(
        &[
            "init",
            "--server",
            url,
            "--device-name",
            name,
            "--token-file",
            &token,
            &r,
        ],
        &[],
    )
    .expect_ok();
    Device {
        root,
        name: name.into(),
    }
}

/// A proxy with a read timeout much shorter than the client's wait. A push must still get through,
/// with no help from the reconcile, and the daemon must not think anything is badly wrong for long.
#[test]
fn the_feed_gets_through_a_proxy_that_cuts_long_requests() {
    let sb = Sandbox::on_disk("d-proxy");
    let server = Server::start(sb.path("server"));
    let proxy = Proxy::start(&server.url, Duration::from_secs(2));
    let a = device_through(&sb, &server, &proxy.url, "a");
    let b = device_through(&sb, &server, &proxy.url, "b");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");
    // Long enough for the feed to have been cut and to have adapted.
    std::thread::sleep(Duration::from_secs(8));
    for i in 0..3 {
        let body = format!("through the proxy {i}");
        a.write("pushed", &body);
        wait_file(&db, b.path("pushed"), body.as_bytes());
    }
    // The feed has settled under the proxy's limit: it is not being cut any more.
    let cuts = proxy.cuts.load(Ordering::SeqCst);
    assert!(
        cuts > 0,
        "the proxy never cut anything, so this tested nothing"
    );
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(proxy.cuts.load(Ordering::SeqCst), cuts, "{}", db.log());
}

/// Something answering at once with no news, as an old server does, or a proxy with a very short idea
/// of waiting: no busy loop.
#[test]
fn a_feed_that_is_never_held_is_not_a_busy_loop() {
    let (_sb, server, a, b) = pair("d-nowait");
    // Answers everything itself but `wait`, which it strips.
    let proxy = StrippingProxy::start(&server.url);
    let config = a.path(".cairn/config.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    value["server"] = serde_json::Value::String(proxy.url.clone());
    std::fs::write(&config, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    std::thread::sleep(Duration::from_secs(6));
    let asked = proxy.requests.load(Ordering::SeqCst);
    assert!(asked < 60, "{asked} requests in six seconds");

    // It is still polling, so changes still arrive.
    b.write("far", "f");
    b.synced();
    wait_file(&da, a.path("far"), b"f");
}

struct StrippingProxy {
    url: String,
    requests: Arc<AtomicU64>,
}

impl StrippingProxy {
    fn start(backend: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let backend = backend.strip_prefix("http://").unwrap().to_string();
        let requests = Arc::new(AtomicU64::new(0));
        let count = requests.clone();
        std::thread::spawn(move || {
            for mut client in listener.incoming().flatten() {
                let (backend, count) = (backend.clone(), count.clone());
                std::thread::spawn(move || {
                    let Ok(mut upstream) = TcpStream::connect(&backend) else {
                        return;
                    };
                    let mut back = upstream.try_clone().unwrap();
                    let mut front = client.try_clone().unwrap();
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut back, &mut front);
                        let _ = front.shutdown(std::net::Shutdown::Both);
                    });
                    let mut buf = [0u8; 65536];
                    while let Ok(n) = client.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let text = String::from_utf8_lossy(&buf[..n]);
                        let out: Vec<u8> =
                            if buf[..n].starts_with(b"GET ") && text.contains("wait=") {
                                count.fetch_add(1, Ordering::SeqCst);
                                text.replace("&wait=", "&ignored=").into_bytes()
                            } else {
                                buf[..n].to_vec()
                            };
                        if upstream.write_all(&out).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Self { url, requests }
    }
}

// --- With real mounts ---

/// Run one `inner_` test below in a private user and mount namespace.
fn in_namespace(test: &str) {
    let probe = Command::new("unshare").args(["-Urm", "true"]).output();
    if !probe.is_ok_and(|o| o.status.success()) {
        eprintln!("no unprivileged namespaces here: skipped");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new("unshare")
        .args(["-Urm", "sh", "-c"])
        .arg(format!(
            "exec '{}' --exact {test} --ignored --nocapture --test-threads=1",
            exe.display()
        ))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[track_caller]
fn sh(program: &str, args: &[&str]) {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn device_at(server: &Server, root: &Path, name: &str) -> Device {
    let token = server.token_file.to_str().unwrap().to_string();
    let r = root.to_str().unwrap().to_string();
    run(
        &[
            "init",
            "--server",
            &server.url,
            "--device-name",
            name,
            "--token-file",
            &token,
            &r,
        ],
        &[],
    )
    .expect_ok();
    Device {
        root: root.to_path_buf(),
        name: name.into(),
    }
}

/// Taking a bind mount away leaves its filesystem alive, so inotify says nothing at all: this is a case
/// only the reconcile can find. The daemon must not hold the mount busy either: a tool that keeps a
/// disk from being unmounted gets killed by the person who wanted to.
#[test]
fn a_bind_mount_going_away_is_found_by_the_reconcile_and_coming_back_resumes() {
    in_namespace("inner_bind_mount");
}

#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_bind_mount() {
    let sb = Sandbox::on_disk("d-bind");
    let server = Server::start(sb.path("server"));
    let b = Device::new(&sb, &server, "b");
    let (disk, mount_point) = (sb.path("disk"), sb.path("mnt"));
    std::fs::create_dir_all(&disk).unwrap();
    std::fs::create_dir_all(&mount_point).unwrap();
    let bind = || {
        sh(
            "mount",
            &[
                "--bind",
                disk.to_str().unwrap(),
                mount_point.to_str().unwrap(),
            ],
        )
    };
    bind();
    let a = device_at(&server, &mount_point, "a");
    for i in 0..20 {
        a.write(&format!("f{i}"), "x");
    }
    let da = Daemon::start(&a, &["--reconcile-secs", "1"]);
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("f19"), b"x");
    da.wait_state("synced");

    // Fails with "busy" if the daemon holds anything open in there. A sync in progress does, for its
    // moment, as any program using the disk would.
    let log = da.log.clone();
    wait_for("the mount to come off", &log, || {
        Command::new("umount")
            .arg(&mount_point)
            .output()
            .unwrap()
            .status
            .success()
    });
    wait_for("the daemon to pause", &log, || da.log().contains("paused"));
    std::thread::sleep(Duration::from_secs(2));
    assert!(b.exists("f19"), "a missing mount was taken for a delete");
    assert!(
        std::fs::read_dir(&mount_point).unwrap().next().is_none(),
        "something was written to the bare mount point"
    );

    // Edited on the disk while it was away from here, then mounted again.
    std::fs::write(disk.join("while-away"), "w").unwrap();
    bind();
    wait_file(&db, b.path("while-away"), b"w");
    a.write("after-remount", "r");
    wait_file(&db, b.path("after-remount"), b"r");
    assert!(b.exists("f19"));
    drop((da, db));
    sh("umount", &[mount_point.to_str().unwrap()]);
}

/// A filesystem really unmounted: the event says so at once, with the reconcile far off. Then a disk
/// mounted with the same files on it: nothing announces that, and the watches have to follow.
#[test]
fn a_filesystem_unmounted_pauses_at_once_and_a_remount_resumes_with_watches() {
    in_namespace("inner_real_unmount");
}

#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_real_unmount() {
    let sb = Sandbox::on_disk("d-umount");
    let server = Server::start(sb.path("server"));
    let b = Device::new(&sb, &server, "b");
    let mount_point = sb.path("mnt");
    std::fs::create_dir_all(&mount_point).unwrap();
    sh(
        "mount",
        &["-t", "tmpfs", "tmpfs", mount_point.to_str().unwrap()],
    );
    let a = device_at(&server, &mount_point, "a");
    for i in 0..20 {
        a.write(&format!("f{i}"), "x");
    }
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("f19"), b"x");
    da.wait_state("synced");

    // What the disk holds, to bring back: a tmpfs does not keep it through an unmount.
    let saved = sb.path("saved");
    sh(
        "cp",
        &["-a", mount_point.to_str().unwrap(), saved.to_str().unwrap()],
    );
    let log = da.log.clone();
    wait_for("the mount to come off", &log, || {
        Command::new("umount")
            .arg(&mount_point)
            .output()
            .unwrap()
            .status
            .success()
    });
    wait_for("the daemon to pause", &log, || da.log().contains("paused"));
    std::thread::sleep(Duration::from_secs(2));
    assert!(b.exists("f19"), "a missing mount was taken for a delete");

    // All at once, as a disk arrives: with no moment of a half-copied root for a sync to trip over.
    let staging = sb.path("staging");
    std::fs::create_dir(&staging).unwrap();
    sh(
        "mount",
        &["-t", "tmpfs", "tmpfs", staging.to_str().unwrap()],
    );
    let contents = format!("{}/.", saved.display());
    sh("cp", &["-a", &contents, staging.to_str().unwrap()]);
    sh(
        "mount",
        &[
            "--move",
            staging.to_str().unwrap(),
            mount_point.to_str().unwrap(),
        ],
    );
    let log = da.log.clone();
    wait_for("the daemon to resume", &log, || da.state() == "synced");
    da.wait_rest();
    a.write("after-remount", "r");
    wait_file(&db, b.path("after-remount"), b"r");
    assert!(b.exists("f19"));
    drop((da, db));
    sh("umount", &[mount_point.to_str().unwrap()]);
}
