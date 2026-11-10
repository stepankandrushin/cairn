//! A real full disk, not an injected one: a size-limited tmpfs in a private user and mount namespace.
//! Skipped where unprivileged namespaces are forbidden, except in CI (`common::skip`); the injected
//! matrix still runs there. Not built on Windows, which has neither.
#![cfg(unix)]

mod common;
use common::*;
use std::process::Command;

/// Re-run this test binary's `test` inside a namespace with `dir` mounted as a small tmpfs.
fn on_small_disk(test: &str, dir: &std::path::Path, megabytes: u32) -> Option<bool> {
    in_namespace(test, Some((dir, &format!("size={megabytes}m"))))
}

fn inner_dir() -> std::path::PathBuf {
    std::env::var_os("CAIRN_INNER")
        .expect("run through the namespace wrapper")
        .into()
}

const SMALL_DISK_MB: u32 = 256;

/// Takes up all but 16 MiB of the small disk; deleting it is how room appears again.
fn fill(dir: &std::path::Path) -> std::path::PathBuf {
    let ballast = dir.join("ballast");
    std::fs::write(
        &ballast,
        vec![1u8; (SMALL_DISK_MB as usize - 16) * 1024 * 1024],
    )
    .unwrap();
    ballast
}

#[test]
fn a_client_disk_filling_up_loses_nothing() {
    let sb = Sandbox::on_disk("full-client");
    let dir = sb.path("small");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = on_small_disk("inner_client_disk", &dir, SMALL_DISK_MB) {
        assert!(ok, "see the output above");
    }
}

#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_client_disk() {
    let small = inner_dir();
    let ballast = fill(&small);
    let sb = Sandbox::fast("full-client-outer");
    let server = Server::start(sb.path("server"));
    let writer = Device::new(&sb, &server, "writer");
    // The client lives on the small disk.
    let reader_root = small.join("reader");
    std::fs::create_dir(&reader_root).unwrap();
    let token = server.token_file.to_str().unwrap().to_string();
    run(
        &[
            "init",
            "--server",
            &server.url,
            "--device-name",
            "reader",
            "--token-file",
            &token,
            reader_root.to_str().unwrap(),
        ],
        &[],
    )
    .expect_ok();
    let reader = Device {
        root: reader_root,
        name: "reader".into(),
    };

    writer.write("small-one", token::make(1, 1000));
    writer.synced();
    reader.synced();
    let held = reader.read("small-one");
    reader.write("local-work", token::make(2, 2000));

    // Far more than fits.
    writer.write("big", token::make(3, 64 * 1024 * 1024));
    writer.write("small-one", token::make(4, 1000));
    writer.synced();

    let run = reader.sync();
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(
        run.stderr.contains("full"),
        "a full disk is named as such: {}",
        run.text()
    );

    // No torn file at any synced path, and what was there is still there.
    let p = Picture::of(&reader.root);
    let found = token::all_in(&p);
    assert!(found.contains(&2), "local work survives");
    assert!(
        !p.entries.contains_key(b"big".as_slice()),
        "no partial big file"
    );
    assert!(found.contains(&1) || found.contains(&4));
    let _ = held;

    // Room appears: it finishes on its own, nothing lost.
    std::fs::remove_file(&ballast).unwrap();
    reader.synced();
    let p = converge(&[&reader, &writer]);
    let found = token::all_in(&p);
    for t in [2, 3, 4] {
        assert!(found.contains(&t), "version {t} lost");
    }
}

#[test]
fn a_server_disk_filling_up_refuses_commits_and_loses_nothing() {
    let sb = Sandbox::on_disk("full-server");
    let dir = sb.path("small");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = on_small_disk("inner_server_disk", &dir, SMALL_DISK_MB) {
        assert!(ok, "see the output above");
    }
}

#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_server_disk() {
    let small = inner_dir();
    let ballast = fill(&small);
    let sb = Sandbox::fast("full-server-outer");
    let mut server = Server::start(small.join("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");

    a.write("first", token::make(1, 2000));
    a.synced();
    b.synced();

    a.write("too-big", token::make(2, 64 * 1024 * 1024));
    let run = a.sync();
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains("disk is full"), "{}", run.text());

    // Everything acknowledged is intact, and the half-done upload is not visible.
    let report = server.fsck();
    assert!(report.contains("clean"), "{report}");
    b.synced();
    assert_eq!(b.read("first"), token::make(1, 2000));
    assert!(!b.exists("too-big"));
    // The client still has its file.
    assert_eq!(a.read("too-big"), token::make(2, 64 * 1024 * 1024));

    std::fs::remove_file(&ballast).unwrap();
    server.restart();
    a.synced();
    b.synced();
    assert_eq!(b.read("too-big"), token::make(2, 64 * 1024 * 1024));
    let report = server.fsck();
    assert!(report.contains("clean"), "{report}");
}

#[test]
fn a_disk_with_no_room_to_ask_what_arrived_stops_and_then_pushes_the_change() {
    let sb = Sandbox::on_disk("full-probe");
    let dir = sb.path("small");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = in_namespace(
        "inner_no_room_to_ask",
        Some((&dir, "size=64m,nr_inodes=400")),
    ) {
        assert!(ok, "see the output above");
    }
}

/// A device whose root is on the small disk.
fn device_on(server: &Server, root: std::path::PathBuf, name: &str) -> Device {
    std::fs::create_dir(&root).unwrap();
    let token = server.token_file.to_str().unwrap().to_string();
    run(
        &[
            "init",
            "--server",
            &server.url,
            "--device-name",
            name,
            "--token-file",
            &token,
            root.to_str().unwrap(),
        ],
        &[],
    )
    .expect_ok();
    Device {
        root,
        name: name.into(),
    }
}

/// A sync of `d` paused at `point` while `fill` takes what room is left, then let go.
fn sync_while_filling(sb: &Sandbox, d: &Device, point: &str, fill: impl FnOnce()) -> Run {
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("{point}:{}", gate.display());
    std::thread::scope(|s| {
        let syncing = s.spawn(|| d.sync_env(&[("CAIRN_FP_PAUSE", &pause)]));
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed().as_secs() < 60, "never reached {point}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        fill();
        std::fs::write(gate.join("go"), "").unwrap();
        syncing.join().unwrap()
    })
}

/// Empty files in `dir` until no inode is left.
fn take_every_inode(dir: &std::path::Path) {
    std::fs::create_dir(dir).unwrap();
    let mut made = 0;
    while std::fs::File::create(dir.join(made.to_string())).is_ok() {
        made += 1;
    }
    assert!(made > 0);
}

/// After a kill, what arrived whole with something else on disk than its record is asked of the
/// filesystem on a scratch entry. With no inode left for one, the sync stops and says the disk is full:
/// what is there standing for what arrived would keep a change made here off the server for good. Once
/// there is room, the change goes up.
#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_no_room_to_ask() {
    let small = inner_dir();
    let sb = Sandbox::fast("full-probe-outer");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = device_on(&server, small.join("b"), "b");
    for i in 0..14 {
        a.write(&format!("fill/{i}"), format!("{i}"));
    }
    a.write("x", "v1");
    chmod(&a.path("x"), 0o644);
    a.synced();
    b.synced();
    a.write("x", "v2");
    a.synced();
    // Killed with the new version in place and the run unfinished, then a chmod here in that moment.
    let killed = b.sync_env(&[("CAIRN_FP_CRASH", "apply.replace.renamed")]);
    assert!(killed.was_killed(), "{}", killed.text());
    chmod(&b.path("x"), 0o600);

    // The next sync's scan is done when the last inode goes.
    let ballast = small.join("ballast");
    let run = sync_while_filling(&sb, &b, "sync.scanned", || take_every_inode(&ballast));
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains("this disk is full"), "{}", run.text());

    std::fs::remove_dir_all(&ballast).unwrap();
    b.synced();
    a.synced();
    use std::os::unix::fs::PermissionsExt;
    let mode = |d: &Device| std::fs::metadata(d.path("x")).unwrap().permissions().mode() & 0o777;
    assert_eq!(a.read("x"), b"v2");
    assert_eq!(mode(&a), 0o600, "the chmod here never went up");
}

#[test]
fn an_xattr_the_disk_has_no_room_for_stays_on_every_device() {
    let sb = Sandbox::on_disk("full-xattr");
    let dir = sb.path("small");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = in_namespace(
        "inner_xattr_on_a_full_disk",
        Some((&dir, "size=24m,nr_inodes=3000")),
    ) {
        assert!(ok, "see the output above");
    }
}

/// The disk fills while a file with an xattr is on its way: the temporary has its content and not the
/// xattr, which is no limit of this filesystem but a full disk. The file is not placed, the sync says the
/// disk is full, and once there is room the file arrives with its xattr; the device it came from keeps it.
#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_xattr_on_a_full_disk() {
    let small = inner_dir();
    let sb = Sandbox::fast("full-xattr-outer");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = device_on(&server, small.join("b"), "b");
    for i in 0..14 {
        a.write(&format!("fill/{i}"), format!("{i}"));
    }
    a.write("x", "v1");
    set_xattr(&a.path("x"), "user.k", b"vvvvvvvvvv");
    a.synced();
    b.synced();
    let xattrs = |d: &Device| Picture::of(&d.root).entries[b"x".as_slice()].xattrs.clone();
    if xattrs(&b).is_empty() {
        skip("no user xattrs on this tmpfs");
        return;
    }
    a.write("x", "v2");
    a.synced();

    // Data, then inodes, then what is left of tmpfs's room for xattrs, which inodes share.
    let ballast = small.join("ballast");
    let run = sync_while_filling(&sb, &b, "apply.temp.written", || {
        std::fs::create_dir(&ballast).unwrap();
        let mut big = std::fs::File::create(ballast.join("big")).unwrap();
        for size in [1 << 20, 4096, 512] {
            use std::io::Write;
            while big.write_all(&vec![0u8; size]).is_ok() {}
        }
        let mut made = 0;
        while std::fs::File::create(ballast.join(made.to_string())).is_ok() {
            made += 1;
        }
        let first = ballast.join("0");
        let mut k = 0;
        for size in [512, 64, 8, 1] {
            while rustix::fs::setxattr(
                &first,
                format!("user.p{k}").as_str(),
                &vec![b'p'; size],
                rustix::fs::XattrFlags::empty(),
            )
            .is_ok()
            {
                k += 1;
            }
        }
    });
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains("this disk is full"), "{}", run.text());

    std::fs::remove_dir_all(&ballast).unwrap();
    b.synced();
    a.synced();
    b.synced();
    assert_eq!(b.read("x"), b"v2");
    assert_eq!(xattrs(&b).get(b"user.k".as_slice()).map(Vec::len), Some(10));
    assert_eq!(
        xattrs(&a).get(b"user.k".as_slice()).map(Vec::len),
        Some(10),
        "a lost its xattr"
    );
}

#[test]
fn nothing_is_written_into_a_filesystem_mounted_in_the_tree() {
    let sb = Sandbox::on_disk("mount-in-tree");
    let dir = sb.path("unused");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = in_namespace("inner_mount_in_the_tree", Some((&dir, "size=1m"))) {
        assert!(ok, "see the output above");
    }
}

#[track_caller]
fn sh(program: &str, args: &[&str]) {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The scan does not cross into another filesystem mounted in the tree, so the apply does not either:
/// written there, a file would be one no scan could see, and every sync would stop at it. It waits,
/// reported, and arrives once the mount is gone.
#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_mount_in_the_tree() {
    let sb = Sandbox::fast("mount-in-tree-outer");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..14 {
        a.write(&format!("fill/{i}"), format!("{i}"));
    }
    a.write("m/k", "k");
    a.synced();
    b.synced();
    // A person mounts another filesystem on the folder.
    let m = b.path("m");
    let m = m.to_str().unwrap();
    sh("mount", &["-t", "tmpfs", "-o", "size=8m", "tmpfs", m]);
    let run = b.sync();
    assert!(
        run.text().contains("sync does not cross mounts"),
        "{}",
        run.text()
    );
    a.write("m/x", "into the mount");
    a.synced();
    let run = b.sync();
    assert!(!b.exists("m/x"), "written into the mount: {}", run.text());
    sh("umount", &[m]);
    b.synced();
    assert_eq!(b.read("m/x"), b"into the mount");
    assert_eq!(b.read("m/k"), b"k");
}

/// Where unprivileged namespaces are forbidden the tests above skip, but not in CI: there `NO_SKIP`
/// turns the skip into a failure that names what was missing. An `unshare` that refuses, first on PATH,
/// stands for a runner image that confines namespaces in a way the workflow's sysctl does not undo.
#[test]
fn a_skip_fails_where_every_test_must_run() {
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        assert!(no_skip(), "GitHub's CI must set {NO_SKIP}=1");
    }
    let sb = Sandbox::on_disk("no-skip");
    let bin = sb.path("bin");
    std::fs::create_dir(&bin).unwrap();
    let unshare = bin.join("unshare");
    std::fs::write(
        &unshare,
        "#!/bin/sh\necho 'unshare: write failed /proc/self/uid_map: Operation not permitted' >&2\nexit 1\n",
    )
    .unwrap();
    chmod(&unshare, 0o755);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let outer = |no_skip: Option<&str>| {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "a_client_disk_filling_up_loses_nothing",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PATH", &path)
        .env_remove(NO_SKIP);
        if let Some(v) = no_skip {
            cmd.env(NO_SKIP, v);
        }
        let out = cmd.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    };

    let (ok, text) = outer(Some("1"));
    assert!(!ok, "{text}");
    assert!(text.contains("1 failed"), "{text}");
    assert!(
        text.contains("no unprivileged namespaces here")
            && text.contains("Operation not permitted")
            && text.contains("CAIRN_NO_SKIP says every test must run here"),
        "{text}"
    );

    // On a contributor's machine, it says so and passes.
    for no_skip in [None, Some(""), Some("0")] {
        let (ok, text) = outer(no_skip);
        assert!(ok, "{no_skip:?}: {text}");
        assert!(text.contains("1 passed"), "{no_skip:?}: {text}");
        assert!(
            text.contains("no unprivileged namespaces here")
                && text.contains("Operation not permitted")
                && text.contains(": skipped"),
            "{no_skip:?}: {text}"
        );
    }
}

/// The rewrite that takes back a state's free pages, on a disk with less room than it needs: told
/// beforehand, and when told nothing, by the disk itself, partway through. Either way the sync it came
/// after stands as reported, the state is as it was, and what a rewrite that failed wrote does not keep
/// the room it took.
#[test]
fn a_compaction_on_a_full_disk_leaves_the_state_and_the_sync_as_they_were() {
    let sb = Sandbox::on_disk("full-compact");
    let dir = sb.path("small");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = on_small_disk("inner_compaction_on_a_full_disk", &dir, 64) {
        assert!(ok, "see the output above");
    }
}

fn free_on(dir: &std::path::Path) -> u64 {
    let s = rustix::fs::statvfs(dir).unwrap();
    s.f_bavail * s.f_frsize
}

#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_compaction_on_a_full_disk() {
    let small = inner_dir();
    let sb = Sandbox::fast("full-compact-outer");
    let server = Server::start(sb.path("server"));
    let writer = Device::new(&sb, &server, "writer");
    let reader = device_on(&server, small.join("reader"), "reader");
    for i in 0..2000 {
        writer.write(&format!("d{}/f{i}", i % 20), format!("file {i}"));
    }
    writer.synced();
    reader.synced();
    for i in 200..2000 {
        remove_file(&writer.path(&format!("d{}/f{i}", i % 20)));
    }
    writer.cairn(&["sync", "--allow-mass-delete"]).expect_ok();
    reader.synced();
    reader.synced();
    let state = reader.path(".cairn/state.sqlite");
    let (count, free) = state_pages(&state);
    assert!(free * 2 > count, "not mostly free: {free} of {count} pages");
    let kept = state_everything(&state);
    let live = (count - free) * 4096;

    // Half of what the rewrite's WAL takes is left, and the WAL emptied first: what the join wrote into
    // it, more than the state now holds, would otherwise take the rewrite without the disk growing.
    rusqlite::Connection::open(&state)
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    let ballast = small.join("ballast");
    let left = live / 2;
    std::fs::write(&ballast, vec![1u8; (free_on(&small) - left) as usize]).unwrap();
    let room = free_on(&small);
    assert!(room < live, "{room} free, {live} in use");

    // Told: not tried, and said.
    let run = reader
        .sync_env(&[("CAIRN_FP_TUNE", "state.compact_floor=0")])
        .expect_ok();
    assert!(run.stdout.contains("synced at sequence"), "{}", run.text());
    assert!(
        run.stderr.contains("not compacted") && run.stderr.contains("free"),
        "{}",
        run.text()
    );
    assert_eq!(state_pages(&state), (count, free));
    same_state(
        &state_everything(&state),
        &kept,
        "not tried for want of room",
    );

    // Not told: the disk fills partway through the rewrite.
    let run = reader
        .sync_env(&[(
            "CAIRN_FP_TUNE",
            "state.compact_floor=0,state.compact_room=0",
        )])
        .expect_ok();
    assert!(run.stdout.contains("synced at sequence"), "{}", run.text());
    assert!(
        run.stderr.contains("not compacted, and stays as it was") && run.stderr.contains("full"),
        "{}",
        run.text()
    );
    assert_eq!(state_pages(&state), (count, free));
    same_state(
        &state_everything(&state),
        &kept,
        "the disk filled during it",
    );
    assert_eq!(
        wal_len(&state),
        0,
        "what the failed rewrite wrote is let go"
    );
    assert!(
        free_on(&small) + 64 * 1024 >= room,
        "{} free after, {room} before",
        free_on(&small)
    );

    // Room again: the next sync's compaction goes through.
    std::fs::remove_file(&ballast).unwrap();
    let run = reader
        .sync_env(&[("CAIRN_FP_TUNE", "state.compact_floor=0")])
        .expect_ok();
    assert!(!run.stderr.contains("compact"), "{}", run.text());
    assert_eq!(state_pages(&state).1, 0);
    same_state(&state_everything(&state), &kept, "compacted");
    converge(&[&reader, &writer]);
}

/// A state of version 1 moved to this build's layout on a disk short of room: told before it starts,
/// and the sync stops with the reason; untold, the disk fills partway through, and the state is version
/// 1 as it was, its WAL let go. Room again, the next sync moves it and holds the same.
#[test]
fn a_move_of_the_state_on_a_full_disk_leaves_version_1_as_it_was() {
    let sb = Sandbox::on_disk("full-migrate");
    let dir = sb.path("small");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = on_small_disk("inner_migration_on_a_full_disk", &dir, 64) {
        assert!(ok, "see the output above");
    }
}

#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_migration_on_a_full_disk() {
    use cairn_engine::state::legacy;
    let small = inner_dir();
    let sb = Sandbox::fast("full-migrate-outer");
    let server = Server::start(sb.path("server"));
    let writer = Device::new(&sb, &server, "writer");
    let reader = device_on(&server, small.join("reader"), "reader");
    for i in 0..3000 {
        writer.write(&format!("d{}/f{i}", i % 20), format!("file {i}"));
    }
    writer.synced();
    reader.synced();
    let state = reader.path(".cairn/state.sqlite");
    legacy::to_v1(&state).unwrap();
    let held = legacy::contents(&state).unwrap();
    let kept = state_everything(&state);
    let (count, free) = state_pages(&state);
    let live = (count - free) * 4096;

    // Less than the move asks for.
    let ballast = small.join("ballast");
    std::fs::write(&ballast, vec![1u8; (free_on(&small) - live / 2) as usize]).unwrap();
    let run = reader.sync();
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr.contains("no sync runs here until there is room") && run.stderr.contains("free"),
        "{}",
        run.text()
    );
    assert_eq!(legacy::version(&state).unwrap(), 1);
    same_state(
        &state_everything(&state),
        &kept,
        "not tried for want of room",
    );

    // Not told, and its pages spilling into the WAL: the disk fills partway through.
    let room = free_on(&small);
    let more = small.join("more");
    std::fs::write(&more, vec![1u8; (room - 96 * 1024) as usize]).unwrap();
    let run = reader.sync_env(&[("CAIRN_FP_TUNE", "state.migrate_room=0,state.cache_kib=64")]);
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr.contains("ran out of room") && run.stderr.contains("stays as it was"),
        "{}",
        run.text()
    );
    assert_eq!(legacy::version(&state).unwrap(), 1);
    same_state(
        &state_everything(&state),
        &kept,
        "the disk filled during it",
    );
    assert_eq!(wal_len(&state), 0, "what the failed move wrote is let go");
    std::fs::remove_file(&more).unwrap();
    assert!(
        free_on(&small) + 64 * 1024 >= room,
        "{} free after, {room} before",
        free_on(&small)
    );

    // Room again.
    std::fs::remove_file(&ballast).unwrap();
    reader.synced();
    assert_eq!(legacy::version(&state).unwrap(), 2);
    assert!(
        legacy::contents(&state).unwrap() == held,
        "moved, not the same"
    );
    writer.write("d0/after", "after the move");
    converge(&[&reader, &writer]);
    assert_eq!(reader.read("d0/after"), b"after the move");
}

/// A server's database of schema 3 moved to 4 on a disk short of room: told before it starts, and
/// the server does not open it, saying why; untold, the disk fills partway through, and the database
/// is schema 3 as it was, its WAL let go. Room again, the server moves it and the devices go on.
#[test]
fn a_move_of_the_servers_database_on_a_full_disk_leaves_schema_3_as_it_was() {
    let sb = Sandbox::on_disk("full-server-move");
    let dir = sb.path("small");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = on_small_disk("inner_server_move_on_a_full_disk", &dir, 64) {
        assert!(ok, "see the output above");
    }
}

#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_server_move_on_a_full_disk() {
    let small = inner_dir();
    let sb = Sandbox::fast("full-server-move-outer");
    let mut server = Server::start(small.join("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..3000 {
        a.write(&format!("d{}/f{i}", i % 20), format!("file {i}"));
    }
    a.synced();
    b.synced();
    server.kill();
    let data = server.data.to_str().unwrap().to_string();
    run_server(&["downgrade", "--data", &data], &[]).expect_ok();
    let db = server.data.join("meta.sqlite");
    let schema = |db: &std::path::Path| -> i64 {
        rusqlite::Connection::open(db)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    assert_eq!(schema(&db), 3);
    let kept = state_everything(&db);
    let (count, free) = state_pages(&db);
    let live = (count - free) * 4096;

    // Less than the move asks for.
    let ballast = small.join("ballast");
    std::fs::write(&ballast, vec![1u8; (free_on(&small) - live) as usize]).unwrap();
    let run = run_server(&["fsck", "--data", &data], &[]);
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr.contains("It stays at schema 3") && run.stderr.contains("free"),
        "{}",
        run.text()
    );
    assert_eq!(schema(&db), 3);
    same_state(&state_everything(&db), &kept, "not tried for want of room");

    // Not told, and its pages spilling into the WAL: the disk fills partway through.
    let room = free_on(&small);
    let more = small.join("more");
    std::fs::write(&more, vec![1u8; (room - 96 * 1024) as usize]).unwrap();
    let run = run_server(
        &["fsck", "--data", &data],
        &[(
            "CAIRN_FP_TUNE",
            "server.migrate_room=0,server.migrate_cache_kib=64",
        )],
    );
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr.contains("ran out of room") && run.stderr.contains("stays at schema 3"),
        "{}",
        run.text()
    );
    assert_eq!(schema(&db), 3);
    same_state(&state_everything(&db), &kept, "the disk filled during it");
    assert_eq!(wal_len(&db), 0, "what the failed move wrote is let go");
    std::fs::remove_file(&more).unwrap();
    assert!(
        free_on(&small) + 64 * 1024 >= room,
        "{} free after, {room} before",
        free_on(&small)
    );

    // Room again.
    std::fs::remove_file(&ballast).unwrap();
    server.restart();
    assert_eq!(schema(&db), 4);
    a.write("d0/after", "after the move");
    b.write("d1/f1", "edited on b");
    converge(&[&a, &b]);
    assert_eq!(b.read("d0/after"), b"after the move");
    assert_eq!(a.read("d1/f1"), b"edited on b");
    assert!(server.fsck().contains("clean"));
}
