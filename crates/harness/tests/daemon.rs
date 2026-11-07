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

    // Metadata alone: on Windows, all a file holds of a mode is its write bits.
    let mode = if cfg!(windows) { 0o444 } else { 0o640 };
    chmod(&a.path("new.txt"), mode);
    let log = db.log.clone();
    wait_for("the mode to arrive", &log, || {
        mode_of(&b.path("new.txt")) == mode
    });

    // A delete.
    db.wait_rest();
    remove_file(&b.path("new.txt"));
    wait_gone(&da, a.path("new.txt"));
    std::fs::remove_dir_all(a.path("came")).unwrap();
    wait_gone(&db, b.path("came"));
}

/// The acceptance scenario, with the daemons doing it: one machine at a time.
#[test]
fn a_git_working_tree_stays_clean_with_daemons_running() {
    if Command::new("git").arg("--version").output().is_err() {
        skip("git is not installed");
        return;
    }
    let (_sb, _s, a, b) = pair("d-git");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    let git = |root: &Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
            // Git for Windows turns line ends into CRLF on checkout by default.
            .args(["-c", "core.autocrlf=false"])
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
    // What Windows does not hold, git there does not record either.
    #[cfg(unix)]
    {
        chmod(&a.path("src/main.sh"), 0o755);
        std::os::unix::fs::symlink("src/main.sh", a.path("run")).unwrap();
    }
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

/// The check on the syncs that take what changed alone: a change the incremental path misses, here by a
/// failpoint standing for a defect in it, in a release build's way, stays missed by every such sync, and
/// by every sync with nothing to do; the first sync the check is due for sets the ledger against what
/// the state says, finds the ledger vouched for what the state does not, says so in the log as a defect,
/// and takes it up as the full sync would, within the round. A check that agrees says so, once.
#[test]
fn the_periodic_check_takes_what_the_incremental_path_missed_and_says_so() {
    let (_sb, _s, a, b) = pair("d-full-check");
    // Apart, so that the one is no neighbour of the other. On Unix a folder, whose mode the scan reads
    // nothing new for, so that a sync with nothing to do records it as it is; Windows holds no folder's
    // mode, and there the read-only bit of a file.
    if cfg!(unix) {
        a.write("far/blind/inside", "first");
    } else {
        a.write("far/blind", "first");
    }
    a.write("near/seen", "first");
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    let blind = [
        ("CAIRN_FP_INCR_ORACLE", "0"),
        ("CAIRN_FP_INCR_BLIND", "blind"),
    ];
    let missed = |d: &Device| {
        let p = std::fs::metadata(d.path("far/blind"))
            .unwrap()
            .permissions();
        #[cfg(unix)]
        return std::os::unix::fs::PermissionsExt::mode(&p) & 0o777 == 0o750;
        #[cfg(not(unix))]
        return p.readonly();
    };
    let db = Daemon::events_only(&b);
    {
        let da = Daemon::start_env(
            &a,
            &["--reconcile-secs", DAY, "--full-sync-secs", DAY],
            &blind,
        );
        da.wait_state("synced");
        chmod(&a.path("far/blind"), if cfg!(unix) { 0o750 } else { 0o444 });
        a.write("near/seen", "edited");
        wait_file(&db, b.path("near/seen"), b"edited");
        da.wait_rest();
        assert!(!missed(&b), "nothing missed it");
        assert!(!da.log().contains("defect"), "{}", da.log());
    }
    // Nothing to do, as far as the incremental path can tell, so the next sync takes the shortcut of one
    // with nothing to do: the check does not.
    let steps = a.root.with_extension("steps-steady");
    let path = steps.to_str().unwrap().to_string();
    for _ in 0..2 {
        let run = a.sync_env(&[blind[0], blind[1], ("CAIRN_FP_LOG", &path)]);
        assert!(run.ok(), "{}", run.text());
    }
    #[cfg(unix)]
    {
        let hits = std::fs::read_to_string(&steps).unwrap_or_default();
        assert!(hits.lines().any(|l| l == "sync.fast"), "{hits}");
    }
    std::thread::sleep(Duration::from_millis(1100));
    let da = Daemon::start_env(
        &a,
        &["--reconcile-secs", DAY, "--full-sync-secs", "1"],
        &blind,
    );
    let log = da.log.clone();
    wait_for("the mode to arrive", &log, || missed(&b));
    wait_for("the defect", &log, || da.log().contains("event: a defect"));
    let text = da.log();
    assert!(
        text.contains(
            "event: a defect: the incremental bookkeeping vouched for far/blind, which the state says is not settled"
        ),
        "{text}"
    );
    // Taken up within the bounded round, not the whole tree.
    assert!(!text.contains("looked at the whole tree"), "{text}");
    std::thread::sleep(Duration::from_millis(1100));
    a.write("near/seen", "again");
    wait_file(&db, b.path("near/seen"), b"again");
    wait_for("a check that agrees", &log, || {
        da.log()
            .contains("checked the ledger against the state, as every 1 seconds: it agrees")
    });
    assert_eq!(
        da.log().matches("event: a defect").count(),
        1,
        "{}",
        da.log()
    );
}

/// The same failpoint on a file edited in place, whose folder's times say nothing: missed by the syncs
/// that take what changed alone, found by the check, by what the state says of its content, and taken up
/// within the round.
#[test]
fn the_periodic_check_finds_an_edit_the_incremental_path_missed() {
    let (_sb, _s, a, b) = pair("d-check-edit");
    a.write("far/e/blind", "first");
    a.write("near/seen", "first");
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    let blind = [
        ("CAIRN_FP_INCR_ORACLE", "0"),
        ("CAIRN_FP_INCR_BLIND", "blind"),
    ];
    std::fs::write(a.path("far/e/blind"), "edited in place").unwrap();
    a.write("near/seen", "edited");
    let run = a.sync_env(&blind);
    assert!(run.ok(), "{}", run.text());
    b.synced();
    assert_eq!(
        b.read("far/e/blind"),
        b"first",
        "nothing missed it: {}",
        run.text()
    );
    std::thread::sleep(Duration::from_millis(1100));
    let da = Daemon::start_env(
        &a,
        &["--reconcile-secs", DAY, "--full-sync-secs", "1"],
        &blind,
    );
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("far/e/blind"), b"edited in place");
    let text = da.log();
    let line = "the incremental bookkeeping vouched for far/e/blind, which the state says is not settled: the scan found another file than its record";
    assert!(text.contains(line), "{text}");
    assert!(!text.contains("looked at the whole tree"), "{text}");
}

/// A write to the state whose mark was lost, as a trigger that did not fire would lose it: here a file's
/// record made racy, which says it is to be read again, with its mark taken away. Syncs that take what
/// changed alone trust the ledger, which still vouches for the file; the check finds the state says
/// otherwise, says so as a defect, and reads the file as a full sync would.
#[test]
fn the_periodic_check_finds_a_write_whose_mark_was_lost() {
    let (_sb, _s, a, b) = pair("d-check-unmarked");
    a.write("far/unmarked", "first");
    a.write("near/seen", "first");
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    let db = a.path(".cairn/state.sqlite");
    let racy = || -> bool { row_named(&db, b"unmarked").racy };
    {
        let n = edit_rows(&db, |n| named(n, b"unmarked"), |r| r.racy = true);
        assert_eq!(n, 1);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DELETE FROM dirty", []).unwrap();
    }
    assert!(racy());
    // A sync that takes what changed alone, as a release build runs it, goes by the ledger.
    let release = [("CAIRN_FP_INCR_ORACLE", "0")];
    a.write("near/seen", "edited");
    let run = a.sync_env(&release);
    assert!(run.ok(), "{}", run.text());
    assert!(racy(), "the ledger was not trusted: {}", run.text());
    std::thread::sleep(Duration::from_millis(1100));
    let da = Daemon::start_env(
        &a,
        &["--reconcile-secs", DAY, "--full-sync-secs", "1"],
        &release,
    );
    let log = da.log.clone();
    wait_for("the defect", &log, || da.log().contains("event: a defect"));
    wait_for("the file read again", &log, || !racy());
    assert!(
        da.log().contains(
            "the incremental bookkeeping vouched for far/unmarked, which the state says is not settled: its record was read too soon after a change to trust"
        ),
        "{}",
        da.log()
    );
    std::thread::sleep(Duration::from_millis(1100));
    a.write("near/seen", "again");
    wait_for("a check that agrees", &log, || {
        da.log().contains("as every 1 seconds: it agrees")
    });
    let dbd = Daemon::events_only(&b);
    wait_file(&dbd, b.path("near/seen"), b"again");
    assert_eq!(b.read("far/unmarked"), b"first");
}

/// The check holds little besides the round it checks: the ledger against the state object by object,
/// no second decision, no second copy of the tree. Peak memory of a daemon whose sync is the check, on a
/// tree of thousands, against one whose sync is not, each with one edit to take: within a fifth more,
/// and a few megabytes whatever the tree's size, for the threads that parse the state, where a second
/// decision beside the round would hold some 20 KB an entry, a quarter of a gigabyte here.
#[cfg(target_os = "linux")]
#[test]
fn the_periodic_check_holds_little_more_than_the_round() {
    let (_sb, _s, a, b) = pair("d-check-memory");
    for i in 0..12_000 {
        a.write(&format!("t/{}/{}/{i}", i % 30, i % 7), format!("{i}"));
    }
    for d in [&a, &b, &a] {
        d.synced();
    }
    let release = [("CAIRN_FP_INCR_ORACLE", "0"), ("CAIRN_FP_FAST_ORACLE", "0")];
    let peak = |full_sync: &str, edit: &str, done: &str| -> u64 {
        a.write(edit, "edited");
        let d = Daemon::start_env(
            &a,
            &["--reconcile-secs", DAY, "--full-sync-secs", full_sync],
            &release,
        );
        let log = d.log.clone();
        wait_for(done, &log, || d.log().contains(done));
        let status = std::fs::read_to_string(format!("/proc/{}/status", d.child.id())).unwrap();
        status
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap()
    };
    let round = peak(DAY, "t/3/3/3", "synced to sequence");
    std::thread::sleep(Duration::from_millis(1100));
    let check = peak("1", "t/4/4/4", "checked the ledger against the state");
    eprintln!("the check peaked at {check} kB, the round at {round} kB");
    assert!(
        check <= round + round / 5 + 8 * 1024,
        "the check peaked at {check} kB, the round at {round} kB"
    );
}

/// A rule taken off a folder whose copy is here already, the server's bytes and times with other modes,
/// as on a device that left its photos out until then: the daemon's sync that takes it in holds about
/// what it decides about, not the whole tree. On a tree of 16,000 with 2,200 taken in, its peak against
/// that of the same daemon's sync just before, which listed the whole tree and took one edit: within
/// 50 KB more for each file taken in (31 KB when measured), where the round confirming the join took the
/// whole tree and held some 11 KB more for every entry of it, 79 KB for each file taken in here, and on a
/// Mac with 557,000 entries 8.9 GB.
#[cfg(target_os = "linux")]
#[test]
fn a_rule_taken_off_what_is_here_holds_what_the_join_decides_about() {
    const REST: usize = 14_000;
    const MEDIA: usize = 2_200;
    let (_sb, _s, a, b) = pair("d-rule-off-memory");
    let rest = |i: usize| format!("rest/{}/{}/{i}", i % 30, i % 7);
    let media = |i: usize| format!("media/{}/{i}", i % 6);
    for i in 0..REST {
        a.write(&rest(i), format!("{i}"));
    }
    for i in 0..MEDIA {
        a.write(&media(i), format!("photo {i}"));
        chmod(&a.path(&media(i)), 0o664);
    }
    a.synced();
    std::fs::write(b.path(".cairn/ignore"), "media/\n").unwrap();
    for (path, text) in (0..REST)
        .map(|i| (rest(i), format!("{i}")))
        .chain((0..MEDIA).map(|i| (media(i), format!("photo {i}"))))
    {
        b.write(&path, text);
        let (sec, nsec) = mtime_of(&a.path(&path));
        set_mtime(&b.path(&path), sec, nsec);
    }
    for i in 0..MEDIA {
        chmod(&b.path(&media(i)), 0o644);
    }
    b.synced();
    b.synced();
    let release = [("CAIRN_FP_INCR_ORACLE", "0"), ("CAIRN_FP_FAST_ORACLE", "0")];
    b.write("rest/3/3/3", "edited");
    let d = Daemon::start_env(&b, &["--reconcile-secs", DAY], &release);
    let peak = || -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{}/status", d.child.id())).unwrap();
        status
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap()
    };
    let synced = |n: usize| d.log().matches("synced to sequence").count() >= n;
    wait_for("the edit", &d.log, || synced(1));
    let edit = peak();
    std::fs::write(b.path(".cairn/ignore"), "").unwrap();
    wait_for("the join", &d.log, || synced(2));
    let join = peak();
    let log = d.log();
    eprintln!("the join peaked at {join} kB, the sync of one edit at {edit} kB");
    assert!(!log.contains("looked at the whole tree"), "{log}");
    assert_eq!(mode_of(&b.path(&media(0))), 0o664, "{log}");
    let most = edit + 50 * MEDIA as u64;
    assert!(
        join <= most,
        "the join peaked at {join} kB, the sync of one edit at {edit} kB: at most {most} kB expected"
    );
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
        skip(&format!("the queue here takes {max} events"));
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

/// What a sync held goes back to the system as it ends, not after a pause: a big tree's sync peaks in
/// gigabytes, which an idle daemon would otherwise hold until the pause and half a minute had passed.
#[test]
fn a_daemon_gives_memory_back_as_each_sync_ends() {
    let (sb, _s, a, _b) = pair("d-give-back");
    let steps = sb.path("steps");
    let da = Daemon::start_env(
        &a,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_LOG", steps.to_str().unwrap())],
    );
    let count = |name: &str| {
        std::fs::read_to_string(&steps)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == name)
            .count()
    };
    for round in 0..3 {
        a.write(&format!("f{round}"), format!("round {round}"));
        let log = da.log.clone();
        wait_for("the file to be sent", &log, || {
            da.state() == "synced"
                && da.status()["recent"]
                    .to_string()
                    .contains(&format!("f{round}"))
        });
        da.wait_rest();
        let synced = count("daemon.synced");
        assert!(synced > round, "{}", da.log());
        assert_eq!(
            count("daemon.gave_back"),
            synced,
            "round {round}: {}",
            da.log()
        );
    }
}

/// A request tried again is two lines in the daemon's log, not one a try: its first new try, naming the
/// error, and how it ended, answered or given up.
#[test]
fn a_daemon_says_a_request_tried_again_in_two_lines() {
    let enobufs = if cfg!(target_os = "macos") {
        55
    } else if cfg!(windows) {
        10055
    } else {
        105
    };
    let lines = |log: &str, has: &str| log.lines().filter(|l| l.contains(has)).count();
    let (_sb, _s, a, b) = pair("d-retry");

    a.write("f", "sent on the third try");
    let spec = format!("client.commit:{enobufs}:2");
    let da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &[("CAIRN_FP_NET", &spec)]);
    da.wait_state("synced");
    let log = da.log();
    drop(da);
    b.synced();
    assert_eq!(b.read("f"), b"sent on the third try");
    assert_eq!(lines(&log, "trying again"), 1, "{log}");
    assert!(
        log.lines().any(|l| l.contains("committing: ")
            && l.contains(&format!("(os error {enobufs})"))
            && l.ends_with("; trying again for up to about a minute")),
        "{log}"
    );
    assert_eq!(
        lines(&log, "committing: answered on try 3 after "),
        1,
        "{log}"
    );
    assert_eq!(lines(&log, " of 7"), 0, "{log}");
    assert!(
        log.contains("tried requests again after the network failed them: 2 time(s)"),
        "{log}"
    );

    // Past the budget: the first, the end, and the sync's error; the next sync delivers.
    a.write("g", "sent by the next sync");
    let spec = format!("client.commit:{enobufs}:8");
    let da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &[("CAIRN_FP_NET", &spec)]);
    let path = da.log.clone();
    wait_for("the next sync to deliver", &path, || {
        da.log().contains("error: ") && da.state() == "synced"
    });
    let log = da.log();
    drop(da);
    b.synced();
    assert_eq!(b.read("g"), b"sent by the next sync");
    assert_eq!(
        lines(&log, "; trying again for up to about a minute"),
        1,
        "{log}"
    );
    assert_eq!(
        lines(&log, "committing: gave up on try 8 after "),
        1,
        "{log}"
    );
    assert_eq!(
        lines(
            &log,
            &format!("(os error {enobufs}); tried 8 times; trying again")
        ),
        1,
        "{log}"
    );
    assert_eq!(lines(&log, " of 7"), 0, "{log}");
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

/// Killed while taking on an update, and started again after a revert elsewhere: what it had received is
/// not its own edit, so the revert stands, as it does for a one-shot sync (kills.rs).
#[test]
fn a_daemon_killed_while_receiving_does_not_undo_a_revert_elsewhere() {
    for point in [
        "apply.replace.swapped",
        "apply.replace.renamed",
        "sync.applied",
    ] {
        let (_sb, _server, a, b) = pair("d-kill-arriving");
        for i in 0..14 {
            a.write(&format!("fill/{i}"), format!("{i}"));
        }
        a.write("x", "v1");
        a.synced();
        b.synced();
        a.write("x", "v2");
        a.synced();
        let mut d = Daemon::start_env(&b, &["--reconcile-secs", DAY], &[("CAIRN_FP_CRASH", point)]);
        let log = d.log.clone();
        wait_for(&format!("the daemon to die at {point}"), &log, || {
            d.exited()
        });
        drop(d);
        a.write("x", "v1");
        a.synced();
        let db = Daemon::events_only(&b);
        wait_file(&db, b.path("x"), b"v1");
        db.wait_rest();
        drop(db);
        a.synced();
        assert_eq!(a.read("x"), b"v1", "{point}: the revert was undone");
        assert!(leftover_temps(&b.root).is_empty(), "{point}");
    }
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
    copy_tree(&server.data, &backup);
    server.restart();
    a.write("second", "2");
    a.synced();

    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    std::thread::sleep(Duration::from_secs(2));
    server.kill();
    std::fs::remove_dir_all(&server.data).unwrap();
    copy_tree(&backup, &server.data);
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
    chmod(&a.path("script"), if cfg!(windows) { 0o444 } else { 0o755 });
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
    remove_file(&a.path("script"));
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
    dir_link(&target, &a.path(".cairn"));
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

/// A waiting request must not hold a stopping server up for its whole wait. Not on Windows, which has
/// no SIGTERM: a server there stops on a console's Ctrl-C, or is killed.
#[cfg(unix)]
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
/// On Windows a folder being watched says nothing when it is itself renamed: the watch on the folder
/// above is what notices.
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

    // With everything at rest, so that only the watcher can notice.
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

/// Put back at its path in another folder, as a disk mounted back on its mount point is, a root is
/// watched there. On Windows a watch is on the folder it was opened on, wherever that goes.
#[test]
fn a_root_back_in_another_folder_is_watched_there() {
    let (sb, _s, a, b) = pair("d-refolder");
    a.write("f", "x");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    wait_file(&db, b.path("f"), b"x");
    da.wait_rest();

    // Paused, so that no sync looks at it halfway there.
    a.cairn(&["pause"]).expect_ok();
    da.wait_state("paused");
    let held = sb.path("held-away");
    std::fs::rename(&a.root, &held).unwrap();
    std::fs::create_dir(&a.root).unwrap();
    for entry in std::fs::read_dir(&held).unwrap() {
        let name = entry.unwrap().file_name();
        std::fs::rename(held.join(&name), a.root.join(&name)).unwrap();
    }
    a.cairn(&["resume"]).expect_ok();
    da.wait_state("synced");

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

/// Nor a build writing under a folder the rules leave out, though the rule names only the folder: on
/// macOS the stream reports every path in the tree, and the file's own path matches no rule.
#[test]
fn a_busy_build_under_an_ignored_folder_does_not_wake_the_daemon() {
    let (_sb, _s, a, _b) = pair("d-busy-dir");
    std::fs::write(a.path(".cairn/ignore"), "build/\n").unwrap();
    std::fs::create_dir_all(a.path("app/build/out")).unwrap();
    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    std::thread::sleep(Duration::from_secs(2));
    let before = da.syncs();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (dir, stop) = (a.path("app/build/out"), stop.clone());
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::SeqCst) {
                i += 1;
                std::fs::write(dir.join("main.o"), i.to_string()).unwrap();
                let tmp = dir.join(format!("t{}", i % 4));
                std::fs::write(&tmp, "x").unwrap();
                std::fs::remove_file(&tmp).unwrap();
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
//
// Not on Windows: these need a Linux user and mount namespace. A root that goes away is tested there by
// its marker going, above.

/// Run one `inner_` test below in a private user and mount namespace.
#[cfg(unix)]
#[track_caller]
fn namespaced(test: &str) {
    if let Some(ok) = in_namespace(test, None) {
        assert!(ok, "see the output above");
    }
}

#[cfg(unix)]
#[track_caller]
fn sh(program: &str, args: &[&str]) {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
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
#[cfg(unix)]
#[test]
fn a_bind_mount_going_away_is_found_by_the_reconcile_and_coming_back_resumes() {
    namespaced("inner_bind_mount");
}

#[cfg(unix)]
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
#[cfg(unix)]
#[test]
fn a_filesystem_unmounted_pauses_at_once_and_a_remount_resumes_with_watches() {
    namespaced("inner_real_unmount");
}

#[cfg(unix)]
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

// --- The local API: status, pause and resume, look now, the list of roots ---

/// A user directory of the test's own, for its list of roots and, on macOS and Windows, its daemons'
/// locks and sockets. Every process of the test gets the same one. In the temp directory, as a macOS
/// socket path holds at most 104 bytes.
struct UserDir(PathBuf);

impl UserDir {
    fn new() -> Self {
        let d = std::env::temp_dir().join(format!("cairn-u-{:08x}", rand::random::<u32>()));
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    fn env(&self) -> [(&str, &str); 1] {
        [("CAIRN_USER_DIR", self.0.to_str().unwrap())]
    }
}

impl Drop for UserDir {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn json(run: &Run) -> serde_json::Value {
    serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", run.text()))
}

/// Whether the daemon's list of recent changes has one like this.
fn has_activity(status: &serde_json::Value, direction: &str, change: &str, path: &str) -> bool {
    status["recent"].as_array().is_some_and(|r| {
        r.iter()
            .any(|a| a["direction"] == direction && a["change"] == change && a["path"] == path)
    })
}

#[test]
fn a_pause_outlives_a_kill_and_a_restart() {
    let (_sb, _s, a, b) = pair("d-pk");
    let user = UserDir::new();
    let env = user.env();
    let mut da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &env);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");

    let run = a.cairn_env(&["pause"], &env).expect_ok();
    assert!(run.stdout.contains("paused"), "{}", run.text());
    da.wait_state("paused");
    assert!(da.status()["paused_since"].is_i64(), "{}", da.status());
    let syncs = da.syncs();
    a.write("while-paused", "1");
    std::thread::sleep(Duration::from_secs(3));
    assert!(!b.exists("while-paused"));
    assert_eq!(da.syncs(), syncs, "{}", da.log());

    // Killed and started again, it is still paused, and syncs nothing.
    let old = da.status()["pid"].clone();
    da.kill();
    let da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &env);
    let log = da.log.clone();
    wait_for("the new daemon to say it is paused", &log, || {
        let s = da.status();
        s["pid"] != old && s["state"] == "paused"
    });
    std::thread::sleep(Duration::from_secs(3));
    assert!(!b.exists("while-paused"));
    assert_eq!(da.syncs(), 0, "{}", da.log());
    let status = a.cairn_env(&["status"], &env).expect_ok();
    assert!(
        status.stdout.contains("paused by request since"),
        "{}",
        status.text()
    );
    // Listed by the daemon that started, and shown paused there too.
    let all = json(
        &a.cairn_env(&["status", "--all", "--json"], &env)
            .expect_ok(),
    );
    let listed = all.as_array().unwrap();
    assert_eq!(listed.len(), 1, "{all}");
    assert_eq!(listed[0]["present"], true, "{all}");
    assert_eq!(listed[0]["daemon"]["state"], "paused", "{all}");

    a.cairn_env(&["resume"], &env).expect_ok();
    wait_file(&db, b.path("while-paused"), b"1");
    da.wait_state("synced");
    assert!(da.status()["paused_since"].is_null(), "{}", da.status());
    // A resumed daemon that is killed stays resumed.
    drop(da);
    let da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &env);
    da.wait_state("synced");
    a.write("after", "2");
    wait_file(&db, b.path("after"), b"2");
}

#[test]
fn every_change_made_while_paused_arrives_after_resume() {
    let (_sb, _s, a, b) = pair("d-pr");
    let user = UserDir::new();
    let env = user.env();
    let da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &env);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");
    a.write("edit", "0");
    a.write("gone", "x");
    a.write("old-name", "n");
    a.write("both", "0");
    wait_file(&db, b.path("both"), b"0");
    db.wait_rest();
    da.wait_rest();

    a.cairn_env(&["pause"], &env).expect_ok();
    da.wait_state("paused");
    a.write("new", "1");
    a.write("edit", "2");
    std::fs::remove_file(a.path("gone")).unwrap();
    std::fs::rename(a.path("old-name"), a.path("new-name")).unwrap();
    a.write("dir/sub/f", "f");
    a.write("both", "a");
    // The other device goes on, and this one takes nothing of it while paused.
    b.write("from-b", "b");
    b.write("both", "b");
    db.wait_rest();
    std::thread::sleep(Duration::from_secs(2));
    assert!(!b.exists("new") && !a.exists("from-b"));
    assert_eq!(b.read("edit"), b"0");
    let run = a.cairn_env(&["ask", "sync_now"], &env);
    assert!(!run.ok() && run.stdout.contains("paused"), "{}", run.text());

    a.cairn_env(&["resume"], &env).expect_ok();
    wait_file(&db, b.path("new"), b"1");
    wait_file(&db, b.path("edit"), b"2");
    wait_gone(&db, b.path("gone"));
    wait_file(&db, b.path("new-name"), b"n");
    wait_gone(&db, b.path("old-name"));
    wait_file(&db, b.path("dir/sub/f"), b"f");
    wait_file(&da, a.path("from-b"), b"b");
    da.wait_state("synced");
    da.wait_rest();

    // Both edited `both`: one keeps the name, the other is a copy, in the conflict inbox.
    let copies = a.conflict_copies("", "both");
    assert_eq!(copies.len(), 1, "{copies:?}");
    let run = a.cairn_env(&["ask", "conflicts"], &env).expect_ok();
    let reply = json(&run);
    assert_eq!(reply["reply"], "conflicts", "{reply}");
    let open = reply["open"].as_array().unwrap();
    assert_eq!(open.len(), 1, "{reply}");
    let copy = open[0]["copy"].as_str().expect("the copy is here");
    assert!(
        Path::new(copy).exists() && copy.ends_with(&copies[0]),
        "{reply}"
    );
    assert!(
        open[0]["original"].as_str().unwrap().ends_with("both"),
        "{reply}"
    );

    // What went up and what came down, in the daemon's list of recent changes.
    let s = da.status();
    for (direction, change, path) in [
        ("up", "added", "new"),
        ("up", "changed", "edit"),
        ("up", "removed", "gone"),
        ("up", "moved", "old-name"),
        ("up", "added", "dir"),
        ("down", "added", "from-b"),
    ] {
        assert!(
            has_activity(&s, direction, change, path),
            "{direction} {change} {path}: {}",
            s["recent"]
        );
    }
}

#[test]
fn a_subscriber_that_never_reads_does_not_stall_syncs() {
    let (_sb, _s, a, b) = pair("d-stuck");
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");
    // A program that subscribes and then stops reading: `cairn ask` with nobody reading its output,
    // so that once its pipe is full it reads no more from the daemon.
    let mut stuck = Command::new(CAIRN)
        .arg("--root")
        .arg(&a.root)
        .args(["ask", "subscribe"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut round = 0;
    while !da.log().contains("stopped reading its status") {
        round += 1;
        assert!(round <= 120, "never dropped: {}", da.log());
        // Many at once, so each status is long.
        for i in 0..40 {
            a.write(&format!("r{round}/f{i}"), format!("{round}"));
        }
        wait_file(
            &db,
            b.path(&format!("r{round}/f39")),
            round.to_string().as_bytes(),
        );
    }
    // And it goes on as before.
    a.write("after", "x");
    wait_file(&db, b.path("after"), b"x");
    let _ = stuck.kill();
    let _ = stuck.wait();
}

#[test]
fn sync_now_brings_a_change_no_event_announced() {
    let (_sb, _s, a, b) = pair("d-now");
    let da = Daemon::start(&a, &["--no-watch", "--reconcile-secs", DAY]);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");
    da.wait_rest();
    a.write("unannounced", "u");
    std::thread::sleep(Duration::from_secs(3));
    assert!(!b.exists("unannounced"));
    let run = a.cairn(&["ask", "sync_now"]).expect_ok();
    assert!(run.stdout.contains(r#""reply":"done""#), "{}", run.text());
    wait_file(&db, b.path("unannounced"), b"u");
}

#[test]
fn roots_made_at_once_are_all_listed_and_one_gone_stays_listed() {
    let sb = Sandbox::on_disk("d-reg");
    let server = Server::start(sb.path("server"));
    let user = UserDir::new();
    let env = user.env();
    let token = server.token_file.to_str().unwrap().to_string();
    let roots: Vec<PathBuf> = (0..24).map(|i| sb.path(&format!("r{i}"))).collect();
    std::thread::scope(|s| {
        for (i, r) in roots.iter().enumerate() {
            std::fs::create_dir_all(r).unwrap();
            let (url, token, env) = (&server.url, &token, &env);
            s.spawn(move || {
                let name = format!("dev{i}");
                let root = r.to_str().unwrap();
                run(
                    &[
                        "init",
                        "--server",
                        url,
                        "--token-file",
                        token,
                        "--device-name",
                        &name,
                        root,
                    ],
                    env,
                )
                .expect_ok();
            });
        }
    });
    let listed = |env: &[(&str, &str)]| {
        let all = json(&run(&["status", "--all", "--json"], env).expect_ok());
        all.as_array().unwrap().clone()
    };
    let all = listed(&env);
    assert_eq!(all.len(), roots.len(), "{all:?}");
    for r in &roots {
        assert!(
            all.iter()
                .any(|v| v["root"]["path"] == r.to_str().unwrap() && v["present"] == true),
            "{} is not listed: {all:?}",
            r.display()
        );
    }

    std::fs::remove_dir_all(&roots[0]).unwrap();
    let all = listed(&env);
    assert_eq!(all.len(), roots.len(), "{all:?}");
    let gone: Vec<_> = all.iter().filter(|v| v["present"] == false).collect();
    assert_eq!(gone.len(), 1, "{all:?}");
    assert_eq!(gone[0]["root"]["path"], roots[0].to_str().unwrap());
    let text = run(&["status", "--all"], &env).expect_ok();
    assert!(
        text.stdout.contains(": gone: no root with ID"),
        "{}",
        text.text()
    );
}

/// A root that has gone stays listed until a person forgets it; one still there is forgotten only when
/// asked, and its daemon lists it again when it starts.
#[test]
fn forget_takes_off_a_root_that_has_gone_and_one_still_there_only_when_asked() {
    let sb = Sandbox::on_disk("d-forget");
    let server = Server::start(sb.path("server"));
    let user = UserDir::new();
    let env = user.env();
    let token = server.token_file.to_str().unwrap().to_string();
    let gone = sb.path("gone");
    std::fs::create_dir_all(&gone).unwrap();
    let gone_str = gone.to_str().unwrap();
    run(
        &[
            "init",
            "--server",
            &server.url,
            "--token-file",
            &token,
            "--device-name",
            "gone",
            gone_str,
        ],
        &env,
    )
    .expect_ok();
    let a = Device::new(&sb, &server, "a");
    let a_str = a.root.to_str().unwrap();
    let id = std::fs::read_to_string(a.path(".cairn/root-id")).unwrap();
    let id = id.trim();
    let mut da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &env);
    da.wait_state("synced");
    let paths = || -> Vec<String> {
        let all = json(&run(&["status", "--all", "--json"], &env).expect_ok());
        let mut p: Vec<String> = all
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["root"]["path"].as_str().unwrap().to_string())
            .collect();
        p.sort();
        p
    };
    let both = {
        let mut b = vec![a_str.to_string(), gone_str.to_string()];
        b.sort();
        b
    };
    assert_eq!(paths(), both);

    // Still there: refused, and still listed.
    for asked in [a_str, gone_str] {
        let r = run(&["forget", asked], &env);
        assert!(
            !r.ok() && r.stderr.contains("still there") && r.stderr.contains("--even-if-present"),
            "{}",
            r.text()
        );
    }
    assert_eq!(paths(), both);

    // Gone: taken off, and said so. Asked again, it is not listed, which is said too.
    std::fs::remove_dir_all(&gone).unwrap();
    let r = run(&["forget", gone_str], &env).expect_ok();
    assert!(r.stdout.contains("had gone"), "{}", r.text());
    assert_eq!(paths(), vec![a_str.to_string()]);
    let r = run(&["forget", gone_str], &env);
    assert!(
        !r.ok() && r.stderr.contains("no root is listed"),
        "{}",
        r.text()
    );

    // Still there, asked explicitly and by its ID: off the list, and the root goes on as it was.
    let r = run(&["forget", "--even-if-present", id], &env).expect_ok();
    assert!(r.stdout.contains("still a root"), "{}", r.text());
    assert_eq!(paths(), Vec::<String>::new());
    a.write("after", "1");
    let log = da.log.clone();
    wait_for("the daemon to send what is new", &log, || {
        has_activity(&da.status(), "up", "added", "after")
    });
    da.kill();
    let da = Daemon::start_env(&a, &["--reconcile-secs", DAY], &env);
    wait_for("the daemon to list its root again", &da.log, || {
        paths() == vec![a_str.to_string()]
    });
}

/// An init its server does not answer, or refuses, makes nothing and lists nothing, and says so: one with
/// no server to reach once left a root listed, and a folder that a second init called a root already.
#[test]
fn an_init_the_server_does_not_take_leaves_no_root_and_no_entry() {
    let sb = Sandbox::on_disk("d-init-fails");
    let server = Server::start(sb.path("server"));
    let user = UserDir::new();
    let env = user.env();
    let token = server.token_file.to_str().unwrap().to_string();
    let wrong = sb.path("wrong-token");
    std::fs::write(&wrong, "not the token").unwrap();
    let wrong = wrong.to_str().unwrap();
    // A port nobody listens on.
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let r = sb.path("r");
    std::fs::create_dir_all(&r).unwrap();
    let root = r.to_str().unwrap();
    let init = |url: &str, token: &str| {
        run(
            &[
                "init",
                "--server",
                url,
                "--token-file",
                token,
                "--device-name",
                "r",
                root,
            ],
            &env,
        )
    };
    let listed = || json(&run(&["status", "--all", "--json"], &env).expect_ok());
    for (url, token, why) in [
        (closed.as_str(), token.as_str(), "cannot reach the server"),
        (server.url.as_str(), wrong, "refused the access token"),
    ] {
        let out = init(url, token);
        assert!(
            !out.ok() && out.stderr.contains(why) && out.stderr.contains("not made a sync root"),
            "{url}: {}",
            out.text()
        );
        assert!(!r.join(".cairn").exists(), "{url}: {}", out.text());
        assert_eq!(listed(), serde_json::json!([]), "{url}: {}", out.text());
    }

    // The same folder, with its server there: a root, and listed.
    init(&server.url, &token).expect_ok();
    let all = listed();
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0]["root"]["path"], root, "{all:?}");
    assert_eq!(all[0]["root"]["server"], server.url.as_str(), "{all:?}");
    assert_eq!(all[0]["present"], true, "{all:?}");
}

/// The harness's own `cairn` keeps its list of roots, and on macOS and Windows its daemons' locks and
/// sockets, out of the user's own directory, where test runs once left a lock file for each root made.
#[test]
fn the_harness_leaves_nothing_in_the_users_own_directory() {
    let sb = Sandbox::on_disk("d-home");
    let server = Server::start(sb.path("server"));
    let home = sb.path("home");
    std::fs::create_dir_all(&home).unwrap();
    let h = home.to_str().unwrap();
    let (state, local) = (format!("{h}/state"), format!("{h}/local"));
    // Empty counts as unset, for the harness as for `cairn`.
    let env = [
        ("HOME", h),
        ("XDG_STATE_HOME", state.as_str()),
        ("LOCALAPPDATA", local.as_str()),
        ("CAIRN_USER_DIR", ""),
    ];
    let d = Device {
        root: sb.path("r"),
        name: "r".into(),
    };
    std::fs::create_dir_all(&d.root).unwrap();
    let token = server.token_file.to_str().unwrap().to_string();
    let root = d.root.to_str().unwrap();
    run(
        &[
            "init",
            "--server",
            &server.url,
            "--token-file",
            &token,
            "--device-name",
            "r",
            root,
        ],
        &env,
    )
    .expect_ok();
    let da = Daemon::start_env(&d, &["--reconcile-secs", DAY], &env);
    da.wait_state("synced");
    d.cairn_env(&["status"], &env).expect_ok();
    // Listed, in the harness's own directory.
    let id = std::fs::read_to_string(d.path(".cairn/root-id")).unwrap();
    let listed = cairn_harness::user_dir().join("roots.json");
    let reg = std::fs::read_to_string(&listed).unwrap_or_default();
    assert!(reg.contains(id.trim()), "{}: {reg}", listed.display());
    let left: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// The daemons on machines not yet given this version hold their lock and write their file, and
/// answer nothing on it.
#[cfg(target_os = "linux")]
#[test]
fn a_daemon_from_before_the_local_api_is_read_from_its_file() {
    let (_sb, _s, a, _b) = pair("d-old");
    let id = std::fs::read_to_string(a.path(".cairn/root-id")).unwrap();
    let _held = cairn_engine::daemon::take_lock(id.trim()).unwrap();
    std::fs::write(
        a.path(".cairn/daemon.json"),
        r#"{"pid":4242,"state":"synced","detail":null,"last_sync":1,"last_synced":1,"head":3,
           "syncs":9,"watched":2,"unwatched":0,"watch_limit_reached":false,"feed_down":null,
           "skips":[],"waiting":null,"not_applied":[],"not_applied_count":0}"#,
    )
    .unwrap();
    let v = json(&a.cairn(&["status", "--json"]).expect_ok());
    assert_eq!(v["daemon"]["pid"], 4242, "{v}");
    assert_eq!(v["daemon"]["state"], "synced", "{v}");
    assert_eq!(v["daemon"]["recent"], serde_json::json!([]), "{v}");
    let text = a.cairn(&["status"]).expect_ok();
    assert!(
        text.stdout.contains("daemon: running as process 4242"),
        "{}",
        text.text()
    );
    // What it cannot answer is said, not waited for.
    let run = a.cairn(&["pause"]);
    assert!(
        !run.ok() && run.stderr.contains("no daemon answers"),
        "{}",
        run.text()
    );
}

#[test]
fn a_subscriber_sees_each_sync_start_and_what_it_did() {
    use std::io::BufRead;
    let (_sb, _s, a, _b) = pair("d-sub");
    let da = Daemon::events_only(&a);
    da.wait_state("synced");
    da.wait_rest();
    let mut sub = Command::new(CAIRN)
        .arg("--root")
        .arg(&a.root)
        .args(["ask", "subscribe"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = sub.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let next = || -> serde_json::Value {
        let line = rx
            .recv_timeout(SOON)
            .unwrap_or_else(|e| panic!("no status ({e}): {}", da.log()));
        serde_json::from_str(&line).unwrap()
    };
    // The status as it is, first.
    let first = next();
    assert_eq!(
        (&first["reply"], &first["state"], &first["syncing"]),
        (&"status".into(), &"synced".into(), &false.into()),
        "{first}"
    );
    a.write("x", "1");
    let mut started = false;
    loop {
        let s = next();
        if s["syncing"] == true {
            started = true;
        } else if has_activity(&s, "up", "added", "x") {
            assert!(started, "no status said the sync had started: {s}");
            assert_eq!(s["state"], "synced", "{s}");
            break;
        }
    }
    let _ = sub.kill();
    let _ = sub.wait();
}

/// A long sync says what it is doing, how far it has got and when anything last moved, in the status
/// and in the log while it runs, and one in which nothing moves says so. A scan that read a folder just
/// taken in ran for most of an hour with a silent log and a status that said only "synced", and was
/// taken for hung.
#[test]
fn a_long_sync_says_how_far_it_has_got_and_a_stuck_one_says_so() {
    let (sb, _s, a, _b) = pair("d-progress");
    for f in ["one", "two", "three"] {
        a.write(f, f);
    }
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("daemon.sync:{}", gate.display());
    let tune = "daemon.progress_first_ms=300,daemon.progress_said_ms=700,\
                daemon.stalled_ms=1500,daemon.progress_shown_ms=100";
    let da = Daemon::start_env(
        &a,
        &["--reconcile-secs", DAY],
        &[
            ("CAIRN_FP_PAUSE", &pause),
            ("CAIRN_FP_DELAY", "scan.file.reading:1000"),
            ("CAIRN_FP_TUNE", tune),
        ],
    );
    let log = da.log.clone();
    // Held before it has done anything: nothing moves, and the log says so.
    wait_for("the stall to be said", &log, || {
        da.log().contains("nothing in this sync has moved for")
    });
    assert!(
        da.log().contains("still syncing, for")
            && da.log().contains("loading what this device knows"),
        "{}",
        da.log()
    );
    // `--all`, as the tray asks: `cairn status` alone would also run a dry sync, which reads the
    // files the daemon is about to.
    let status = a.cairn(&["status", "--all"]);
    assert!(
        status.stdout.contains("syncing now, its first sync")
            && status.stdout.contains("daemon: syncing for")
            && status.stdout.contains("last moved"),
        "{}",
        status.text()
    );
    std::fs::write(gate.join("go"), b"").unwrap();
    wait_for("the sync to move again", &log, || {
        da.log().contains("the sync moves again, after")
    });
    // Each file takes a second to read: the status names it, with what the scan has done so far.
    let mut seen = serde_json::Value::Null;
    wait_for("the status to name the file being read", &log, || {
        let run = a.cairn(&["ask", "status"]);
        let v: serde_json::Value = serde_json::from_str(run.stdout.trim()).unwrap_or_default();
        let p = &v["progress"];
        if v["syncing"] == true && p["phase"] == "scanning" && p["reading"].is_string() {
            seen = v.clone();
            return true;
        }
        false
    });
    // What a person or an agent asks: the same, in words.
    let status = a.cairn(&["status"]);
    assert!(
        status.stdout.contains("daemon: syncing for")
            && status.stdout.contains("to hash them, now ")
            && status.stdout.contains("last moved"),
        "{}",
        status.text()
    );
    let p = &seen["progress"];
    assert!(
        ["one", "two", "three"].contains(&p["reading"].as_str().unwrap())
            && p["files_read"].as_u64().unwrap() >= 1
            && p["entries"].as_u64().unwrap() >= 1,
        "{seen}"
    );
    wait_for("the log to say the scan goes on", &log, || {
        da.log()
            .lines()
            .any(|l| l.contains("still syncing, for") && l.contains("looking at the tree"))
    });
    da.wait_state("synced");
    let ended = da.status();
    assert!(
        ended["syncing"] == false && ended["progress"].is_null(),
        "{ended}"
    );
    assert!(
        da.log().lines().any(|l| l.contains("synced to sequence")
            && l.contains("file(s)")
            && l.contains("to hash them, took")),
        "{}",
        da.log()
    );
}

/// A mode or a time changed elsewhere is listed where it arrives, as where it was made: as metadata,
/// not as an edit, and not left out.
#[test]
fn a_change_of_metadata_alone_is_listed_where_it_arrives_too() {
    let (_sb, _s, a, b) = pair("d-meta");
    a.write("mode", "m");
    a.write("time", "t");
    a.sync().expect_ok();
    b.sync().expect_ok();
    let da = Daemon::events_only(&a);
    let db = Daemon::events_only(&b);
    da.wait_state("synced");
    db.wait_state("synced");
    // Windows holds a file's write bits alone, and times to 100 ns.
    let (mode, nsec) = if cfg!(windows) {
        (0o444, 500)
    } else {
        (0o751, 5)
    };
    chmod(&b.path("mode"), mode);
    set_mtime(&b.path("time"), 1_000_000_000, nsec);
    let log = da.log.clone();
    wait_for("the mode and the time to arrive", &log, || {
        a.exists("mode")
            && mode_of(&a.path("mode")) == mode
            && a.exists("time")
            && mtime_of(&a.path("time")) == (1_000_000_000, nsec)
    });
    da.wait_rest();
    db.wait_rest();
    let (here, there) = (da.status(), db.status());
    for path in ["mode", "time"] {
        assert!(
            has_activity(&there, "up", "metadata", path),
            "up {path}: {}",
            there["recent"]
        );
        assert!(
            has_activity(&here, "down", "metadata", path),
            "down {path}: {}",
            here["recent"]
        );
        assert!(
            !has_activity(&here, "down", "changed", path),
            "{path}: {}",
            here["recent"]
        );
    }
}

/// A join leaves most of the state's file free pages, from the marks its rounds made and dropped: the
/// daemon compacts it after that sync, says so in its log alone, once, and goes on syncing. (A big delete
/// no longer does: what it frees is the deleted files' rows, ledger entries and manifests, while their
/// nodes stay, under half of what the state holds.)
#[test]
fn the_daemon_compacts_a_state_its_join_left_mostly_free() {
    let sb = Sandbox::on_disk("daemon-compact");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    let file = |i: usize| format!("d{}/f{i}", i % 20);
    for i in 0..2000 {
        a.write(&file(i), format!("file {i}"));
    }
    a.synced();
    let d = Daemon::start_env(
        &b,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_TUNE", "state.compact_floor=0")],
    );
    wait_for("the compaction", &d.log, || {
        d.log().contains("compacted the state")
    });
    let log = d.log();
    let line = log
        .lines()
        .find(|l| l.contains("compacted the state"))
        .unwrap();
    assert!(
        line.contains("% of it free pages: ") && line.contains(" to ") && line.ends_with(" s"),
        "{line}"
    );
    assert_eq!(b.read(&file(1999)), b"file 1999");
    a.write("after", "a change after it");
    a.synced();
    wait_file(&d, b.path("after"), b"a change after it");
    let log = d.log();
    assert_eq!(log.matches("compacted the state").count(), 1, "{log}");
    assert!(!log.contains("not compacted"), "{log}");
    let (count, free) = state_pages(&b.path(".cairn/state.sqlite"));
    assert!(free * 2 <= count, "{free} of {count} pages free");
}

/// A state of 0.1.8's layout, as an update to this build finds it: the daemon's first sync moves it,
/// says so in its log alone, once, and compacts what that freed after the sync; changes go on arriving.
#[test]
fn the_daemon_moves_a_state_of_an_older_layout_and_goes_on_syncing() {
    let sb = Sandbox::on_disk("daemon-migrate");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..500 {
        a.write(&format!("d{}/f{i}", i % 10), format!("file {i}"));
    }
    a.synced();
    b.synced();
    let state = b.path(".cairn/state.sqlite");
    cairn_engine::state::legacy::to_v1(&state).unwrap();
    let d = Daemon::start_env(
        &b,
        &["--reconcile-secs", DAY],
        &[("CAIRN_FP_TUNE", "state.compact_floor=0")],
    );
    d.wait_state("synced");
    wait_for("the compaction after the move", &d.log, || {
        d.log().contains("compacted the state")
    });
    let log = d.log();
    let line = log
        .lines()
        .find(|l| l.contains("moved the state from layout 1 to 2"))
        .unwrap_or_else(|| panic!("no move in the log: {log}"));
    assert!(
        line.contains("objects") && line.ends_with("after a sync"),
        "{line}"
    );
    assert_eq!(cairn_engine::state::legacy::version(&state).unwrap(), 2);
    a.write("after", "a change after it");
    a.synced();
    wait_file(&d, b.path("after"), b"a change after it");
    assert_eq!(d.log().matches("moved the state").count(), 1, "{}", d.log());
}

/// The tray's "Recently deleted": the daemon lists what another device deleted, the same after a
/// restart, and brings it back when asked, then syncs it here and everywhere.
#[test]
fn a_daemon_lists_what_was_deleted_across_a_restart_and_brings_it_back() {
    let (_sb, _s, a, b) = pair("daemon-deleted");
    a.write("keep", "k");
    a.write("d/f", "deleted elsewhere");
    a.synced();
    let mut db = Daemon::events_only(&b);
    wait_file(&db, b.path("d/f"), b"deleted elsewhere");
    std::fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    wait_gone(&db, b.path("d"));

    let ask = |request: &str| -> serde_json::Value {
        let run = b.cairn(&["ask", request]).expect_ok();
        serde_json::from_str(run.stdout.trim()).unwrap_or_else(|e| panic!("{e}: {}", run.text()))
    };
    let first = ask("deleted");
    assert_eq!(first["reply"], "deleted", "{first}");
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{first}");
    assert_eq!(items[0]["path"], "d");
    assert_eq!(items[0]["device_name"], "a");
    assert_eq!(items[0]["files"], 1);

    db.kill();
    let db = Daemon::events_only(&b);
    // The file the killed daemon left says synced too: the new one's answer is what tells.
    let pid = db.child.id();
    wait_for("the new daemon to answer", &db.log, || {
        let run = b.cairn(&["ask", "status"]);
        run.ok()
            && serde_json::from_str::<serde_json::Value>(run.stdout.trim())
                .is_ok_and(|v| v["pid"] == pid && v["state"] == "synced")
    });
    let again = ask(r#"{"op":"deleted","days":7}"#);
    assert_eq!(again["items"], first["items"], "the same after a restart");

    let id = items[0]["id"].as_str().unwrap();
    let done = ask(&format!(r#"{{"op":"undelete","ids":["{id}"]}}"#));
    assert_eq!(done["reply"], "undeleted", "{done}");
    assert_eq!(done["items"][0]["path"], "d");
    assert_eq!(done["items"][0]["renamed"], false);
    assert_eq!(done["seqs"].as_array().unwrap().len(), 1);
    wait_file(&db, b.path("d/f"), b"deleted elsewhere");
    assert!(
        wait_log(&db, "brought back 1 deleted item(s)"),
        "{}",
        db.log()
    );
    assert_eq!(ask("deleted")["items"], serde_json::json!([]));
    a.synced();
    assert_eq!(a.read("d/f"), b"deleted elsewhere");
}

fn wait_log(d: &Daemon, text: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < SOON {
        if d.log().contains(text) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}
