//! A real full disk, not an injected one: a size-limited tmpfs in a private user and mount namespace.
//! Skipped where unprivileged namespaces are forbidden; the injected matrix still runs there. Not
//! built on Windows, which has neither.
#![cfg(unix)]

mod common;
use common::*;
use std::process::Command;

/// Re-run this test binary inside a namespace with `dir` mounted as a small tmpfs.
fn in_namespace(test: &str, dir: &std::path::Path, megabytes: u32) -> Option<bool> {
    in_namespace_with(test, dir, &format!("size={megabytes}m"))
}

/// The same, with the tmpfs's own options.
fn in_namespace_with(test: &str, dir: &std::path::Path, options: &str) -> Option<bool> {
    let exe = std::env::current_exe().unwrap();
    let inner = format!(
        "mount -t tmpfs -o {options} tmpfs '{}' && CAIRN_INNER='{}' exec '{}' --exact {test} --ignored --nocapture --test-threads=1",
        dir.display(),
        dir.display(),
        exe.display(),
    );
    let probe = Command::new("unshare").args(["-Urm", "true"]).output();
    if !probe.is_ok_and(|o| o.status.success()) {
        eprintln!("no unprivileged namespaces here: skipped");
        return None;
    }
    let out = Command::new("unshare")
        .args(["-Urm", "sh", "-c", &inner])
        .output()
        .unwrap();
    if !out.status.success() {
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Some(out.status.success())
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
    if let Some(ok) = in_namespace("inner_client_disk", &dir, SMALL_DISK_MB) {
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
    if let Some(ok) = in_namespace("inner_server_disk", &dir, SMALL_DISK_MB) {
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
    if let Some(ok) = in_namespace_with("inner_no_room_to_ask", &dir, "size=64m,nr_inodes=400") {
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
    if let Some(ok) = in_namespace_with(
        "inner_xattr_on_a_full_disk",
        &dir,
        "size=24m,nr_inodes=3000",
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
        eprintln!("no user xattrs on this tmpfs: skipped");
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
    if let Some(ok) = in_namespace_with("inner_mount_in_the_tree", &dir, "size=1m") {
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
