//! Interleavings the matrices cannot reach: a user or another device acting in the gaps of one sync,
//! or between a crash and its recovery. Each of these lost data or metadata before it was fixed.

mod common;
use common::*;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

/// Run a sync on a thread, paused at a failpoint; `during` acts in the gap.
fn sync_with_gap(sb: &Sandbox, d: &Device, point: &str, during: impl FnOnce()) -> Run {
    let gate: PathBuf = sb.path(&format!("gate-{:x}", rand::random::<u32>()));
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("{point}:{}", gate.display());
    let device = d.clone();
    let worker = std::thread::spawn(move || device.sync_env(&[("CAIRN_FP_PAUSE", &pause)]));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !gate.join("reached").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the sync never reached {point}"
        );
        assert!(
            !worker.is_finished(),
            "the sync ended before reaching {point}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    during();
    std::fs::write(gate.join("go"), b"").unwrap();
    worker.join().unwrap()
}

fn contents(p: &Picture) -> Vec<Vec<u8>> {
    p.entries
        .values()
        .filter(|e| e.kind == 'f')
        .map(|e| e.content.clone())
        .collect()
}

#[track_caller]
fn has_version(p: &Picture, version: &[u8]) {
    assert!(
        contents(p).iter().any(|c| c == version),
        "lost {:?}; tree: {:?}",
        String::from_utf8_lossy(version),
        p.names()
    );
}

#[test]
fn an_edit_right_after_a_download_is_not_recorded_as_that_download() {
    let (sb, _s, a, b) = pair("edit-after-download");
    b.write("f", "from b");
    b.write("g", "from b too");
    b.synced();
    let _ = sync_with_gap(&sb, &a, "apply.create.renamed", || {
        // Whichever arrived first is edited while the other is still coming.
        let name = if a.exists("f") { "f" } else { "g" };
        std::fs::write(a.path(name), "edited on a").unwrap();
    });
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited on a");
}

#[test]
fn a_delete_that_has_not_seen_an_edit_does_not_remove_it() {
    let (_sb, _s, a, b) = pair("stale-delete");
    a.write("f", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    // a decides to delete and queues it, and dies before the server hears.
    std::fs::remove_file(a.path("f")).unwrap();
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.enqueued")]);
    assert!(run.was_killed(), "{}", run.text());
    // b edits meanwhile and finishes completely.
    b.write("f", "edited on b");
    b.synced();
    // a comes back: its queued delete must not win.
    a.synced();
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited on b");
}

#[test]
fn a_file_that_could_not_be_written_is_not_recorded_as_written() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("not-written");
    a.write("x", "base");
    a.write("other", "o");
    a.synced();
    b.synced();

    chmod(&a.path("x"), 0o000);
    b.write("x", "from b");
    b.synced();
    // a has other work too, so a commit goes out while x could not be touched.
    a.write("other", "changed");
    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());

    chmod(&a.path("x"), 0o644);
    a.write("x", "from a");
    a.synced();
    let p = converge(&[&a, &b]);
    has_version(&p, b"from b");
    has_version(&p, b"from a");
}

#[test]
fn a_local_edit_during_an_apply_that_also_links_is_not_overwritten() {
    let (sb, _s, a, b) = pair("link-gap");
    a.write("f", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    b.write("f", "from b");
    std::fs::hard_link(b.path("f"), b.path("f-link")).unwrap();
    b.synced();
    let _ = sync_with_gap(&sb, &a, "apply.prefetched", || {
        std::fs::write(a.path("f"), "edited on a in the gap").unwrap();
    });
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited on a in the gap");
    has_version(&p, b"from b");
}

#[test]
fn what_cannot_be_read_right_after_a_push_does_not_revert_it() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("carried-revert");
    a.write("f", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    a.write("f", "pushed");
    // The commit lands and a dies before its base catches up.
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.committed")]);
    assert!(run.was_killed(), "{}", run.text());
    b.synced();
    assert_eq!(b.read("f"), b"pushed");

    chmod(&a.path("f"), 0o000);
    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());
    b.synced();
    assert_eq!(b.read("f"), b"pushed", "the server was reverted");

    chmod(&a.path("f"), 0o644);
    let p = converge(&[&a, &b]);
    has_version(&p, b"pushed");
    assert_eq!(a.read("f"), b"pushed");
}

#[test]
fn a_remote_edit_is_not_reverted_by_a_file_being_written_here() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("carried-remote");
    a.write("f", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    b.write("f", "from b");
    b.synced();
    a.synced();
    assert_eq!(a.read("f"), b"from b");
    b.write("f", "from b again");
    b.synced();
    chmod(&a.path("f"), 0o000);
    assert_eq!(a.sync().code, Some(3));
    b.synced();
    assert_eq!(b.read("f"), b"from b again");
    chmod(&a.path("f"), 0o644);
    let p = converge(&[&a, &b]);
    has_version(&p, b"from b again");
}

#[test]
fn a_moved_file_that_cannot_be_read_is_not_a_delete() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("moved-unreadable");
    a.write("f", "content");
    a.write("keep", "k");
    a.synced();
    b.synced();
    std::fs::rename(a.path("f"), a.path("g")).unwrap();
    chmod(&a.path("g"), 0o000);
    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());
    assert!(
        run.stdout.contains("waiting for permission"),
        "{}",
        run.text()
    );
    b.synced();
    assert_eq!(b.read("f"), b"content", "another device lost it");

    chmod(&a.path("g"), 0o644);
    let p = converge(&[&a, &b]);
    has_version(&p, b"content");
    assert_eq!(a.read("g"), b"content");
    assert!(
        !a.exists("f"),
        "the move went through once it could be seen"
    );
}

#[test]
fn a_tree_pushed_and_emptied_before_finishing_still_pauses() {
    let (_sb, _s, a, b) = pair("guard-pushed");
    for i in 0..60 {
        a.write(&format!("d/{i}"), format!("{i}"));
    }
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.committed")]);
    assert!(run.was_killed(), "{}", run.text());
    std::fs::remove_dir_all(a.path("d")).unwrap();
    let run = a.sync();
    assert_eq!(run.code, Some(2), "{}", run.text());
    b.synced();
    assert!(b.exists("d/30"));
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn a_server_restored_from_backup_is_noticed_and_rejoining_loses_nothing() {
    let (sb, mut server, a, b) = pair("rollback");
    a.write("old", "from before the backup");
    a.synced();
    b.synced();

    // A backup, then more history on top of it.
    let backup = sb.path("backup");
    server.kill();
    copy_tree(&server.data, &backup);
    server.restart();
    a.write("new", "only after the backup");
    a.synced();
    b.synced();

    server.kill();
    std::fs::remove_dir_all(&server.data).unwrap();
    copy_tree(&backup, &server.data);
    server.restart();

    // Behind the devices: both stop rather than believe it.
    let run = a.sync();
    assert_eq!(run.code, Some(2), "{}", run.text());
    assert!(
        run.stdout.contains("restored from a backup"),
        "{}",
        run.text()
    );
    assert_eq!(
        a.read("new"),
        b"only after the backup",
        "nothing changed here"
    );

    // A device that went through it from scratch reaches a different entry at the same number.
    let c = Device::new(&sb, &server, "c");
    c.write("from-c", "a new line of history");
    c.synced();
    let run = b.sync();
    assert_eq!(run.code, Some(2), "{}", run.text());

    // The way back that the message gives: nothing is lost, and the server gets back what it lost.
    std::fs::remove_dir_all(a.path(".cairn")).unwrap();
    run_init(&server, &a);
    let p = converge(&[&a, &c]);
    has_version(&p, b"only after the backup");
    has_version(&p, b"from before the backup");
    has_version(&p, b"a new line of history");
}

fn run_init(server: &Server, d: &Device) {
    let token = server.token_file.to_str().unwrap().to_string();
    run(
        &[
            "init",
            "--server",
            &server.url,
            "--device-name",
            &d.name,
            "--token-file",
            &token,
            d.root.to_str().unwrap(),
        ],
        &[],
    )
    .expect_ok();
}

#[test]
fn a_crash_while_updating_hard_links_does_not_split_them() {
    let (_sb, _s, a, b) = pair("relink");
    a.write("one", "base");
    std::fs::hard_link(a.path("one"), a.path("two")).unwrap();
    std::fs::hard_link(a.path("one"), a.path("three")).unwrap();
    a.write("keep", "k");
    a.synced();
    b.synced();
    b.write("one", "from b");
    b.synced();

    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "apply.replace.relink")]);
    assert!(run.was_killed(), "{}", run.text());
    a.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[&b"two".to_vec()].content, b"from b");
    for d in [&a, &b] {
        let m = std::fs::metadata(d.path("one")).unwrap();
        assert_eq!(m.nlink(), 3, "the links split on {}", d.name);
        assert_eq!(m.ino(), std::fs::metadata(d.path("three")).unwrap().ino());
    }
}

#[test]
fn an_edit_to_a_name_left_behind_by_a_crash_survives_recovery() {
    let (_sb, _s, a, b) = pair("relink-edit");
    a.write("one", "base");
    std::fs::hard_link(a.path("one"), a.path("two")).unwrap();
    a.synced();
    b.synced();
    b.write("one", "from b");
    b.synced();
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "apply.replace.relink")]);
    assert!(run.was_killed(), "{}", run.text());
    // "two" still holds the old file, and now someone writes to it.
    std::fs::write(a.path("two"), "edited after the crash").unwrap();
    a.synced();
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited after the crash");
    has_version(&p, b"from b");
}

#[test]
fn a_rejoining_copy_with_other_metadata_leaves_a_log_entry() {
    let (sb, s, a, _b) = pair("adopt-note");
    a.write("f", "same");
    a.synced();
    let c = sb.path("c");
    std::fs::create_dir(&c).unwrap();
    std::fs::write(c.join("f"), "same").unwrap();
    chmod(&c.join("f"), 0o750);
    let d = Device {
        root: c,
        name: "c".into(),
    };
    run_init(&s, &d);
    let run = d.synced();
    assert!(run.stdout.contains("this device had 750"), "{}", run.text());
    let log = d.cairn(&["log"]).expect_ok();
    assert!(log.stdout.contains("note: mode"), "{}", log.text());
}

#[test]
fn the_loser_of_a_metadata_conflict_is_in_the_log_with_nothing_else_to_push() {
    let (_sb, _s, a, b) = pair("note");
    a.write("f", "x");
    a.synced();
    b.synced();
    chmod(&a.path("f"), 0o600);
    chmod(&b.path("f"), 0o640);
    a.synced();
    b.synced();
    let log = a.cairn(&["log"]).expect_ok();
    assert!(log.stdout.contains("note: mode"), "{}", log.text());
    converge(&[&a, &b]);
}

#[test]
fn restoring_a_directory_leaves_a_hard_link_outside_it() {
    let (_sb, _s, a, _b) = pair("restore-link");
    a.write("d/old", "o");
    a.write("other/x", "shared");
    let first = a.synced();
    let then: u64 = first
        .stdout
        .lines()
        .find(|l| l.starts_with("synced"))
        .unwrap()
        .split_whitespace()
        .nth(3)
        .unwrap()
        .trim_end_matches(':')
        .parse()
        .unwrap();
    std::fs::hard_link(a.path("other/x"), a.path("d/x")).unwrap();
    a.synced();
    a.cairn(&["restore", "d", "--at", &then.to_string()])
        .expect_ok();
    assert!(!a.exists("d/x"), "came later, inside the restored path");
    assert_eq!(a.read("other/x"), b"shared", "outside the restored path");
}

/// `a` has pushed a new file and died before its base caught up, and edits it again.
fn pushed_then_edited(a: &Device) {
    a.write("x", "pushed");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.committed")]);
    assert!(run.was_killed(), "{}", run.text());
    a.write("x", "edited after the push");
}

/// Found by review of an earlier fix: "synced" was recorded before the server answered, so a commit
/// that was then refused made the edit it carried look like the common ancestor.
#[test]
fn a_refused_commit_does_not_make_its_edit_look_already_shared() {
    let (sb, _s, a, b) = pair("refused");
    pushed_then_edited(&a);
    // Something unrelated from b gets in between a's look at the server and its commit.
    let _ = sync_with_gap(&sb, &a, "sync.uploaded", || {
        b.write("x", "from b");
        b.synced();
    });
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited after the push");
    has_version(&p, b"from b");
}

#[test]
fn a_commit_refused_over_an_unrelated_name_keeps_the_edit() {
    let (sb, _s, a, b) = pair("refused-name");
    pushed_then_edited(&a);
    a.write("taken", "a's");
    let _ = sync_with_gap(&sb, &a, "sync.uploaded", || {
        b.write("taken", "b's");
        b.synced();
    });
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited after the push");
    has_version(&p, b"a's");
    has_version(&p, b"b's");
}

#[test]
fn a_commit_refused_because_the_directory_moved_on_keeps_the_edit() {
    let (sb, _s, a, b) = pair("refused-dir");
    pushed_then_edited(&a);
    a.write("mine", "m");
    let _ = sync_with_gap(&sb, &a, "sync.uploaded", || {
        b.write("unrelated", "u");
        b.synced();
    });
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited after the push");
    assert_eq!(a.read("x"), b"edited after the push");
    assert_eq!(b.read("x"), b"edited after the push");
}

#[test]
fn a_refused_commit_replayed_from_the_queue_keeps_the_edit() {
    let (_sb, _s, a, b) = pair("refused-replay");
    pushed_then_edited(&a);
    a.write("mine", "m");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.enqueued")]);
    assert!(run.was_killed(), "{}", run.text());
    b.write("x", "from b");
    b.write("unrelated", "u");
    b.synced();
    a.synced();
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited after the push");
}

#[test]
fn an_edit_that_has_not_seen_a_delete_survives_it() {
    let (sb, _s, a, b) = pair("edit-after-delete");
    a.write("x", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    b.write("x", "edited on b");
    let _ = sync_with_gap(&sb, &b, "sync.uploaded", || {
        std::fs::remove_file(a.path("x")).unwrap();
        a.synced();
    });
    let p = converge(&[&a, &b]);
    has_version(&p, b"edited on b");
}

#[test]
fn children_of_a_pushed_directory_that_cannot_be_read_are_not_deleted() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("unlisted-pushed");
    a.write("d/one", "1");
    a.write("d/two", "2");
    a.write("keep", "k");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.committed")]);
    assert!(run.was_killed(), "{}", run.text());
    chmod(&a.path("d"), 0o000);
    assert_eq!(a.sync().code, Some(3));
    b.synced();
    assert_eq!(b.read("d/one"), b"1", "another device lost it");
    assert_eq!(b.read("d/two"), b"2", "another device lost it");
    chmod(&a.path("d"), 0o755);
    converge(&[&a, &b]);
}

#[test]
fn a_crash_while_updating_a_hard_link_in_a_read_only_directory_does_not_split_it() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("relink-readonly");
    a.write("one", "base");
    a.write("ro/keep", "k");
    std::fs::hard_link(a.path("one"), a.path("ro/two")).unwrap();
    chmod(&a.path("ro"), 0o555);
    a.synced();
    b.synced();
    b.write("one", "from b");
    b.synced();
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "apply.replace.renamed")]);
    assert!(run.was_killed(), "{}", run.text());
    a.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[&b"ro/two".to_vec()].content, b"from b");
    for d in [&a, &b] {
        let one = std::fs::metadata(d.path("one")).unwrap();
        let two = std::fs::metadata(d.path("ro/two")).unwrap();
        assert_eq!(one.ino(), two.ino(), "the link split on {}", d.name);
        assert_eq!(two.nlink(), 2);
    }
    assert_eq!(
        std::fs::metadata(a.path("ro")).unwrap().mode() & 0o777,
        0o555
    );
}

#[test]
fn a_damaged_chunk_on_the_server_is_replaced_by_the_next_upload_of_it() {
    let (_sb, mut server, a, _b) = pair("damaged");
    a.write("f", "the same content");
    a.synced();
    server.kill();
    // Damage it in place, keeping the length.
    let mut damaged = 0;
    let mut stack = vec![server.data.join("ns")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else if std::fs::read(e.path()).unwrap() == b"the same content" {
                std::fs::write(e.path(), b"THE same content").unwrap();
                damaged += 1;
            }
        }
    }
    assert_eq!(damaged, 1);
    server.restart();
    a.write("g", "the same content");
    a.synced();
    let report = server.fsck();
    assert!(report.contains("clean"), "{report}");
}

#[test]
fn a_new_directory_that_cannot_be_read_waits_without_blocking_anything_else() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("new-unreadable");
    a.write("new-dir/inside", "x");
    chmod(&a.path("new-dir"), 0o000);
    a.write("other", "o");
    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());
    assert!(
        run.stdout.contains("waiting for permission"),
        "{}",
        run.text()
    );
    b.synced();
    assert_eq!(b.read("other"), b"o", "unrelated files go through");
    assert!(!b.exists("new-dir"), "not pushed half-described");
    chmod(&a.path("new-dir"), 0o755);
    a.synced();
    b.synced();
    assert_eq!(b.read("new-dir/inside"), b"x");
}

#[test]
fn hard_links_across_commits_of_a_big_change_all_stay() {
    let sb = Sandbox::fast("big-links");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..7_000 {
        if i % 500 == 0 {
            std::fs::create_dir(a.path(&format!("d{}", i / 500))).unwrap();
        }
        let name = format!("d{}/{i}", i / 500);
        std::fs::write(a.path(&name), i.to_string()).unwrap();
        std::fs::hard_link(a.path(&name), a.path(&format!("{name}-two"))).unwrap();
        std::fs::hard_link(a.path(&name), a.path(&format!("{name}-three"))).unwrap();
    }
    let run = a.synced();
    assert!(run.stdout.contains(" 2 commits"), "{}", run.text());
    b.synced();
    for d in [&a, &b] {
        for i in 0..7_000 {
            let name = format!("d{}/{i}", i / 500);
            assert_eq!(
                std::fs::metadata(d.path(&name)).unwrap().nlink(),
                3,
                "{name} on {}",
                d.name
            );
        }
    }
    converge(&[&a, &b]);
}

/// Swapped after the scan and before apply opens anything: the open itself must refuse the link.
#[test]
fn a_parent_swapped_for_a_symlink_before_apply_opens_it_does_not_redirect_it() {
    let (sb, _s, a, b) = pair("symlink-before-open");
    let outside = sb.path("outside");
    std::fs::create_dir(&outside).unwrap();
    a.write("d/f", "base");
    a.write("d/deep/g", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    b.write("d/new", "from remote");
    b.write("d/f", "changed remotely");
    b.write("d/deep/g", "changed remotely");
    std::fs::remove_file(b.path("keep")).unwrap();
    b.synced();

    let moved = sb.path("moved-away");
    let _ = sync_with_gap(&sb, &a, "apply.prefetched", || {
        std::fs::rename(a.path("d"), &moved).unwrap();
        std::os::unix::fs::symlink(&outside, a.path("d")).unwrap();
    });
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "something arrived outside the root"
    );

    std::fs::remove_file(a.path("d")).unwrap();
    std::fs::rename(&moved, a.path("d")).unwrap();
    let p = converge(&[&a, &b]);
    has_version(&p, b"from remote");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

/// Found by review: a file this device pushed but could not read stood for the server's state, so
/// the base moved past what the disk held, and the old file then read as an edit on top of it.
#[test]
fn the_base_does_not_move_past_what_could_not_be_read() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("base-past-disk");
    a.write("x", "pushed");
    a.write("keep", "k");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.committed")]);
    assert!(run.was_killed(), "{}", run.text());
    b.synced();
    b.write("x", "from b");
    b.synced();

    chmod(&a.path("x"), 0o000);
    assert_eq!(a.sync().code, Some(3));
    chmod(&a.path("x"), 0o644);
    a.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(a.read("x"), b"from b", "the remote edit was reverted");
    has_version(&p, b"from b");
}

#[test]
fn the_base_does_not_move_past_what_could_not_be_read_with_an_edit_too() {
    if is_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("base-past-disk-edit");
    a.write("x", "pushed");
    a.write("keep", "k");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.committed")]);
    assert!(run.was_killed(), "{}", run.text());
    b.synced();
    b.write("x", "from b");
    b.synced();

    chmod(&a.path("x"), 0o000);
    assert_eq!(a.sync().code, Some(3));
    chmod(&a.path("x"), 0o644);
    a.write("x", "from a");
    a.synced();
    let p = converge(&[&a, &b]);
    has_version(&p, b"from b");
    has_version(&p, b"from a");
}

#[test]
fn an_edit_between_the_last_look_and_the_replacement_survives() {
    let (sb, _s, a, b) = pair("gap-replace");
    a.write("f", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    b.write("f", "from b");
    b.synced();
    let _ = sync_with_gap(&sb, &a, "apply.replace.before_rename", || {
        std::fs::write(a.path("f"), "typed in the gap").unwrap();
    });
    let p = converge(&[&a, &b]);
    has_version(&p, b"typed in the gap");
    has_version(&p, b"from b");
}

#[test]
fn an_edit_between_the_last_look_and_the_delete_survives() {
    let (sb, _s, a, b) = pair("gap-delete");
    a.write("f", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    std::fs::remove_file(b.path("f")).unwrap();
    b.synced();
    let _ = sync_with_gap(&sb, &a, "apply.unlink.before_aside", || {
        std::fs::write(a.path("f"), "typed in the gap").unwrap();
    });
    let p = converge(&[&a, &b]);
    has_version(&p, b"typed in the gap");
}

#[test]
fn a_same_size_same_mtime_edit_survives_relink_recovery() {
    let (_sb, _s, a, b) = pair("relink-sneaky");
    a.write("one", "base");
    std::fs::hard_link(a.path("one"), a.path("two")).unwrap();
    a.write("keep", "k");
    a.synced();
    b.synced();
    b.write("one", "from b");
    b.synced();
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "apply.replace.relink")]);
    assert!(run.was_killed(), "{}", run.text());
    let meta = std::fs::symlink_metadata(a.path("two")).unwrap();
    std::fs::write(a.path("two"), "BASE").unwrap();
    set_mtime(&a.path("two"), meta.mtime(), meta.mtime_nsec());
    a.synced();
    let p = converge(&[&a, &b]);
    has_version(&p, b"BASE");
    has_version(&p, b"from b");
}

#[test]
fn a_directory_moved_on_both_sides_before_finishing_does_not_jam_pushes() {
    let (_sb, _s, a, b) = pair("dir-two-links");
    a.write("d/f", "x");
    a.write("keep", "k");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.committed")]);
    assert!(run.was_killed(), "{}", run.text());
    b.synced();
    std::fs::rename(b.path("d"), b.path("on-b")).unwrap();
    chmod(&b.path("on-b"), 0o700);
    b.synced();
    std::fs::rename(a.path("d"), a.path("on-a")).unwrap();
    a.write("later", "from a");
    a.synced();
    let p = converge(&[&a, &b]);
    has_version(&p, b"from a");
    has_version(&p, b"x");
}

#[test]
fn a_fifo_in_the_way_does_not_hold_back_everything_else() {
    let (_sb, _s, a, b) = pair("fifo-blocks");
    a.write("keep", "k");
    a.synced();
    b.synced();
    b.write("taken", "from b");
    b.synced();
    assert!(std::process::Command::new("mkfifo")
        .arg(a.path("taken"))
        .status()
        .unwrap()
        .success());
    a.write("other", "from a");
    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());
    assert!(
        run.stdout.contains("destination cannot represent"),
        "{}",
        run.text()
    );
    b.synced();
    assert_eq!(
        b.read("other"),
        b"from a",
        "the rest of the tree went through"
    );
    std::fs::remove_file(a.path("taken")).unwrap();
    a.synced();
    assert_eq!(a.read("taken"), b"from b");
    converge(&[&a, &b]);
}

#[test]
fn a_conflict_on_a_name_with_a_very_long_extension_keeps_both() {
    let (_sb, _s, a, b) = pair("long-ext");
    let name = format!("x.{}", "e".repeat(250));
    a.write(&name, "base");
    a.synced();
    b.synced();
    a.write(&name, "from a");
    b.write(&name, "from b");
    a.synced();
    b.synced();
    let p = converge(&[&a, &b]);
    has_version(&p, b"from a");
    has_version(&p, b"from b");
}

/// Invariant 4: what was queued goes out, even if the file has moved on by the next run. Merging
/// would reach the same tree without it, but that version would be missing from history.
#[test]
fn a_queued_commit_reaches_history_even_if_the_file_moved_on() {
    let (_sb, _s, a, _b) = pair("queued");
    a.write("f", "first");
    a.synced();
    a.write("f", "queued version");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.enqueued")]);
    assert!(run.was_killed(), "{}", run.text());
    a.write("f", "later version");
    a.synced();
    let history = a.cairn(&["history", "f"]).expect_ok();
    assert_eq!(history.stdout.lines().count(), 3, "{}", history.text());
}

#[test]
fn a_change_too_big_for_one_commit_goes_in_several() {
    let sb = Sandbox::fast("big-change");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..21_000 {
        if i % 1000 == 0 {
            std::fs::create_dir(a.path(&format!("d{}", i / 1000))).unwrap();
        }
        std::fs::write(a.path(&format!("d{}/{i}", i / 1000)), i.to_string()).unwrap();
    }
    let run = a.synced();
    assert!(run.stdout.contains(" 2 commits"), "{}", run.text());
    b.synced();
    assert_eq!(b.read("d20/20999"), b"20999");
}

/// Each chunk is synced to disk before the server answers: on a slow disk, one request for a tree of
/// small files outlasted the client's timeout, however few bytes it held.
#[test]
fn many_small_files_go_up_in_several_requests() {
    let sb = Sandbox::fast("many-small");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    for i in 0..1_100 {
        a.write(&format!("f{i}"), i.to_string());
    }
    let log = sb.path("steps");
    a.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
        .expect_ok();
    let steps = std::fs::read_to_string(&log).unwrap();
    let requests = steps.lines().filter(|s| *s == "sync.upload.sent").count();
    assert!(requests >= 2, "{requests} request(s)");
}

/// A conflict copy's mark lives until the next finished sync. Here the copy is gone by then, deleted on
/// both sides, and a symlink has its inode number: under the copy's identity the server refuses that
/// symlink on every round, and the sync never finishes.
#[test]
fn a_conflict_copys_inode_number_reused_by_a_symlink_is_a_new_entry() {
    let (_sb, _s, a, b) = pair("copy-ino");
    a.write("f", "base");
    a.synced();
    b.synced();
    a.write("f", "edit here");
    b.write("f", "edit there");
    b.synced();
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.committed")]);
    assert!(run.was_killed(), "{}", run.text());
    b.synced();
    let copies = b.conflict_copies("", "f");
    assert_eq!(copies.len(), 1);
    std::fs::remove_file(b.path(&copies[0])).unwrap();
    b.synced();

    let copy = a.path(&copies[0]);
    let ino = std::fs::metadata(&copy).unwrap().ino();
    std::fs::remove_file(&copy).unwrap();
    let mut made = Vec::new();
    let mut reused = false;
    for i in 0..2000 {
        let path = a.path(&format!("try-{i}"));
        std::os::unix::fs::symlink("f", &path).unwrap();
        if std::fs::symlink_metadata(&path).unwrap().ino() == ino {
            std::fs::rename(&path, a.path("link")).unwrap();
            reused = true;
            break;
        }
        made.push(path);
    }
    for path in made {
        std::fs::remove_file(path).unwrap();
    }
    if !reused {
        eprintln!("this filesystem did not reuse the inode number: nothing was tested");
        return;
    }

    a.synced();
    b.synced();
    assert_eq!(
        std::fs::read_link(b.path("link")).unwrap(),
        PathBuf::from("f")
    );
}

/// A folder deleted here while it is still arriving. What is placed is recorded only once the whole
/// run's folders are durable, so nothing proves that this device held it, and it comes down again: the
/// safe way. In the deployment 1,958 of 2,000 files came back within 39 s of `rm -rf`, and nothing
/// said so. Now the sync says so.
#[test]
fn a_folder_deleted_while_it_arrives_comes_back_and_says_so() {
    let (sb, _s, a, b) = pair("rm-arriving");
    let said = |run: &Run| -> Vec<String> {
        run.stdout
            .lines()
            .filter(|l| l.contains("removed here while it was arriving"))
            .map(|l| l.split(": removed").next().unwrap().to_string())
            .collect()
    };
    // All of it in place, then removed before it is recorded.
    for i in 0..50 {
        a.write(&format!("dir/f{i:02}"), format!("{i}"));
    }
    a.synced();
    let run = sync_with_gap(&sb, &b, "apply.created", || {
        std::fs::remove_dir_all(b.path("dir")).unwrap();
    });
    assert!(run.ok(), "{}", run.text());
    assert_eq!(
        said(&run),
        ["event: dir and 50 entries in it"],
        "{}",
        run.text()
    );

    // Removed after its first file, so the rest never got there.
    for i in 0..50 {
        a.write(&format!("dir2/f{i:02}"), format!("{i}"));
    }
    a.synced();
    let run = sync_with_gap(&sb, &b, "apply.create.renamed", || {
        std::fs::remove_dir_all(b.path("dir2")).unwrap();
    });
    assert!(run.ok(), "{}", run.text());
    assert_eq!(said(&run), ["event: dir2"], "{}", run.text());

    // Nothing was lost, and nothing went for anyone.
    let p = converge(&[&a, &b]);
    // Two folders of 50, and the root.
    assert_eq!(p.names().len(), 103, "{:?}", p.names());
}
