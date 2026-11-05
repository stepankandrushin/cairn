//! A device joining a tree the server already has: its first sync merges what is here, with no base,
//! against everything there. What it has must come through whole, and nothing it holds the same as the
//! server may turn into something new for every other device.

mod common;
use common::*;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// This platform's own xattrs, as the log names them.
#[cfg(target_os = "linux")]
const NS: &str = "linux.xattr";
#[cfg(target_os = "macos")]
const NS: &str = "macos.xattr";

fn seed(label: &str) -> (Sandbox, Server, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    (sb, server, a)
}

#[cfg(unix)]
fn entry(d: &Device, path: &str) -> Entry {
    Picture::of(&d.root)
        .entries
        .get(path.as_bytes())
        .unwrap_or_else(|| panic!("{path} is not on {}", d.name))
        .clone()
}

#[cfg(unix)]
fn xattr(e: &Entry, name: &str) -> Option<String> {
    e.xattrs
        .get(name.as_bytes())
        .map(|v| String::from_utf8_lossy(v).into_owned())
}

#[track_caller]
fn killed_at(d: &Device, point: &str) {
    let run = d.sync_env(&[("CAIRN_FP_CRASH", point)]);
    assert!(run.was_killed(), "never reached {point}: {}", run.text());
}

/// The same bytes with other metadata, as a device synced by something else until now has them: what
/// only this copy has stays and goes to the others, and where both have a value the server's stands,
/// this copy's in the log. Killed once the disk is done and before anything went up, the next run must
/// not take the server's record for the whole of it either. Not on Windows, which holds no xattrs and
/// of a mode only the write bits.
#[cfg(unix)]
#[test]
fn a_joining_device_keeps_the_metadata_it_has_and_the_server_lacks() {
    for kill in [None, Some("sync.applied")] {
        let (sb, server, a) = seed("join-meta");
        a.write("f", "same bytes");
        chmod(&a.path("f"), 0o664);
        set_xattr(&a.path("f"), "user.server", b"1");
        set_xattr(&a.path("f"), "user.both", b"server's");
        a.write("d/g", "also the same");
        a.write("d/only-there", "comes down");
        a.synced();

        let b = Device::new(&sb, &server, "b");
        b.write("f", "same bytes");
        chmod(&b.path("f"), 0o644);
        set_xattr(&b.path("f"), "user.mine", b"2");
        set_xattr(&b.path("f"), "user.both", b"mine");
        b.write("d/g", "also the same");
        set_xattr(&b.path("d"), "user.folder", b"tagged");
        set_xattr(&b.path("d/g"), "user.tag", b"red");
        if let Some(point) = kill {
            killed_at(&b, point);
        }
        let run = b.synced();
        let context = format!("killed at {kill:?}: {}", run.text());

        let f = entry(&b, "f");
        assert_eq!(xattr(&f, "user.mine").as_deref(), Some("2"), "{context}");
        assert_eq!(xattr(&f, "user.server").as_deref(), Some("1"), "{context}");
        assert_eq!(
            xattr(&f, "user.both").as_deref(),
            Some("server's"),
            "the server's stands: {context}"
        );
        assert_eq!(f.mode, 0o664, "the server's stands: {context}");
        assert_eq!(
            xattr(&entry(&b, "d/g"), "user.tag").as_deref(),
            Some("red"),
            "{context}"
        );
        assert_eq!(
            xattr(&entry(&b, "d"), "user.folder").as_deref(),
            Some("tagged"),
            "{context}"
        );

        // What lost is in the log, with the value it had.
        if kill.is_none() {
            let log = b.cairn(&["log"]).expect_ok();
            for line in [
                "note: mode: kept 664 from the server; this device had 644".to_string(),
                format!(
                    "note: record {NS} user.both: kept the server's value; this device had mine"
                ),
            ] {
                assert!(log.stdout.contains(&line), "{line}\n{}", log.text());
            }
        }

        // What only this device had reaches the others.
        a.synced();
        assert_eq!(
            xattr(&entry(&a, "f"), "user.mine").as_deref(),
            Some("2"),
            "{context}"
        );
        assert_eq!(
            xattr(&entry(&a, "d/g"), "user.tag").as_deref(),
            Some("red"),
            "{context}"
        );
        assert_eq!(
            xattr(&entry(&a, "d"), "user.folder").as_deref(),
            Some("tagged"),
            "{context}"
        );
        converge(&[&a, &b]);
    }
}

/// More losers than one commit holds, as a big tree joining after another tool has: each is in the
/// log, the ones the first commit had no room for in the next. Its losers are xattrs, so not on Windows.
#[cfg(unix)]
#[test]
fn a_join_too_big_for_one_commit_logs_every_loser() {
    // Many losers to a file, so that a few hundred files are enough.
    const FILES: usize = 401;
    const CLASHES: usize = 49;
    let (sb, server, a) = seed("join-notes");
    for i in 0..FILES {
        let path = format!("d{}/f{i}", i % 20);
        a.write(&path, format!("{i}"));
        chmod(&a.path(&path), 0o664);
        for k in 0..CLASHES {
            set_xattr(&a.path(&path), &format!("user.k{k}"), b"s");
        }
    }
    a.synced();

    let b = Device::new(&sb, &server, "b");
    for i in 0..FILES {
        let path = format!("d{}/f{i}", i % 20);
        b.write(&path, format!("{i}"));
        chmod(&b.path(&path), 0o644);
        for k in 0..CLASHES {
            set_xattr(&b.path(&path), &format!("user.k{k}"), b"b");
        }
    }
    let run = b.synced();
    assert!(
        run.stdout.contains(" 2 commits pushed"),
        "more than one commit holds: {}",
        run.stdout.lines().last().unwrap_or_default()
    );
    let log = b.cairn(&["log"]).expect_ok();
    let notes = log.stdout.lines().filter(|l| l.contains("note: ")).count();
    assert_eq!(notes, FILES * (1 + CLASHES));
}

/// The server holds one file under several names, and this device has a separate copy at each, as a
/// tool that does not sync hard links leaves them: the same bytes are no conflict, and the names
/// become links of one file here too, with what each copy had of its own. Only a copy with something
/// of its own that the file here would not hold stays itself, a conflict copy listed as one, so that
/// nothing of it goes. Not on Windows, for its modes, xattrs and inode numbers.
#[cfg(unix)]
#[test]
fn separate_equal_copies_of_a_hard_linked_file_become_its_links_here() {
    let (sb, server, a) = seed("join-links");
    a.write("one", "shared");
    chmod(&a.path("one"), 0o644);
    std::fs::hard_link(a.path("one"), a.path("two")).unwrap();
    a.write("d1/x", "across folders");
    std::fs::create_dir(a.path("d2")).unwrap();
    std::fs::hard_link(a.path("d1/x"), a.path("d2/x")).unwrap();
    for (first, second) in [("p", "q"), ("r", "s")] {
        a.write(first, "kept");
        std::fs::hard_link(a.path(first), a.path(second)).unwrap();
    }
    a.synced();

    let b = Device::new(&sb, &server, "b");
    b.write("one", "shared");
    chmod(&b.path("one"), 0o644);
    b.write("two", "shared");
    chmod(&b.path("two"), 0o600);
    b.write("d1/x", "across folders");
    b.write("d2/x", "across folders");
    for name in ["p", "q", "r", "s"] {
        b.write(name, "kept");
    }
    set_xattr(&b.path("q"), "user.own", b"q's");
    set_xattr(&b.path("r"), "user.r", b"r's");
    set_xattr(&b.path("s"), "user.s", b"s's");
    let run = b.synced();

    let ino = |d: &Device, p: &str| std::fs::symlink_metadata(d.path(p)).unwrap().ino();
    for (x, y) in [("one", "two"), ("d1/x", "d2/x"), ("p", "q"), ("r", "s")] {
        assert_eq!(ino(&b, x), ino(&b, y), "{x} {y}: {}", run.text());
    }
    for d in [&a, &b] {
        d.synced();
        for (dir, stem) in [
            ("", "one"),
            ("", "two"),
            ("d1", "x"),
            ("d2", "x"),
            ("", "p"),
            ("", "q"),
            ("", "r"),
        ] {
            assert!(
                d.conflict_copies(dir, stem).is_empty(),
                "{}: {:?}",
                d.name,
                Picture::of(&d.root).names()
            );
        }
        // What one copy had of its own is the file's now, under both names.
        for name in ["p", "q"] {
            assert_eq!(
                xattr(&entry(d, name), "user.own").as_deref(),
                Some("q's"),
                "{} {name}",
                d.name
            );
        }
    }
    let log = b.cairn(&["log"]).expect_ok();
    assert!(
        log.stdout
            .contains("note: mode: kept 644 from the server; this device's copy at two had 600"),
        "{}",
        log.text()
    );

    // Two copies with something of their own each: one is the file, the other stays itself, whole,
    // and listed.
    let kept = b.conflict_copies("", "s");
    assert_eq!(kept.len(), 1, "{:?}", Picture::of(&b.root).names());
    assert_eq!(
        xattr(&entry(&b, &kept[0]), "user.s").as_deref(),
        Some("s's")
    );
    assert_eq!(xattr(&entry(&b, "s"), "user.r").as_deref(), Some("r's"));
    let inbox = a.cairn(&["conflicts"]).expect_ok();
    assert!(
        inbox
            .stdout
            .contains(&format!("{}: the competing version of ", kept[0])),
        "{}",
        inbox.text()
    );
    let p = converge(&[&a, &b]);
    assert_eq!(
        p.entries[b"one".as_slice()].link_group,
        p.entries[b"two".as_slice()].link_group
    );
}

/// Other bytes at a name the server holds: the copy kept beside it is a conflict like any other, in
/// everyone's inbox, even when it goes up in a later run than the one that moved it aside.
#[test]
fn a_conflict_copy_made_at_a_join_is_in_the_inbox() {
    for kill in [None, Some("sync.applied")] {
        let (sb, server, a) = seed("join-inbox");
        a.write("doc.txt", "the server's");
        a.synced();

        let b = Device::new(&sb, &server, "b");
        b.write("doc.txt", "this device's");
        if let Some(point) = kill {
            killed_at(&b, point);
        }
        let run = b.synced();
        if kill.is_none() {
            assert!(run.stdout.contains(" 1 conflict(s)"), "{}", run.text());
        }
        let copies = b.conflict_copies("", "doc");
        assert_eq!(copies.len(), 1, "killed at {kill:?}");
        assert_eq!(b.read(&copies[0]), b"this device's");
        assert_eq!(b.read("doc.txt"), b"the server's");
        a.synced();
        let inbox = a.cairn(&["conflicts"]).expect_ok();
        let line = format!("{}: the competing version of doc.txt", copies[0]);
        assert!(
            inbox.stdout.contains(&line),
            "killed at {kill:?}: {}",
            inbox.text()
        );
        converge(&[&a, &b]);
    }
}

/// On a filesystem that takes a decomposed name for its composed form, or one name for another in
/// another case, as APFS does: a new name here that is the server's in another form must not go up as a
/// second name, or every device that keeps names as bytes holds both, folders whole. It waits, reported
/// with both names, until it is renamed to the server's form, and then the two merge.
#[cfg(target_os = "macos")]
#[test]
fn a_new_name_this_filesystem_takes_for_the_servers_waits_until_renamed() {
    let (sb, server, a) = seed("join-nfd");
    let nfc = "Caf\u{e9}";
    let nfd = "Cafe\u{301}";
    a.write(&format!("{nfc}/menu.txt"), "soup");
    a.write("Readme.txt", "read me");
    a.synced();
    let names_on = |d: &Device| -> Vec<Vec<u8>> {
        let mut names: Vec<Vec<u8>> = std::fs::read_dir(&d.root)
            .unwrap()
            .map(|e| {
                std::os::unix::ffi::OsStrExt::as_bytes(e.unwrap().file_name().as_os_str()).to_vec()
            })
            .filter(|n| n != b".cairn")
            .collect();
        names.sort();
        names
    };
    let on_a = names_on(&a);

    let b = Device::new(&sb, &server, "b");
    b.write(&format!("{nfd}/menu.txt"), "soup");
    b.write(&format!("{nfd}/only-here.txt"), "mine");
    b.write("README.txt", "read me");
    let run = b.synced();
    for name in [nfd, "README.txt"] {
        let line = run
            .stdout
            .lines()
            .find(|l| l.starts_with(&format!("skipped: {name}: ")))
            .unwrap_or_else(|| panic!("{name}: {}", run.text()));
        assert!(line.contains("rename"), "{line}");
    }
    for (what, out) in [
        ("status", b.cairn(&["status"]).expect_ok()),
        ("dry run", b.cairn(&["sync", "--dry-run"]).expect_ok()),
    ] {
        assert!(
            out.stdout.contains(&format!("skipped: {nfd}: ")),
            "{what}: {}",
            out.text()
        );
    }
    // Nothing of it reached the server.
    a.synced();
    assert_eq!(names_on(&a), on_a);
    assert!(!a.exists(&format!("{nfc}/only-here.txt")));

    std::fs::rename(b.path(nfd), b.path(nfc)).unwrap();
    std::fs::rename(b.path("README.txt"), b.path("Readme.txt")).unwrap();
    b.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(names_on(&a), on_a);
    assert_eq!(
        p.entries[format!("{nfc}/only-here.txt").as_bytes()].content,
        b"mine"
    );
    assert!(b.conflict_copies("", "Readme").is_empty());
    assert!(b.conflict_copies(nfc, "menu").is_empty());
}
