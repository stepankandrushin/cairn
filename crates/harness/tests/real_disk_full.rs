//! A real full disk, not an injected one: a size-limited tmpfs in a private user and mount namespace.
//! Skipped where unprivileged namespaces are forbidden; the injected matrix still runs there. Not
//! built on Windows, which has neither.
#![cfg(unix)]

mod common;
use common::*;
use std::process::Command;

/// Re-run this test binary inside a namespace with `dir` mounted as a small tmpfs.
fn in_namespace(test: &str, dir: &std::path::Path, megabytes: u32) -> Option<bool> {
    let exe = std::env::current_exe().unwrap();
    let inner = format!(
        "mount -t tmpfs -o size={megabytes}m tmpfs '{}' && CAIRN_INNER='{}' exec '{}' --exact {test} --ignored --nocapture --test-threads=1",
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
