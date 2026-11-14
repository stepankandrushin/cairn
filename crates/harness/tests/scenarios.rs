//! The invariants and PLAN.md's merge table, end to end, with real processes.

mod common;
use common::*;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::process::Command;

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

fn seq(run: &Run) -> u64 {
    let line = run
        .stdout
        .lines()
        .find(|l| l.starts_with("synced at sequence"))
        .unwrap_or_else(|| panic!("{}", run.text()));
    line.split_whitespace()
        .nth(3)
        .unwrap()
        .trim_end_matches(':')
        .parse()
        .unwrap()
}

// --- What Nextcloud gets wrong, from the plan's table ---

#[test]
fn renaming_a_big_tree_moves_no_data() {
    let (_sb, _s, a, b) = pair("rename");
    for i in 0..50 {
        a.write(&format!("tree/sub/{i}"), format!("file {i}"));
    }
    a.synced();
    b.synced();
    std::fs::rename(a.path("tree"), a.path("renamed")).unwrap();
    let run = a.synced();
    assert!(
        run.stdout.contains("0 chunks (0 bytes) uploaded"),
        "{}",
        run.text()
    );
    b.synced();
    assert!(!b.exists("tree"));
    assert_eq!(b.read("renamed/sub/7"), b"file 7");
}

/// Two names swapped elsewhere arrive as moves, and are nobody's change here. On NTFS a file renamed
/// where another has just gone takes that one's creation time (tunneling), so the engine's own renames
/// moved birth times: read as a reused file ID, each name became a new file, and read as a change, each
/// a revision from the device that received them.
#[test]
fn a_swap_of_two_names_arrives_as_moves_and_makes_no_revision() {
    let (_sb, _s, a, b) = pair("swap-arrives");
    a.write("x", "was x");
    a.write("y", "was y");
    a.synced();
    b.synced();
    std::fs::rename(a.path("x"), a.path("t")).unwrap();
    std::fs::rename(a.path("y"), a.path("x")).unwrap();
    std::fs::rename(a.path("t"), a.path("y")).unwrap();
    let run = a.synced();
    assert!(
        run.stdout.contains("0 chunks (0 bytes) uploaded"),
        "{}",
        run.text()
    );
    for _ in 0..2 {
        let run = b.synced();
        assert!(run.stdout.contains(" 0 commits"), "{}", run.text());
    }
    assert_eq!(b.read("x"), b"was y");
    assert_eq!(b.read("y"), b"was x");
    let run = a.synced();
    assert!(
        run.stdout.contains(" 0 steps applied here, 0 commits"),
        "{}",
        run.text()
    );
}

#[test]
fn chmod_is_a_metadata_revision_with_nothing_uploaded() {
    let (_sb, _s, a, b) = pair("chmod");
    a.write("script.sh", "#!/bin/sh\n");
    a.synced();
    b.synced();
    chmod(&a.path("script.sh"), other_mode(0o755));
    let run = a.synced();
    assert!(
        run.stdout.contains("0 chunks (0 bytes) uploaded"),
        "{}",
        run.text()
    );
    b.synced();
    assert_eq!(mode_of(&b.path("script.sh")), other_mode(0o755));
}

#[test]
fn a_small_edit_in_a_big_file_moves_only_nearby_chunks() {
    let (_sb, _s, a, b) = pair("delta");
    let mut data = token::make(1, 32 * 1024 * 1024);
    a.write("big", &data);
    a.synced();
    b.synced();
    data.splice(5_000_000..5_000_000, *b"a few inserted bytes");
    a.write("big", &data);
    let run = a.synced();
    let bytes: u64 = run
        .stdout
        .lines()
        .find(|l| l.starts_with("synced"))
        .and_then(|l| l.split('(').nth(1))
        .and_then(|l| l.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{}", run.text()));
    assert!(
        bytes < 12 * 1024 * 1024,
        "{bytes} of {} bytes uploaded",
        data.len()
    );
    b.synced();
    assert_eq!(b.read("big"), data);
}

#[test]
fn nothing_is_dropped_for_its_name() {
    let (_sb, _s, a, b) = pair("names");
    for name in [
        "node_modules/x/index.js",
        ".venv/bin/python",
        ".git/config",
        "__pycache__/m.pyc",
        "build/out",
        "target/debug/x",
        "desktop.ini",
        "Thumbs.db",
        ".DS_Store",
        "x.part",
        "~lock",
    ] {
        a.write(name, name);
    }
    a.synced();
    b.synced();
    assert_eq!(
        b.read("node_modules/x/index.js"),
        b"node_modules/x/index.js"
    );
    assert_eq!(b.read(".DS_Store"), b".DS_Store");
    assert_eq!(b.read("~lock"), b"~lock");
}

// --- Merge table ---

#[test]
fn content_on_both_keeps_both_and_fills_the_inbox() {
    let (_sb, _s, a, b) = pair("conflict");
    a.write("doc.txt", "base");
    a.synced();
    b.synced();
    a.write("doc.txt", "from a");
    b.write("doc.txt", "from b");
    a.synced();
    let run = b.synced();
    assert!(run.stdout.contains("both versions kept"), "{}", run.text());
    // The server's order decides the name, and neither version is lost.
    assert_eq!(b.read("doc.txt"), b"from a");
    let copies = b.conflict_copies("", "doc");
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert!(copies[0].contains("(conflict b "), "{copies:?}");
    assert!(copies[0].ends_with(".txt"), "{copies:?}");
    assert_eq!(b.read(&copies[0]), b"from b");
    a.synced();
    assert_eq!(a.read(&copies[0]), b"from b");
    let inbox = a.cairn(&["conflicts"]).expect_ok();
    assert_eq!(inbox.stdout.lines().count(), 1, "{}", inbox.text());
    let line = format!("{}: the competing version of doc.txt", copies[0]);
    assert!(inbox.stdout.contains(&line), "{}", inbox.text());
    converge(&[&a, &b]);

    // Deleting the copy is dealing with it: the inbox no longer waits for a person.
    std::fs::remove_file(a.path(&copies[0])).unwrap();
    a.synced();
    let inbox = b.cairn(&["conflicts"]).expect_ok();
    assert!(
        inbox
            .stdout
            .starts_with("no conflict waits for a person\n1 more dealt with"),
        "{}",
        inbox.text()
    );
    let all = b.cairn(&["conflicts", "--all"]).expect_ok();
    let line = format!("dealt with: {}, deleted", copies[0]);
    assert!(all.stdout.contains(&line), "{}", all.text());
}

/// The inbox is read from this device's base and the journal since, not the whole journal: what came
/// after its last sync counts, a copy made, its folder renamed, the copy moved.
#[test]
fn the_inbox_is_the_server_s_now_however_far_behind_this_device_is() {
    let (_sb, _s, a, b) = pair("inbox-behind");
    a.write("d/doc.txt", "base");
    a.synced();
    b.synced();
    a.write("d/doc.txt", "from a");
    b.write("d/doc.txt", "from b");
    a.synced();
    b.synced();
    let copies = b.conflict_copies("d", "doc");
    assert_eq!(copies.len(), 1, "{copies:?}");
    let copy = &copies[0];
    // Never by the whole journal, which it falls back to when the base does not do.
    let ask = || {
        let run = a.cairn(&["conflicts"]).expect_ok();
        assert!(!run.stderr.contains("whole journal"), "{}", run.text());
        run
    };

    // a has not synced since the copy was made.
    let inbox = ask();
    let line = format!("d/{copy}: the competing version of d/doc.txt");
    assert!(inbox.stdout.contains(&line), "{}", inbox.text());
    assert_eq!(inbox.stdout.lines().count(), 1, "{}", inbox.text());
    // Synced, the copy is in its base, and still open.
    a.synced();
    let inbox = ask();
    assert!(inbox.stdout.contains(&line), "{}", inbox.text());

    // Renaming the folder above the copy is not dealing with it, and the inbox says where it is now.
    std::fs::rename(b.path("d"), b.path("e")).unwrap();
    b.synced();
    let inbox = ask();
    let line = format!("e/{copy}: the competing version of e/doc.txt");
    assert!(inbox.stdout.contains(&line), "{}", inbox.text());

    // Moving the copy out of its folder is, before a has synced it and after.
    b.write("kept/.keep", "");
    std::fs::rename(
        b.path(&format!("e/{copy}")),
        b.path(&format!("kept/{copy}")),
    )
    .unwrap();
    b.synced();
    for _ in 0..2 {
        let inbox = ask();
        assert!(
            inbox
                .stdout
                .starts_with("no conflict waits for a person\n1 more dealt with"),
            "{}",
            inbox.text()
        );
        a.synced();
    }
    let all = a.cairn(&["conflicts", "--all"]).expect_ok();
    let line = format!("dealt with: d/{copy}, now kept/{copy}");
    assert!(all.stdout.contains(&line), "{}", all.text());
}

#[test]
fn edit_against_delete_the_edit_survives_either_way() {
    let (_sb, _s, a, b) = pair("editdelete");
    a.write("x", "base");
    a.write("y", "base");
    a.write("keep", "so the tree is not empty");
    a.synced();
    b.synced();
    // x: edited on a, deleted on b, a first. y: deleted on a, edited on b, a first.
    a.write("x", "edited");
    std::fs::remove_file(b.path("x")).unwrap();
    std::fs::remove_file(a.path("y")).unwrap();
    b.write("y", "edited");
    a.synced();
    let run = b.synced();
    assert!(run.stdout.contains("event:"), "{}", run.text());
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[&b"x".to_vec()].content, b"edited");
    assert_eq!(p.entries[&b"y".to_vec()].content, b"edited");
}

#[test]
fn an_edit_inside_a_directory_deleted_elsewhere_brings_the_directory_back() {
    let (_sb, _s, a, b) = pair("editdeletedir");
    a.write("d/inner/f", "base");
    a.write("keep", "k");
    a.synced();
    b.synced();
    std::fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    b.write("d/inner/f", "edited");
    b.write("d/new", "new");
    b.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[&b"d/inner/f".to_vec()].content, b"edited");
    assert_eq!(p.entries[&b"d/new".to_vec()].content, b"new");
}

#[test]
fn different_new_files_at_one_name_keep_both_and_equal_ones_merge() {
    let (_sb, _s, a, b) = pair("samename");
    a.write("new.txt", "mine");
    a.write("same.txt", "identical");
    a.write("d/from-a", "1");
    b.write("new.txt", "theirs");
    b.write("same.txt", "identical");
    b.write("d/from-b", "2");
    a.synced();
    b.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[&b"new.txt".to_vec()].content, b"mine");
    assert_eq!(a.conflict_copies("", "new").len(), 1);
    assert!(
        a.conflict_copies("", "same").is_empty(),
        "same content is not a conflict"
    );
    assert!(p.entries.contains_key(b"d/from-a".as_slice()));
    assert!(p.entries.contains_key(b"d/from-b".as_slice()));
}

#[test]
fn a_file_and_a_directory_at_one_name_both_stay() {
    let (_sb, _s, a, b) = pair("kinds");
    a.write("n", "a file");
    b.write("n/inside", "a directory");
    a.synced();
    b.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[&b"n".to_vec()].content, b"a file");
    let copies = a.conflict_copies("", "n");
    assert_eq!(copies.len(), 1, "{:?}", p.names());
    assert_eq!(a.read(&format!("{}/inside", copies[0])), b"a directory");
}

#[test]
fn concurrent_moves_that_would_cycle_settle_on_one_tree() {
    let (_sb, _s, a, b) = pair("cycle");
    a.write("p/fp", "1");
    a.write("q/fq", "2");
    a.synced();
    b.synced();
    std::fs::rename(a.path("p"), a.path("q/p")).unwrap();
    std::fs::rename(b.path("q"), b.path("p/q")).unwrap();
    a.synced();
    let run = b.synced();
    assert!(run.stdout.contains("cycle"), "{}", run.text());
    let p = converge(&[&a, &b]);
    let names = p.names();
    assert!(
        names.iter().any(|n| n.ends_with("/fp")) && names.iter().any(|n| n.ends_with("/fq")),
        "{names:?}"
    );
}

#[test]
fn swapped_names_and_a_directory_into_its_former_child() {
    let (_sb, _s, a, b) = pair("swap");
    a.write("one", "1");
    a.write("two", "2");
    a.write("top/under/f", "f");
    a.synced();
    b.synced();
    std::fs::rename(a.path("one"), a.path("tmp")).unwrap();
    std::fs::rename(a.path("two"), a.path("one")).unwrap();
    std::fs::rename(a.path("tmp"), a.path("two")).unwrap();
    std::fs::rename(a.path("top/under"), a.path("under")).unwrap();
    std::fs::rename(a.path("top"), a.path("under/top")).unwrap();
    a.synced();
    let run = b.synced();
    assert!(run.ok());
    assert_eq!(b.read("one"), b"2");
    assert_eq!(b.read("two"), b"1");
    assert_eq!(b.read("under/f"), b"f");
    assert!(b.path("under/top").is_dir());
    converge(&[&a, &b]);
}

#[test]
fn metadata_on_both_merges_field_by_field() {
    let (_sb, _s, a, b) = pair("metamerge");
    a.write("f", "x");
    a.synced();
    b.synced();
    chmod(&a.path("f"), other_mode(0o600));
    // Windows holds no xattrs: the other field there is the time.
    #[cfg(unix)]
    set_xattr(&b.path("f"), "user.from-b", b"1");
    #[cfg(windows)]
    set_mtime(&b.path("f"), 1_000_000_000, 0);
    a.synced();
    b.synced();
    let p = converge(&[&a, &b]);
    let e = &p.entries[&b"f".to_vec()];
    assert_eq!(e.mode, other_mode(0o600));
    #[cfg(unix)]
    assert_eq!(e.xattrs[&b"user.from-b".to_vec()], b"1");
    #[cfg(windows)]
    assert_eq!(e.mtime, (1_000_000_000, 0));
}

/// Not on Windows: a clash takes two changes of one field from a third value, and of a mode a Windows
/// file holds two, writable or read-only. Times clash silently, as any two copies' differ.
#[cfg(unix)]
#[test]
fn metadata_on_both_same_field_the_loser_is_in_the_log() {
    let (_sb, _s, a, b) = pair("metaclash");
    a.write("f", "x");
    a.synced();
    b.synced();
    chmod(&a.path("f"), 0o600);
    chmod(&b.path("f"), 0o640);
    a.synced();
    let run = b.synced();
    assert!(run.stdout.contains("event:"), "{}", run.text());
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[&b"f".to_vec()].mode, 0o600, "server order wins");
}

#[test]
fn hard_links_stay_linked_through_an_edit_and_a_break() {
    let (_sb, _s, a, b) = pair("hardlinks");
    a.write("one", "shared");
    std::fs::hard_link(a.path("one"), a.path("two")).unwrap();
    a.synced();
    b.synced();
    assert_eq!(nlink(&b.path("one")), 2);

    // An edit through one name is an edit to the object: the link survives elsewhere.
    std::fs::OpenOptions::new()
        .append(true)
        .open(a.path("two"))
        .unwrap();
    std::fs::write(a.path("two"), "changed in place").unwrap();
    a.synced();
    b.synced();
    assert_eq!(b.read("one"), b"changed in place");
    assert_eq!(inode(&b.path("one")), inode(&b.path("two")));

    // A save by replacing breaks the link here, and that is what the other side gets.
    std::fs::write(a.path("tmp"), "replaced").unwrap();
    std::fs::rename(a.path("tmp"), a.path("two")).unwrap();
    a.synced();
    b.synced();
    assert_eq!(b.read("one"), b"changed in place");
    assert_eq!(b.read("two"), b"replaced");
    assert_ne!(inode(&b.path("one")), inode(&b.path("two")));
    converge(&[&a, &b]);
}

// --- Invariant 2: stored and applied are separate ---

/// The plan's sentence: Linux uploads script.sh at 0755, a client that cannot hold the bit edits it,
/// the new revision keeps 0755, Linux receives the edit at 0755. On Windows the bit is read-only.
#[test]
fn a_client_that_cannot_hold_a_mode_does_not_erase_it() {
    let sb = Sandbox::on_disk("inv2");
    let server = Server::start(sb.path("server"));
    let linux = Device::new(&sb, &server, "linux");
    let other = Device::with_flags(&sb, &server, "other", &["--no-mode"]);

    linux.write("script.sh", "#!/bin/sh\necho 1\n");
    chmod(&linux.path("script.sh"), other_mode(0o755));
    linux.synced();
    other.synced();
    let applied = mode_of(&other.path("script.sh"));
    assert_eq!(
        if cfg!(windows) {
            applied & 0o222
        } else {
            applied & 0o111
        },
        if cfg!(windows) { 0o200 } else { 0 },
        "this client does not apply modes"
    );
    assert!(
        other
            .synced()
            .stdout
            .contains(" 0 steps applied here, 0 commits"),
        "and that is no change"
    );

    other.write("script.sh", "#!/bin/sh\necho 2\n");
    other.synced();
    linux.synced();
    assert_eq!(linux.read("script.sh"), b"#!/bin/sh\necho 2\n");
    assert_eq!(mode_of(&linux.path("script.sh")), other_mode(0o755));
}

/// What Windows does not hold stays stored: the rest of a mode, an owner, another platform's xattrs.
/// Put there by a Linux device, here by a commit of the test's own, they arrive as no change, and an
/// edit on Windows, or read-only set there, keeps them.
#[cfg(windows)]
#[test]
fn what_windows_does_not_hold_stays_stored() {
    use cairn_protocol::wire::{CommitRequest, CommitResponse};
    use cairn_protocol::{
        Bytes, DeviceId, ObjectId, Op, OpId, Owner, RecordKey, RevisionBody, RevisionId,
        NS_LINUX_XATTR,
    };
    let (_sb, _s, a, b) = pair("windows-holds");
    a.write("f", "made on windows");
    a.synced();
    b.synced();
    let config = cairn_engine::root::open_config(&a.root).unwrap();
    let client =
        cairn_engine::client::Client::new(&config, cairn_engine::client::Retries::Short).unwrap();
    // The newest revision of what was made at `f`.
    let latest = || -> (ObjectId, RevisionId, RevisionBody) {
        let mut found: Option<(ObjectId, RevisionId, RevisionBody)> = None;
        for change in client.changes(0).unwrap().changes {
            for op in change.ops {
                match op {
                    Op::Create {
                        object,
                        revision,
                        body,
                        link,
                        ..
                    } if link.name.as_bytes() == b"f" => found = Some((object, revision, body)),
                    Op::Update {
                        object,
                        revision,
                        body,
                        ..
                    } if found.as_ref().is_some_and(|f| f.0 == object) => {
                        found = Some((object, revision, body))
                    }
                    _ => {}
                }
            }
        }
        found.expect("f is on the server")
    };

    let (object, base, mut body) = latest();
    body.metadata.mode = 0o755;
    body.metadata.owner = Some(Owner {
        uid: 4242,
        gid: 4343,
        user: Some(Bytes(b"ann".to_vec())),
        group: Some(Bytes(b"staff".to_vec())),
    });
    body.metadata.records.insert(
        RecordKey {
            ns: NS_LINUX_XATTR.into(),
            name: Bytes(b"user.from-linux".to_vec()),
        },
        Bytes(b"kept".to_vec()),
    );
    let linux = body.metadata.clone();
    let answer = client
        .commit(&CommitRequest {
            op_id: OpId::random(),
            device: DeviceId::random(),
            device_name: "linux".into(),
            ops: vec![Op::Update {
                object,
                base,
                revision: RevisionId::random(),
                body,
                restored_from: None,
            }],
        })
        .unwrap();
    assert!(
        matches!(answer, CommitResponse::Committed { .. }),
        "{answer:?}"
    );

    // Arriving, it is no change of anyone's.
    for d in [&a, &b] {
        let run = d.synced();
        assert!(run.stdout.contains(" 0 commits"), "{}", run.text());
        assert_eq!(mode_of(&d.path("f")), 0o644);
    }
    assert_eq!(latest().2.metadata, linux);

    // An edit on Windows keeps all of it.
    a.write("f", "edited on windows");
    a.synced();
    let kept = latest().2.metadata;
    assert_eq!(
        (kept.mode, &kept.owner, &kept.records),
        (0o755, &linux.owner, &linux.records)
    );

    // Read-only, which Windows holds, changes the write bits alone.
    chmod(&a.path("f"), 0o444);
    a.synced();
    let kept = latest().2.metadata;
    assert_eq!(
        (kept.mode, &kept.owner, &kept.records),
        (0o555, &linux.owner, &linux.records)
    );
    b.synced();
    assert_eq!(b.read("f"), b"edited on windows");
    assert_eq!(mode_of(&b.path("f")), 0o444);
    let run = b.synced();
    assert!(
        run.stdout.contains(" 0 steps applied here, 0 commits"),
        "{}",
        run.text()
    );
}

/// Not on Windows, which holds no setgid bit to set.
#[cfg(unix)]
#[test]
fn setuid_and_foreign_xattrs_stay_stored_when_not_applied() {
    let sb = Sandbox::on_disk("inv2-setid");
    let server = Server::start(sb.path("server"));
    let a = Device::with_flags(&sb, &server, "a", &["--setid"]);
    let b = Device::new(&sb, &server, "b");
    a.write("tool", "x");
    chmod(&a.path("tool"), 0o2755);
    a.synced();
    b.synced();
    assert_eq!(mode_of(&b.path("tool")), 0o755, "setgid needs a policy");
    // b changes a permission and the content; setgid is not b's to lose.
    chmod(&b.path("tool"), 0o775);
    b.write("tool", "y");
    b.synced();
    a.synced();
    assert_eq!(a.read("tool"), b"y");
    assert_eq!(mode_of(&a.path("tool")), 0o2775);
}

// --- Invariant 5: a missing mount ---

#[test]
fn a_folder_with_no_marker_pauses_and_changes_nothing() {
    let (sb, _s, a, b) = pair("nomount");
    a.write("data", "precious");
    a.synced();
    b.synced();
    let head = seq(&b.synced());

    // The mount has gone: an empty folder at the same path, state directory and all.
    let held = sb.path("unmounted");
    std::fs::rename(&a.root, &held).unwrap();
    std::fs::create_dir(&a.root).unwrap();
    let run = a.sync();
    assert_eq!(run.code, Some(2), "{}", run.text());
    assert!(run.stdout.contains("not mounted"), "{}", run.text());
    assert_eq!(seq(&b.synced()), head, "the server saw nothing");

    // Back again, nothing happened.
    std::fs::remove_dir(&a.root).unwrap();
    std::fs::rename(&held, &a.root).unwrap();
    assert_eq!(seq(&a.synced()), head);
    assert_eq!(b.read("data"), b"precious");
}

#[test]
fn an_emptied_folder_is_not_a_mass_delete_until_proven_so() {
    let (_sb, _s, a, b) = pair("emptied");
    for i in 0..20 {
        a.write(&format!("d/{i}"), format!("{i}"));
    }
    a.synced();
    b.synced();
    let head = seq(&b.synced());

    std::fs::remove_dir_all(a.path("d")).unwrap();
    let run = a.sync();
    assert_eq!(run.code, Some(2), "{}", run.text());
    assert!(run.stdout.contains("--allow-mass-delete"), "{}", run.text());
    assert_eq!(seq(&b.synced()), head);
    assert!(b.exists("d/7"));

    // A person says it is meant, and it goes through, with history keeping it all.
    a.cairn(&["sync", "--allow-mass-delete"]).expect_ok();
    b.synced();
    assert!(!b.exists("d"));
}

#[test]
fn losing_most_of_a_tree_pauses_but_ordinary_deletes_do_not() {
    let (_sb, _s, a, _b) = pair("share");
    for i in 0..30 {
        a.write(&format!("{i}"), "x");
    }
    a.synced();
    for i in 0..5 {
        std::fs::remove_file(a.path(&format!("{i}"))).unwrap();
    }
    a.synced();
    for i in 5..25 {
        std::fs::remove_file(a.path(&format!("{i}"))).unwrap();
    }
    assert_eq!(a.sync().code, Some(2));
}

// --- Invariant 6: symlinks ---

#[test]
fn sync_never_traverses_a_symlink() {
    let (sb, _s, a, b) = pair("symlink");
    let outside = sb.path("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), "not yours").unwrap();
    #[cfg(unix)]
    {
        symlink(&outside, a.path("escape")).unwrap();
        symlink(outside.join("secret"), a.path("escape-file")).unwrap();
    }
    // Windows syncs no links yet: a junction made there is reported and stays here, and holds nothing
    // else back.
    #[cfg(windows)]
    dir_link(&outside, &a.path("escape"));
    let run = a.synced();
    b.synced();
    // It synced as a link with its target verbatim, and nothing behind it travelled.
    #[cfg(unix)]
    assert_eq!(std::fs::read_link(b.path("escape")).unwrap(), outside);
    #[cfg(windows)]
    {
        assert!(
            run.stdout
                .contains("skipped: escape: this kind of entry is not synced here"),
            "{}",
            run.text()
        );
        assert!(!b.exists("escape"));
    }
    let _ = run;
    let p = Picture::of(&a.root);
    assert_eq!(p.entries[&b"escape".to_vec()].kind, 'l');
    assert!(!p.entries.contains_key(b"escape/secret".as_slice()));

    // A chmod or an edit through the link's name goes nowhere near what it points at.
    let before = Picture::of(&outside);
    b.write("real-dir/f", "x");
    b.synced();
    a.synced();
    assert_eq!(
        Picture::of(&outside).diff(&before, Ignore::default()),
        Vec::<String>::new()
    );
}

/// On Windows the link is a junction, as below.
#[test]
fn a_directory_replaced_by_a_symlink_does_not_redirect_writes() {
    let (sb, _s, a, b) = pair("symlink-swap");
    let outside = sb.path("outside");
    std::fs::create_dir(&outside).unwrap();
    a.write("d/f", "base");
    a.synced();
    b.synced();
    // Meanwhile the remote adds to d, and locally d becomes a link leaving the tree.
    b.write("d/new", "from remote");
    b.write("d/f", "changed remotely");
    b.synced();
    std::fs::rename(a.path("d"), sb.path("d-gone")).unwrap();
    dir_link(&outside, &a.path("d"));

    let _ = a.sync();
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing arrived outside the root"
    );
    // Windows syncs no links yet: the junction stays out of view, and in the way, until it goes.
    #[cfg(windows)]
    remove_dir_link(&a.path("d"));
    converge(&[&a, &b]);
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

/// The race itself: a parent is swapped for a link while a file for it is being written. The kernel
/// holds the original directory's descriptor, so the write cannot follow the swap.
#[test]
fn a_parent_swapped_for_a_symlink_during_apply_does_not_redirect_it() {
    let (sb, _s, a, b) = pair("symlink-race");
    let outside = sb.path("outside");
    std::fs::create_dir(&outside).unwrap();
    a.write("d/f", "base");
    a.synced();
    b.synced();
    b.write("d/new", "from remote");
    b.synced();

    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("apply.temp.written:{}", gate.display());
    let device = a.clone();
    let worker = std::thread::spawn(move || device.sync_env(&[("CAIRN_FP_PAUSE", &pause)]));
    while !gate.join("reached").exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Windows refuses to move a folder while a file in it is open: there the race cannot happen.
    match std::fs::rename(a.path("d"), sb.path("moved-away")) {
        Ok(()) => dir_link(&outside, &a.path("d")),
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(e) => panic!("{e}"),
    }
    std::fs::write(gate.join("go"), b"").unwrap();
    let _ = worker.join().unwrap();

    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing arrived outside the root"
    );
}

// --- Invariant 7: restore ---

#[test]
fn every_restore_is_a_new_revision_and_can_be_undone() {
    let (_sb, _s, a, b) = pair("restore");
    a.write("f", "version 1");
    let v1 = seq(&a.synced());
    a.write("f", "version 2");
    let v2 = seq(&a.synced());
    b.synced();

    let run = a
        .cairn(&["restore", "f", "--at", &v1.to_string()])
        .expect_ok();
    assert!(run.stdout.contains("restored f"), "{}", run.text());
    assert_eq!(a.read("f"), b"version 1");
    b.synced();
    assert_eq!(b.read("f"), b"version 1");

    let history = a.cairn(&["history", "f"]).expect_ok();
    assert_eq!(history.stdout.lines().count(), 3, "{}", history.text());
    assert!(
        history.stdout.contains("restored from"),
        "{}",
        history.text()
    );

    // The restore is undone the same way.
    a.cairn(&["restore", "f", "--at", &v2.to_string()])
        .expect_ok();
    assert_eq!(a.read("f"), b"version 2");
    b.synced();
    assert_eq!(b.read("f"), b"version 2");
}

#[test]
fn deleted_files_and_directories_come_back_from_any_sequence() {
    let (_sb, _s, a, b) = pair("restore-deleted");
    a.write("d/one", "1");
    a.write("d/sub/two", "2");
    a.write("other", "o");
    let before = seq(&a.synced());
    std::fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    b.synced();
    assert!(!b.exists("d"));

    a.cairn(&["restore", "d", "--at", &before.to_string()])
        .expect_ok();
    assert_eq!(a.read("d/sub/two"), b"2");
    b.synced();
    assert_eq!(b.read("d/one"), b"1");
    assert_eq!(b.read("d/sub/two"), b"2");
    converge(&[&a, &b]);
}

#[test]
fn a_directory_restored_as_of_a_sequence_loses_only_what_came_later() {
    let (_sb, _s, a, b) = pair("restore-dir");
    a.write("d/old", "old content");
    a.write("elsewhere", "untouched");
    let then = seq(&a.synced());
    a.write("d/old", "edited");
    a.write("d/later", "came later");
    a.write("elsewhere", "edited too");
    a.synced();

    a.cairn(&["restore", "d", "--at", &then.to_string()])
        .expect_ok();
    assert_eq!(a.read("d/old"), b"old content");
    assert!(!a.exists("d/later"), "came later");
    assert_eq!(a.read("elsewhere"), b"edited too", "outside the restore");
    // Later content is not gone: it is one more restore away.
    let latest = seq(&a.synced()) - 1;
    a.cairn(&["restore", "d", "--at", &latest.to_string()])
        .expect_ok();
    assert_eq!(a.read("d/later"), b"came later");
    converge(&[&a, &b]);
}

#[test]
fn a_restore_onto_a_name_now_in_use_keeps_both() {
    let (_sb, _s, a, _b) = pair("restore-taken");
    a.write("n", "the old one");
    a.write("keep", "k");
    let then = seq(&a.synced());
    std::fs::remove_file(a.path("n")).unwrap();
    a.synced();
    a.write("n", "a new one");
    a.synced();
    a.cairn(&["restore", "n", "--at", &then.to_string()])
        .expect_ok();
    let names = Picture::of(&a.root).names();
    let found: Vec<_> = names.iter().filter(|n| n.starts_with('n')).collect();
    assert_eq!(found.len(), 2, "{names:?}");
    assert!(a.read("n") == b"a new one", "what is there stays");
}

#[test]
fn export_writes_a_past_tree_and_changes_nothing() {
    let (sb, _s, a, _b) = pair("export");
    a.write("d/f", "v1");
    let then = seq(&a.synced());
    a.write("d/f", "v2");
    let head = seq(&a.synced());
    let dest = sb.path("inspect");
    a.cairn(&["export", dest.to_str().unwrap(), "--at", &then.to_string()])
        .expect_ok();
    assert_eq!(std::fs::read(dest.join("d/f")).unwrap(), b"v1");
    assert_eq!(seq(&a.synced()), head);
    assert_eq!(a.read("d/f"), b"v2");
    // Refuses to write over anything.
    let run = a.cairn(&["export", dest.to_str().unwrap()]);
    assert!(
        !run.ok() && run.stderr.contains("not empty"),
        "{}",
        run.text()
    );
}

// --- Invariant 3 and 4 under races ---

#[test]
fn an_edit_made_during_apply_is_not_overwritten() {
    let (sb, _s, a, b) = pair("edit-race");
    a.write("f", "base");
    a.synced();
    b.synced();
    b.write("f", "from remote");
    b.synced();

    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("apply.replace.before_check:{}", gate.display());
    let device = a.clone();
    let worker = std::thread::spawn(move || device.sync_env(&[("CAIRN_FP_PAUSE", &pause)]));
    while !gate.join("reached").exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    a.write("f", "typed while syncing");
    std::fs::write(gate.join("go"), b"").unwrap();
    let _ = worker.join().unwrap();

    let p = converge(&[&a, &b]);
    let contents: Vec<&[u8]> = p
        .entries
        .values()
        .filter(|e| e.kind == 'f')
        .map(|e| e.content.as_slice())
        .collect();
    assert!(
        contents.contains(&&b"typed while syncing"[..]),
        "the edit is lost: {:?}",
        p.names()
    );
    assert!(
        contents.contains(&&b"from remote"[..]),
        "the remote version is lost: {:?}",
        p.names()
    );
}

#[test]
fn a_same_size_edit_that_keeps_its_mtime_is_still_seen() {
    let (_sb, _s, a, b) = pair("lost-event");
    a.write("f", "AAAA");
    set_mtime(&a.path("f"), 1_000_000_000, 5);
    a.synced();
    b.synced();
    // Wait past the window in which timestamps cannot prove anything, then make the sneakiest edit:
    // same size, same mtime. Only ctime moves.
    std::thread::sleep(std::time::Duration::from_millis(300));
    a.synced();
    a.write("f", "BBBB");
    set_mtime(&a.path("f"), 1_000_000_000, 5);
    a.synced();
    b.synced();
    assert_eq!(b.read("f"), b"BBBB");
}

#[test]
fn a_long_offline_device_does_not_bring_back_what_was_deleted() {
    let (_sb, _s, a, b) = pair("offline");
    for i in 0..10 {
        a.write(&format!("old/{i}"), format!("{i}"));
    }
    a.write("keep", "k");
    a.synced();
    b.synced();

    // b goes away. Life goes on: many commits, deletes, moves.
    std::fs::remove_file(a.path("old/3")).unwrap();
    a.synced();
    std::fs::remove_dir_all(a.path("old")).unwrap();
    a.cairn(&["sync", "--allow-mass-delete"]).expect_ok();
    for i in 0..60 {
        a.write("churn", format!("{i}"));
        a.synced();
    }
    b.write("written-offline", "while away");

    b.synced();
    assert!(!b.exists("old"), "deleted files came back");
    assert_eq!(b.read("written-offline"), b"while away");
    let p = converge(&[&a, &b]);
    assert!(!p.entries.contains_key(b"old".as_slice()));
    assert_eq!(
        p.entries[&b"written-offline".to_vec()].content,
        b"while away"
    );
}

#[test]
fn a_device_rejoining_with_its_state_lost_merges_and_loses_nothing() {
    let (sb, s, a, b) = pair("rejoin");
    a.write("same", "same content");
    a.write("diverged", "server's");
    a.synced();
    b.synced();
    // b loses .cairn; its files stay, one edited in the meantime.
    std::fs::remove_dir_all(b.path(".cairn")).unwrap();
    b.write("diverged", "mine");
    b.write("only-here", "mine too");
    let token = s.token_file.to_str().unwrap().to_string();
    run(
        &[
            "init",
            "--server",
            &s.url,
            "--device-name",
            "b2",
            "--token-file",
            &token,
            b.root.to_str().unwrap(),
        ],
        &[],
    )
    .expect_ok();
    let _ = sb;
    b.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[&b"same".to_vec()].content, b"same content");
    assert_eq!(p.entries[&b"diverged".to_vec()].content, b"server's");
    assert_eq!(p.entries[&b"only-here".to_vec()].content, b"mine too");
    assert_eq!(
        a.conflict_copies("", "diverged").len(),
        1,
        "{:?}",
        p.names()
    );
}

#[test]
fn a_second_sync_at_once_is_refused_not_interleaved() {
    let (sb, _s, a, b) = pair("lock");
    b.write("f", "from b");
    b.synced();
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("sync.round:{}", gate.display());
    let device = a.clone();
    let worker = std::thread::spawn(move || device.sync_env(&[("CAIRN_FP_PAUSE", &pause)]));
    while !gate.join("reached").exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let second = a.sync();
    assert_eq!(second.code, Some(1), "{}", second.text());
    assert!(
        second.stderr.contains("already working"),
        "{}",
        second.text()
    );
    // What only asks the server needs no lock, so it answers while the sync runs.
    for args in [
        vec!["log"],
        vec!["history", "f"],
        vec!["conflicts"],
        vec!["export", sb.path("out").to_str().unwrap()],
    ] {
        a.cairn(&args).expect_ok();
    }
    let status = a.cairn(&["status"]).expect_ok();
    assert!(
        status.stdout.contains("a sync is running now"),
        "{}",
        status.text()
    );
    std::fs::write(gate.join("go"), b"").unwrap();
    worker.join().unwrap().expect_ok();
}

#[test]
fn init_makes_a_root_of_the_folder_given_by_root() {
    let sb = Sandbox::on_disk("init-root");
    let server = Server::start(sb.path("server"));
    let here = sb.path("here");
    let there = sb.path("there");
    std::fs::create_dir(&here).unwrap();
    std::fs::create_dir(&there).unwrap();
    let token = server.token_file.to_str().unwrap();
    let init = |dir: &std::path::Path, extra: &[&str]| {
        let mut cmd = Command::new(CAIRN);
        cmd.current_dir(dir)
            .args(["init", "--server", &server.url, "--token-file", token])
            .args(extra);
        cmd.output().unwrap()
    };
    let out = init(&here, &["--root", there.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(there.join(".cairn/root-id").exists());
    assert!(!here.join(".cairn").exists(), "--root was ignored");

    // Two different folders is a mistake to say, not to guess at.
    let other = sb.path("other");
    std::fs::create_dir(&other).unwrap();
    let out = init(&here, &["--root", other.to_str().unwrap(), "."]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!here.join(".cairn").exists() && !other.join(".cairn").exists());
}

#[test]
fn output_nobody_reads_is_no_failure() {
    // `cairn status | head`: the reader goes away, and the command neither panics nor stops.
    let (_sb, _s, a, _b) = pair("pipe");
    for i in 0..20 {
        a.write(&format!("f{i}"), "x");
    }
    for args in [vec!["status"], vec!["sync"], vec!["log"]] {
        let mut cmd = Command::new(CAIRN);
        cmd.args(["--root", a.root.to_str().unwrap()])
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{args:?}: {:?} {err}", out.status);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
    }
    // The sync whose output went nowhere still did its work.
    let status = a.cairn(&["status"]).expect_ok();
    assert!(status.stdout.contains("nothing to do"), "{}", status.text());
}

#[test]
fn a_wrong_token_is_refused() {
    let (sb, s, a, _b) = pair("auth");
    let bad = sb.path("bad-token");
    std::fs::write(&bad, "not the token").unwrap();
    let c = sb.path("c");
    std::fs::create_dir(&c).unwrap();
    let r = run(
        &[
            "init",
            "--server",
            &s.url,
            "--token-file",
            bad.to_str().unwrap(),
            c.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        !r.ok() && r.stderr.contains("refused the access token"),
        "{}",
        r.text()
    );
    a.synced();
}

#[test]
fn plain_http_beyond_loopback_is_refused() {
    let sb = Sandbox::on_disk("http");
    let token = sb.path("token");
    std::fs::write(&token, "t").unwrap();
    let c = sb.path("c");
    std::fs::create_dir(&c).unwrap();
    let r = run(
        &[
            "init",
            "--server",
            "http://192.0.2.1:1",
            "--token-file",
            token.to_str().unwrap(),
            c.to_str().unwrap(),
        ],
        &[],
    );
    assert!(!r.ok() && r.stderr.contains("in the clear"), "{}", r.text());
}

/// The plan's acceptance scenario, as far as one Linux machine goes: a working tree used from two
/// places, one at a time.
#[test]
fn a_git_working_tree_comes_out_clean_on_the_other_side() {
    if Command::new("git").arg("--version").output().is_err() {
        skip("git is not installed");
        return;
    }
    let (_sb, _s, a, b) = pair("git");
    let git = |root: &std::path::Path, args: &[&str]| {
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
        symlink("src/main.sh", a.path("run")).unwrap();
    }
    a.write("node_modules/dep/index.js", "module.exports = 1;\n");
    a.write(".venv/bin/python", "#!/fake\n");
    a.write(".gitignore", ".cairn\nnode_modules\n.venv\n");
    git(&a.root, &["add", "."]);
    git(&a.root, &["commit", "-qm", "first"]);

    a.synced();
    b.synced();
    assert_eq!(
        git(&b.root, &["status", "--porcelain"]),
        "",
        "git status is not clean"
    );
    assert_eq!(git(&b.root, &["log", "--format=%s"]).trim(), "first");
    #[cfg(unix)]
    assert_eq!(mode_of(&b.path("src/main.sh")) & 0o777, 0o755);
    assert!(b.exists("node_modules/dep/index.js") && b.exists(".venv/bin/python"));

    // Work continues on the other machine, one at a time.
    b.write("src/main.sh", "#!/bin/sh\necho changed\n");
    git(&b.root, &["commit", "-qam", "second"]);
    b.synced();
    a.synced();
    assert_eq!(git(&a.root, &["status", "--porcelain"]), "");
    assert_eq!(git(&a.root, &["log", "--format=%s"]).lines().count(), 2);
    let copies: Vec<String> = Picture::of(&a.root)
        .names()
        .into_iter()
        .filter(|n| n.contains("(conflict"))
        .collect();
    assert!(
        copies.is_empty(),
        "no conflict copies inside .git: {copies:?}"
    );
}

/// Two machines commit to one repository before either has synced: git cannot take a copy of a ref
/// under a name with spaces, so the copy must be a branch it can merge.
#[test]
fn commits_made_at_once_on_two_machines_become_two_branches() {
    if Command::new("git").arg("--version").output().is_err() {
        skip("git is not installed");
        return;
    }
    let (_sb, _s, a, b) = pair("git-race");
    let git = |root: &std::path::Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(root.join("repo"))
            .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
            // Git for Windows turns line ends into CRLF on checkout by default.
            .args(["-c", "core.autocrlf=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    a.write("repo/one", "1\n");
    a.write("repo/two", "2\n");
    git(&a.root, &["init", "-q"]);
    git(&a.root, &["add", "."]);
    git(&a.root, &["commit", "-qm", "base"]);
    a.synced();
    b.synced();

    a.write("repo/one", "1 from a\n");
    git(&a.root, &["commit", "-qam", "from a"]);
    b.write("repo/two", "2 from b\n");
    git(&b.root, &["commit", "-qam", "from b"]);
    a.synced();
    b.synced();
    a.synced();

    let branch = git(&a.root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let branch = branch.trim();
    for dev in [&a, &b] {
        let all = git(&dev.root, &["log", "--all", "--format=%s"]);
        for subject in ["base", "from a", "from b"] {
            assert!(
                all.lines().any(|l| l == subject),
                "{subject} missing: {all}"
            );
        }
        git(&dev.root, &["fsck", "--no-dangling"]);
    }
    let branches = git(&b.root, &["branch", "--format=%(refname:short)"]);
    let copy = format!("{branch}.conflict-b-");
    let theirs = branches
        .lines()
        .find(|l| l.starts_with(&copy))
        .unwrap_or_else(|| panic!("no branch {copy}*: {branches}"))
        .to_string();

    // b's file is in the tree but not in the commit that won, and git says so; merging joins them.
    git(&b.root, &["merge", "--autostash", "--no-edit", &theirs]);
    git(&b.root, &["branch", "-d", &theirs]);
    b.synced();
    a.synced();
    for dev in [&a, &b] {
        assert_eq!(git(&dev.root, &["status", "--porcelain"]), "");
        assert_eq!(dev.read("repo/one"), b"1 from a\n");
        assert_eq!(dev.read("repo/two"), b"2 from b\n");
        let log = git(&dev.root, &["log", "--format=%s"]);
        assert!(log.contains("from a") && log.contains("from b"), "{log}");
        git(&dev.root, &["fsck", "--no-dangling"]);
        let spaced: Vec<String> = Picture::of(&dev.root.join("repo/.git"))
            .names()
            .into_iter()
            .filter(|n| n.contains(' '))
            .collect();
        assert!(spaced.is_empty(), "{spaced:?}");
    }
}

/// A program that empties a file to write it again leaves it empty for a moment, which reads as steady
/// as any file, and on a busy machine for longer than the stable window. While the program still has it
/// open, the empty file goes nowhere.
#[test]
fn a_file_emptied_by_a_program_still_writing_it_is_not_sent_empty() {
    use std::io::Write;
    let (_sb, _s, a, b) = pair("emptied");
    a.write("doc", "the first version\n");
    a.synced();
    b.synced();
    let mut saving = std::fs::File::create(a.path("doc")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());
    assert!(run.text().contains("emptied"), "{}", run.text());
    b.synced();
    assert_eq!(b.read("doc"), b"the first version\n");
    // However long the program takes: the kernel says it still has the file open.
    #[cfg(any(target_os = "linux", windows))]
    {
        std::thread::sleep(std::time::Duration::from_millis(2200));
        let run = a.sync();
        assert_eq!(run.code, Some(3), "{}", run.text());
        b.synced();
        assert_eq!(b.read("doc"), b"the first version\n");
    }
    saving.write_all(b"the second version\n").unwrap();
    drop(saving);
    a.synced();
    b.synced();
    assert_eq!(b.read("doc"), b"the second version\n");
}

/// What is emptied for good is sent empty: at once where the kernel can say nobody is writing it, a
/// moment later where it cannot.
#[test]
fn a_file_emptied_and_closed_is_sent_empty() {
    let (_sb, _s, a, b) = pair("emptied-closed");
    a.write("log", "old lines\n");
    a.synced();
    b.synced();
    std::fs::File::create(a.path("log")).unwrap();
    let start = std::time::Instant::now();
    loop {
        let run = a.sync();
        if run.ok() {
            break;
        }
        assert_eq!(run.code, Some(3), "{}", run.text());
        assert!(start.elapsed().as_secs() < 5, "{}", run.text());
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    b.synced();
    assert_eq!(b.read("log"), b"");
}

/// A program that keeps a file open between writes, a log or a database, does not keep it from
/// syncing once it has been still for a moment.
#[test]
fn a_file_an_idle_program_keeps_open_still_syncs() {
    use std::io::Write;
    let (_sb, _s, a, b) = pair("held-open");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(a.path("app.log"))
        .unwrap();
    log.write_all(b"started\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2300));
    a.synced();
    b.synced();
    assert_eq!(b.read("app.log"), b"started\n");
    drop(log);
}
