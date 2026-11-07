//! The state rewritten without its free pages once a sync has left over half of it free, as a join or a
//! big delete does: everything in it kept, the sync's result as it was, and a kill at any point of the
//! rewrite the crash the next run recovers from. A full disk under it is in real_disk_full.rs.

mod common;
use common::*;
use rusqlite::Connection;
use std::path::PathBuf;

/// The floor out of the way: a test's tree is far smaller than any worth compacting otherwise.
const ANY_SIZE: (&str, &str) = ("CAIRN_FP_TUNE", "state.compact_floor=0");
const FILES: usize = 2000;
const KEPT: usize = 200;

fn state_of(d: &Device) -> PathBuf {
    d.path(".cairn/state.sqlite")
}

/// Two devices on a tree of `FILES` files, all but `KEPT` of them then deleted on `a` and the delete
/// taken in by both, so that most of each state's file is free pages; the floor keeps it as it is.
fn mostly_free(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..FILES {
        a.write(&format!("d{}/f{i}", i % 20), format!("file {i}"));
    }
    a.synced();
    b.synced();
    for i in KEPT..FILES {
        remove_file(&a.path(&format!("d{}/f{i}", i % 20)));
    }
    a.cairn(&["sync", "--allow-mass-delete"]).expect_ok();
    a.synced();
    b.synced();
    let (count, free) = state_pages(&state_of(&a));
    assert!(free * 2 > count, "not mostly free: {free} of {count} pages");
    (sb, server, a, b)
}

/// The floor: a state a few MiB at most is never worth a rewrite, however much of it is free.
#[test]
fn a_small_state_is_left_with_its_free_pages() {
    let (_sb, _s, a, _b) = mostly_free("compact-floor");
    a.synced();
    let (count, free) = state_pages(&state_of(&a));
    assert!(free * 2 > count, "compacted: {free} of {count} pages free");
}

/// Rewritten after the sync is reported, everything in it as it was, and the next sync with nothing to
/// do still finds that it has nothing to do.
#[test]
fn a_sync_that_leaves_the_state_mostly_free_compacts_it_and_keeps_all_of_it() {
    let (sb, _s, a, b) = mostly_free("compact");
    let state = state_of(&a);
    let (count, free) = state_pages(&state);
    let kept = state_everything(&state);
    let tree = Picture::of(&a.root);

    let run = a.sync_env(&[ANY_SIZE]).expect_ok();
    assert!(run.stdout.contains("synced at sequence"), "{}", run.text());
    assert!(
        !run.stderr.contains("compact"),
        "a compaction says nothing when it is done: {}",
        run.text()
    );
    let (now, now_free) = state_pages(&state);
    assert_eq!(now_free, 0);
    assert!(
        now <= count - free + 8,
        "{now} pages, where {} were in use",
        count - free
    );
    assert_eq!(wal_len(&state), 0, "the WAL is emptied");
    same_state(&state_everything(&state), &kept, "after the compaction");

    let steps = sb.path("steps");
    let again = a
        .sync_env(&[ANY_SIZE, ("CAIRN_FP_LOG", steps.to_str().unwrap())])
        .expect_ok();
    let log = std::fs::read_to_string(&steps).unwrap_or_default();
    assert!(
        log.lines().any(|l| l == "sync.fast"),
        "the next sync took the long way: {}",
        again.text()
    );
    assert!(!log.contains("state.compact"), "compacted again: {log}");
    let p = converge(&[&a, &b]);
    assert_eq!(p.names(), tree.names());
}

/// SIGKILL at points all the way through the rewrite, after it and before the WAL is emptied: each
/// leaves every table as it was, and the next sync recovers it and goes on as if nothing had happened.
#[test]
fn a_kill_anywhere_in_a_compaction_leaves_the_state_whole() {
    let (sb, _s, a, b) = mostly_free("compact-killed");
    let state = state_of(&a);
    let tree = Picture::of(&a.root);

    // How many of SQLite's steps the rewrite takes, by its progress handler on a copy, which the
    // compaction hits a point on every thousand of.
    let copy = sb.path("copy.sqlite");
    {
        let db = Connection::open(&state).unwrap();
        db.execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
            .unwrap();
    }
    let steps = {
        let db = Connection::open(&copy).unwrap();
        let n = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = n.clone();
        db.progress_handler(
            1000,
            Some(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                false
            }),
        );
        db.execute_batch("VACUUM").unwrap();
        n.load(std::sync::atomic::Ordering::SeqCst)
    };
    assert!(steps >= 8, "too short a rewrite to kill it in: {steps}");

    let mut points: Vec<String> = [1, steps / 4, steps / 2, steps * 3 / 4]
        .into_iter()
        .map(|k| format!("state.compact.vacuuming#{}", k.max(1)))
        .collect();
    // The rewrite committed, the WAL not yet emptied: from here the state is the compacted one.
    points.push("state.compact.vacuumed".into());
    for point in &points {
        let gate = sb.path("gate");
        std::fs::create_dir(&gate).unwrap();
        let spec = format!("state.compact.due:{}", gate.display());
        let device = a.clone();
        let kill = point.clone();
        let worker = std::thread::spawn(move || {
            device.sync_env(&[
                ANY_SIZE,
                ("CAIRN_FP_PAUSE", &spec),
                ("CAIRN_FP_CRASH", &kill),
            ])
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !gate.join("reached").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "{point}: no compaction began"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The state as the sync left it, the moment before the rewrite.
        let kept = state_everything(&state);
        std::fs::write(gate.join("go"), b"").unwrap();
        let run = worker.join().unwrap();
        std::fs::remove_dir_all(&gate).unwrap();
        assert!(run.was_killed(), "{point}: never reached: {}", run.text());
        assert!(
            run.stdout.contains("synced at sequence"),
            "{point}: the sync was not reported before the compaction: {}",
            run.text()
        );
        same_state(&state_everything(&state), &kept, point);
    }
    // The last kill came once the rewrite had committed: nothing is free now, and nothing more to do.
    assert_eq!(state_pages(&state).1, 0);
    let steps_log = sb.path("steps");
    a.sync_env(&[ANY_SIZE, ("CAIRN_FP_LOG", steps_log.to_str().unwrap())])
        .expect_ok();
    let log = std::fs::read_to_string(&steps_log).unwrap_or_default();
    assert!(!log.contains("state.compact"), "compacted again: {log}");
    let p = converge(&[&a, &b]);
    assert_eq!(p.names(), tree.names());
}
