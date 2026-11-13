//! A state of version 1's layout, JSON text as 0.1.8 left it, moved to this build's when a sync opens it:
//! every record meaning what it did, a kill at any point of the move leaving version 1 whole or this
//! one whole, and the devices agreeing after it as before. A full disk under it is in real_disk_full.rs.

mod common;
use cairn_engine::state::legacy;
use common::*;
use std::path::PathBuf;

const FILES: usize = 2000;

fn state_of(d: &Device) -> PathBuf {
    d.path(".cairn/state.sqlite")
}

/// Two devices on a tree of every kind of entry, agreed, and `a`'s state then made into version 1.
fn on_version_1(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..FILES {
        a.write(&format!("d{}/f{i}", i % 12), format!("file {i}"));
    }
    a.write("gone", "deleted, its node kept");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("d0/f0", a.path("link")).unwrap();
        std::fs::hard_link(a.path("d1/f1"), a.path("hard")).unwrap();
        chmod(&a.path("d2/f2"), 0o600);
    }
    a.synced();
    std::fs::remove_file(a.path("gone")).unwrap();
    a.synced();
    b.synced();
    a.synced();
    let state = state_of(&a);
    legacy::to_v1(&state).unwrap();
    assert_eq!(legacy::version(&state).unwrap(), 1);
    (sb, server, a, b)
}

/// A sync on a state of version 1 moves it, and every node, row, manifest, queued commit, intent and
/// key means what it did; the next sync has nothing to move, and the devices agree as before.
#[test]
fn a_state_of_version_1_is_moved_by_the_next_sync_and_holds_the_same() {
    let (sb, _s, a, b) = on_version_1("migrate");
    let state = state_of(&a);
    let held = legacy::contents(&state).unwrap();
    assert!(held.rows.len() > FILES, "{} rows", held.rows.len());
    let steps = sb.path("steps");
    let run = a
        .sync_env(&[("CAIRN_FP_LOG", steps.to_str().unwrap())])
        .expect_ok();
    let log = std::fs::read_to_string(&steps).unwrap_or_default();
    assert!(log.lines().any(|l| l == "state.migrate.committed"), "{log}");
    assert!(run.stdout.contains("synced at sequence"), "{}", run.text());
    assert!(
        !run.stderr.contains("state"),
        "a move says nothing when it is done: {}",
        run.text()
    );
    assert_eq!(legacy::version(&state).unwrap(), 2);
    assert!(
        legacy::contents(&state).unwrap() == held,
        "the move and a sync with nothing to do changed what the state holds"
    );
    std::fs::remove_file(&steps).unwrap();
    a.sync_env(&[("CAIRN_FP_LOG", steps.to_str().unwrap())])
        .expect_ok();
    let log = std::fs::read_to_string(&steps).unwrap_or_default();
    assert!(!log.contains("state.migrate"), "moved again: {log}");
    a.write("d0/new", "after the move");
    b.write("d1/f1", "edited on b");
    let p = converge(&[&a, &b]);
    assert!(
        p.names().iter().any(|n| n.ends_with("d0/new")),
        "{:?}",
        p.names()
    );
    assert_eq!(a.read("d1/f1"), b"edited on b");
    #[cfg(unix)]
    assert_eq!(a.read("hard"), b"edited on b");
}

/// SIGKILL at points all the way through the move, with a page cache small enough that it spills
/// into the WAL before the commit: each leaves version 1 as it was, decoded and table for table, until
/// the commit, after which the state is version 2 holding the same. Then the next sync goes on as if
/// nothing had happened, and the devices agree.
#[test]
fn a_kill_anywhere_in_the_move_leaves_one_layout_or_the_other_whole() {
    let (_sb, _s, a, b) = on_version_1("migrate-killed");
    let state = state_of(&a);
    let held = legacy::contents(&state).unwrap();
    let raw = state_everything(&state);
    let spill = ("CAIRN_FP_TUNE", "state.cache_kib=64");
    for point in [
        "state.migrate.due",
        "state.migrate.begun",
        "state.migrate.objects#1",
        "state.migrate.objects#2",
        "state.migrate.manifests#1",
        "state.migrate.manifests#2",
        "state.migrate.commit",
    ] {
        let run = a.sync_env(&[spill, ("CAIRN_FP_CRASH", point)]);
        assert!(run.was_killed(), "{point}: never reached: {}", run.text());
        if point == "state.migrate.commit" {
            assert!(
                wal_len(&state) > 0,
                "nothing of the move was in the WAL when it was killed"
            );
        }
        assert_eq!(legacy::version(&state).unwrap(), 1, "{point}");
        same_state(&state_everything(&state), &raw, point);
        assert!(legacy::contents(&state).unwrap() == held, "{point}");
    }
    let run = a.sync_env(&[spill, ("CAIRN_FP_CRASH", "state.migrate.committed")]);
    assert!(run.was_killed(), "never committed: {}", run.text());
    assert_eq!(legacy::version(&state).unwrap(), 2);
    assert!(
        legacy::contents(&state).unwrap() == held,
        "moved, then killed: not what it held"
    );
    a.synced();
    a.write("d3/after", "after the kills");
    b.write("d4/f4", "edited on b");
    converge(&[&a, &b]);
    assert_eq!(b.read("d3/after"), b"after the kills");
    assert_eq!(a.read("d4/f4"), b"edited on b");
}

/// A layout newer than this build stops the sync with the reason, and nothing in the state changes.
#[test]
fn a_state_newer_than_this_build_is_refused_and_left_as_it_is() {
    let sb = Sandbox::on_disk("migrate-newer");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    a.write("f", "x");
    a.synced();
    let state = state_of(&a);
    {
        let db = rusqlite::Connection::open(&state).unwrap();
        db.pragma_update(None, "user_version", 99).unwrap();
    }
    let raw = state_everything(&state);
    let run = a.sync();
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr
            .contains("newer than this binary: upgrade cairn-sync"),
        "{}",
        run.text()
    );
    same_state(&state_everything(&state), &raw, "refused");
}
