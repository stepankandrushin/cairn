//! Milestone 1's exit criterion: a tree with every metadata type round-trips byte-exact and
//! metadata-exact through the server.
//!
//! Unprivileged, this covers what an unprivileged user can set. Run as root (`scripts/ci.sh` does) it
//! also covers owners, setuid, capabilities, `trusted.*` and device nodes.

mod common;
use common::*;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, FileExt};
use std::os::unix::net::UnixListener;
use std::path::Path;

/// Bytes with a seed, incompressible enough to chunk.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// One of everything in PLAN.md's metadata and filename tables that Linux has.
fn build(root: &Path) {
    let p = |rel: &str| root.join(rel);
    std::fs::create_dir_all(p("dir/nested/deep")).unwrap();
    std::fs::create_dir_all(p("empty")).unwrap();

    // Content: empty, small, multi-chunk.
    std::fs::write(p("empty-file"), b"").unwrap();
    std::fs::write(p("small.txt"), b"hello\n").unwrap();
    std::fs::write(p("dir/big.bin"), noise(24 * 1024 * 1024, 7)).unwrap();

    // Every permission an owner can set, including odd ones.
    std::fs::write(p("script.sh"), b"#!/bin/sh\n").unwrap();
    chmod(&p("script.sh"), 0o755);
    std::fs::write(p("private"), b"secret").unwrap();
    chmod(&p("private"), 0o600);
    std::fs::write(p("sticky"), b"x").unwrap();
    chmod(&p("sticky"), 0o1644);
    std::fs::create_dir(p("setgid-dir")).unwrap();
    chmod(&p("setgid-dir"), 0o2775);
    std::fs::create_dir(p("sticky-dir")).unwrap();
    chmod(&p("sticky-dir"), 0o1777);
    std::fs::create_dir(p("read-only-dir")).unwrap();
    std::fs::write(p("read-only-dir/inside"), b"in a read-only dir").unwrap();
    chmod(&p("read-only-dir"), 0o555);

    // Symlinks: relative, absolute, to a directory, broken, odd bytes, leaving the root.
    symlink("small.txt", p("link-rel")).unwrap();
    symlink("/etc/hostname", p("link-abs")).unwrap();
    symlink("dir/nested", p("link-dir")).unwrap();
    symlink("no/such/target", p("link-broken")).unwrap();
    symlink("../../outside", p("link-leaving")).unwrap();
    symlink(OsStr::from_bytes(b"bad-\xff-target"), p("link-bytes")).unwrap();
    set_mtime(&p("link-rel"), 1_000_000_000, 123_456_789);

    // Hard links, across directories, and a pair apart from it.
    std::fs::write(p("hard-a"), b"same inode").unwrap();
    std::fs::hard_link(p("hard-a"), p("dir/hard-b")).unwrap();
    std::fs::hard_link(p("hard-a"), p("dir/nested/hard-c")).unwrap();
    std::fs::write(p("other-a"), b"same inode too").unwrap();
    std::fs::hard_link(p("other-a"), p("other-b")).unwrap();

    // Sparse: holes in the middle and at the end, and a file that is only a hole.
    let sparse = std::fs::File::create(p("sparse")).unwrap();
    sparse.set_len(1 << 40).unwrap();
    sparse.write_all_at(&[1u8; 4096], 1 << 20).unwrap();
    sparse.write_all_at(&noise(4096, 3), 1 << 30).unwrap();
    drop(sparse);
    std::fs::File::create(p("all-hole"))
        .unwrap()
        .set_len(1 << 30)
        .unwrap();

    // Names as bytes: not UTF-8, newline, spaces, a leading dash, near the length limit, case pairs,
    // and the names other sync tools drop.
    std::fs::write(root.join(OsStr::from_bytes(b"not-utf8-\xff\xfe")), b"latin").unwrap();
    std::fs::write(p("new\nline"), b"nl").unwrap();
    std::fs::write(p("with spaces and :colons:"), b"sp").unwrap();
    std::fs::write(p("-dash"), b"d").unwrap();
    std::fs::write(p(&"n".repeat(255)), b"long").unwrap();
    std::fs::write(p("File.txt"), b"upper").unwrap();
    std::fs::write(p("file.txt"), b"lower").unwrap();
    for ignored_elsewhere in [
        "node_modules",
        ".venv",
        ".git",
        "__pycache__",
        "build",
        "target",
        "desktop.ini",
    ] {
        std::fs::create_dir_all(p(ignored_elsewhere)).unwrap();
        std::fs::write(p(ignored_elsewhere).join("kept"), b"kept").unwrap();
    }
    std::fs::write(p(".cairn-looks-like-ours"), b"a user's file").unwrap();
    std::fs::create_dir_all(p("dir/.cairn")).unwrap();
    std::fs::write(p("dir/.cairn/not-the-root"), b"only the root's is reserved").unwrap();

    // xattrs, with binary values; POSIX ACLs are xattrs, set here by their raw encoding.
    std::fs::write(p("xattrs"), b"x").unwrap();
    set_xattr(&p("xattrs"), "user.text", b"value");
    set_xattr(&p("xattrs"), "user.binary", &[0, 255, 1, 254]);
    // Near what ext4 holds for one file with a 4K block.
    set_xattr(&p("xattrs"), "user.big", &noise(2 * 1024, 11));
    set_xattr(&p("dir"), "user.on-a-dir", b"d");
    std::fs::write(p("acl"), b"a").unwrap();
    chmod(&p("acl"), 0o640);
    let acl: &[u8] = &[
        2, 0, 0, 0, // version
        1, 0, 6, 0, 0xff, 0xff, 0xff, 0xff, // user:: rw-
        2, 0, 4, 0, 0xe8, 0x03, 0, 0, // user:1000: r--
        4, 0, 4, 0, 0xff, 0xff, 0xff, 0xff, // group:: r--
        0x10, 0, 4, 0, 0xff, 0xff, 0xff, 0xff, // mask:: r--
        0x20, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, // other:: ---
    ];
    set_xattr(&p("acl"), "system.posix_acl_access", acl);

    // mtimes to the nanosecond, old and pre-epoch, last, deepest first, once all writes are done.
    set_mtime(&p("small.txt"), 1_000_000_000, 999_999_999);
    set_mtime(&p("empty-file"), 0, 1);
    set_mtime(&p("private"), -1_000_000, 500);
    set_mtime(&p("dir/nested/deep"), 1_200_000_000, 42);
    set_mtime(&p("empty"), 1_300_000_000, 7);
    set_mtime(&p("read-only-dir"), 1_400_000_000, 8);
    set_mtime(&p("dir"), 1_500_000_000, 9);
}

fn build_as_root(root: &Path) {
    let p = |rel: &str| root.join(rel);
    use rustix::fs::{chownat, AtFlags, Gid, Uid, CWD};
    // Other owners, including on a symlink and a directory.
    std::fs::write(p("owned"), b"o").unwrap();
    chownat(
        CWD,
        p("owned"),
        Some(Uid::from_raw(1234)),
        Some(Gid::from_raw(5678)),
        AtFlags::empty(),
    )
    .unwrap();
    symlink("x", p("owned-link")).unwrap();
    chownat(
        CWD,
        p("owned-link"),
        Some(Uid::from_raw(4321)),
        Some(Gid::from_raw(8765)),
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
    std::fs::create_dir(p("owned-dir")).unwrap();
    chownat(
        CWD,
        p("owned-dir"),
        Some(Uid::from_raw(2000)),
        Some(Gid::from_raw(2000)),
        AtFlags::empty(),
    )
    .unwrap();
    // setuid and setgid, set after chown, which clears them.
    std::fs::write(p("setuid"), b"s").unwrap();
    chownat(
        CWD,
        p("setuid"),
        Some(Uid::from_raw(1234)),
        None,
        AtFlags::empty(),
    )
    .unwrap();
    chmod(&p("setuid"), 0o4755);
    std::fs::write(p("setgid"), b"s").unwrap();
    chmod(&p("setgid"), 0o2755);
    // A mode the owner has no read bit for.
    std::fs::write(p("no-access"), b"only root reads this").unwrap();
    chmod(&p("no-access"), 0o000);
    // Linux capabilities: cap_net_bind_service, effective, in raw v2 encoding.
    std::fs::write(p("capable"), b"c").unwrap();
    let cap: &[u8] = &[1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    set_xattr(&p("capable"), "security.capability", cap);
    set_xattr(&p("capable"), "trusted.mine", b"t");
    set_xattr(&p("xattrs"), "trusted.also", b"t2");
    set_mtime(&p("no-access"), 1_111_111_111, 111);
}

#[test]
fn every_metadata_type_round_trips_through_the_server() {
    let sb = Sandbox::on_disk("roundtrip");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    build(&a.root);
    if is_root() {
        build_as_root(&a.root);
    }
    // Directory times are final only once the state directory and everything in it exist.
    let first = a.synced();
    let before = Picture::of(&a.root);
    assert!(before.entries.len() > 40, "the fixture should be rich");
    assert!(
        first.stdout.contains("synced at sequence"),
        "{}",
        first.text()
    );

    // A second sync of an unchanged tree does nothing at all.
    let again = a.synced();
    assert!(
        again.stdout.contains("0 steps applied here, 0 commits"),
        "{}",
        again.text()
    );
    assert_eq!(
        Picture::of(&a.root).diff(&before, Ignore::default()),
        Vec::<String>::new()
    );

    // Exact restore into a new folder, from the server's copy only.
    let dest = sb.path("restored");
    let exported = a.cairn(&["export", "--exact", dest.to_str().unwrap()]);
    let after = Picture::of(&dest);
    let ignore = Ignore {
        owner: !is_root(),
        ..Ignore::default()
    };
    let diff = before.diff(&after, ignore);
    assert_eq!(
        diff,
        Vec::<String>::new(),
        "export output: {}",
        exported.text()
    );

    // Sparse files stayed sparse rather than inflating.
    let sparse = std::fs::symlink_metadata(dest.join("sparse")).unwrap();
    assert_eq!(std::os::unix::fs::MetadataExt::size(&sparse), 1 << 40);
    assert!(
        std::os::unix::fs::MetadataExt::blocks(&sparse) < 4096,
        "inflated on download"
    );

    // The server's data passes its own check.
    let mut server = server;
    let report = server.fsck();
    assert!(report.contains("clean"), "{report}");
}

#[test]
fn a_second_device_gets_the_same_tree_under_its_default_policy() {
    let sb = Sandbox::on_disk("roundtrip-two");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    build(&a.root);
    a.synced();
    let before = Picture::of(&a.root);

    // setgid is applied only by explicit policy; this fixture has a setgid directory.
    let b = Device::with_flags(&sb, &server, "b", &["--setid"]);
    b.synced();
    let after = Picture::of(&b.root);
    let diff = before.diff(
        &after,
        Ignore {
            owner: !is_root(),
            ..Ignore::default()
        },
    );
    assert_eq!(diff, Vec::<String>::new());

    // A change comes back the other way, and a sync after it is quiet on both.
    std::fs::write(b.path("small.txt"), b"edited on b\n").unwrap();
    b.synced();
    a.synced();
    assert_eq!(a.read("small.txt"), b"edited on b\n");
    assert!(a
        .synced()
        .stdout
        .contains("0 steps applied here, 0 commits"));
    assert!(b
        .synced()
        .stdout
        .contains("0 steps applied here, 0 commits"));
}

#[test]
fn what_cannot_be_synced_is_named_with_a_reason_and_an_action() {
    let sb = Sandbox::on_disk("skips");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    a.write("keep", b"k");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        a.path("fifo").as_path(),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    let _socket = UnixListener::bind(a.path("socket")).unwrap();

    let run = a.synced();
    for (name, reason) in [
        ("fifo", "named pipes are not synced"),
        ("socket", "sockets are not synced"),
    ] {
        assert!(
            run.stdout.lines().any(|l| l.starts_with("skipped: ")
                && l.contains(name)
                && l.contains(reason)
                && l.contains("move it out")),
            "{}",
            run.text()
        );
    }
    let b = Device::new(&sb, &server, "b");
    b.synced();
    assert!(b.exists("keep"));
    assert!(!b.exists("fifo") && !b.exists("socket"));
}

#[test]
fn an_unreadable_file_is_reported_and_not_deleted() {
    if is_root() {
        return; // root reads everything
    }
    let sb = Sandbox::on_disk("unreadable");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    a.write("locked", b"content");
    a.write("locked-dir/inside", b"content");
    a.synced();

    chmod(&a.path("locked"), 0o000);
    chmod(&a.path("locked-dir"), 0o000);
    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());
    assert!(
        run.stdout.contains("waiting for permission"),
        "{}",
        run.text()
    );

    // Still on the server, intact, for everyone else.
    let b = Device::new(&sb, &server, "b");
    b.synced();
    assert_eq!(b.read("locked"), b"content");
    assert_eq!(b.read("locked-dir/inside"), b"content");

    // Permission back: it picks up where it left off.
    chmod(&a.path("locked"), 0o644);
    chmod(&a.path("locked-dir"), 0o755);
    a.synced();
}
