//! A device's first sync of a big tree, made hours shorter: the server makes an upload request's
//! chunks durable together, a big upload goes in several commits from one plan, a download asks for
//! manifests and stores rows many at a time, brings chunks into its cache a slice at a time, and is not
//! read back; and a new device reads a big journal in several answers. Each test here is what the
//! faster way must not let through.

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn steps(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn count(steps: &[String], name: &str) -> usize {
    steps.iter().filter(|s| *s == name).count()
}

/// Where the step that is the `nth` (1-based) hit of `name` falls in the whole run.
fn nth_step(steps: &[String], name: &str, nth: usize) -> usize {
    steps
        .iter()
        .enumerate()
        .filter(|(_, s)| *s == name)
        .nth(nth - 1)
        .map(|(i, _)| i + 1)
        .unwrap_or_else(|| panic!("no hit {nth} of {name} in {steps:?}"))
}

/// Run `f` for this part's steps of 1..=count, one after another, failing at the end with the first that
/// panicked.
fn for_each_step(shard: Shard, count: usize, f: impl Fn(usize)) {
    let failures = each_item(shard, count, |i| f(i + 1));
    assert!(
        failures.is_empty(),
        "{} of this part's steps of {count} failed; first: {:?}",
        failures.len(),
        failures.first().map(|(i, m)| (i + 1, m))
    );
}

/// Run a sync on a thread, paused at a failpoint; `during` acts in the gap.
fn sync_with_gap(
    sb: &Sandbox,
    d: &Device,
    point: &str,
    env: &[(&str, &str)],
    during: impl FnOnce(),
) -> Run {
    let gate: PathBuf = sb.path(&format!("gate-{:x}", rand::random::<u32>()));
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("{point}:{}", gate.display());
    let device = d.clone();
    let mut all: Vec<(String, String)> = vec![("CAIRN_FP_PAUSE".into(), pause)];
    all.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let worker = std::thread::spawn(move || {
        let env: Vec<(&str, &str)> = all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        device.sync_env(&env)
    });
    // Only against a run that never gets there: under a full suite's load each commit of a big upload
    // can take many seconds, as on macOS, whose full flushes wait on each other.
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    while !gate.join("reached").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the sync never reached {point}"
        );
        assert!(
            !worker.is_finished(),
            "the sync ended before reaching {point}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    during();
    std::fs::write(gate.join("go"), b"").unwrap();
    worker.join().unwrap()
}

/// The one namespace's chunk store on the server.
fn store_of(server: &Server) -> PathBuf {
    let ns = std::fs::read_dir(server.data.join("ns"))
        .unwrap()
        .next()
        .expect("a namespace")
        .unwrap();
    ns.path()
}

/// Same size, other bytes, and the modification time put back: the edit a fingerprint finds hardest.
fn edit_keeping_size_and_time(path: &Path) {
    let (sec, nsec) = mtime_of(path);
    let mut data = std::fs::read(path).unwrap();
    for b in data.iter_mut() {
        *b = b.wrapping_add(1);
    }
    std::fs::write(path, &data).unwrap();
    set_mtime(path, sec, nsec);
}

// --- The server: an upload request's chunks durable together ---

/// Twelve small files and one of several chunks: one request of many frames.
fn upload_tree(a: &Device) {
    for i in 0..12 {
        a.write(
            &format!("d{}/f{i}", i % 3),
            token::make(i, 300 + 40 * i as usize),
        );
    }
    a.write("big", token::make(99, 1_700_000));
}

#[test]
fn an_upload_request_is_answered_only_once_all_of_it_is_durable() {
    let sb = Sandbox::on_disk("durable-request");
    let log = sb.path("steps");
    std::fs::write(&log, "").unwrap();
    let log_s = log.to_str().unwrap().to_string();
    let server = Server::start_with(
        sb.path("server"),
        vec![("CAIRN_FP_LOG".into(), log_s.clone())],
    );
    let a = Device::new(&sb, &server, "a");
    upload_tree(&a);
    a.sync_env(&[("CAIRN_FP_LOG", &log_s)]).expect_ok();
    let steps = steps(&log);
    let at = |name: &str| -> Vec<usize> {
        steps
            .iter()
            .enumerate()
            .filter(|(_, s)| *s == name)
            .map(|(i, _)| i)
            .collect()
    };
    let (synced, renamed, durable) = (
        at("server.chunk.synced"),
        at("server.chunk.renamed"),
        at("server.chunk.durable"),
    );
    let sent = at("sync.upload.sent");
    assert_eq!(sent.len(), 1, "one request: {steps:?}");
    assert!(synced.len() >= 26, "every chunk and manifest: {steps:?}");
    assert_eq!(synced.len(), renamed.len(), "{steps:?}");
    // Each chunk whole and synced before any reaches its final path, every directory synced after
    // the last rename, and only then the answer.
    assert!(synced.last() < renamed.first(), "{steps:?}");
    assert!(
        !durable.is_empty() && renamed.last() < durable.first(),
        "{steps:?}"
    );
    assert!(durable.last() < sent.first(), "{steps:?}");
    // Each directory once.
    assert!(durable.len() <= renamed.len(), "{steps:?}");
}

fn a_server_killed_at_every_step_of_an_upload_request_keeps_only_whole_chunks(shard: Shard) {
    let probe = Sandbox::fast("killed-request-probe");
    let log = probe.path("steps");
    std::fs::write(&log, "").unwrap();
    let server = Server::start_with(
        probe.path("server"),
        vec![("CAIRN_FP_LOG".into(), log.to_str().unwrap().into())],
    );
    let a = Device::new(&probe, &server, "a");
    upload_tree(&a);
    a.synced();
    drop(server);
    let steps = steps(&log);
    assert!(count(&steps, "server.chunk.renamed") >= 26, "{steps:?}");
    for_each_step(shard, steps.len(), |n| {
        let sb = Sandbox::fast("killed-request");
        let mut server = Server::start_with(
            sb.path("server"),
            vec![("CAIRN_FP_CRASH".into(), n.to_string())],
        );
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        upload_tree(&a);
        let run = a.sync();
        let context = format!("server killed at step {n} ({})", steps[n - 1]);
        assert!(
            run.ok() || !server.alive_after_a_kill(),
            "{context}: {}",
            run.text()
        );
        // As the kill left it, before a restart tidies anything: every chunk at its final path is
        // whole, and every commit names only chunks that are there.
        let report = server.fsck();
        assert!(report.contains("clean"), "{context}: {report}");
        server.set_env(Vec::new());
        server.restart();
        let p = converge(&[&a, &b]);
        assert_eq!(token::all_in(&p).len(), 13, "{context}: {:?}", p.names());
        let incoming = std::fs::read_dir(store_of(&server).join("incoming"))
            .unwrap()
            .count();
        assert_eq!(incoming, 0, "{context}: uploads left in incoming/");
    });
}

shards!(a_server_killed_at_every_step_of_an_upload_request_keeps_only_whole_chunks: s01 s02 s03
    s04 s05 s06 s07 s08 s09 s10 s11 s12 s13 s14 s15 s16);

#[test]
fn a_full_disk_in_the_middle_of_an_upload_request_leaves_nothing_behind() {
    let probe = Sandbox::fast("full-request-probe");
    let log = probe.path("steps");
    std::fs::write(&log, "").unwrap();
    let server = Server::start_with(
        probe.path("server"),
        vec![("CAIRN_FP_LOG".into(), log.to_str().unwrap().into())],
    );
    let a = Device::new(&probe, &server, "a");
    upload_tree(&a);
    a.synced();
    drop(server);
    let steps = steps(&log);
    let middle = nth_step(&steps, "server.chunk.before_write", 9);

    let sb = Sandbox::fast("full-request");
    let mut server = Server::start_with(
        sb.path("server"),
        vec![("CAIRN_FP_ENOSPC".into(), middle.to_string())],
    );
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    upload_tree(&a);
    let run = a.sync();
    assert!(!run.ok(), "{}", run.text());
    assert!(run.stderr.contains("disk is full"), "{}", run.text());
    // Nothing half-done is left, the server still up, and nothing reached a final path unsynced.
    assert!(server.alive());
    let incoming = std::fs::read_dir(store_of(&server).join("incoming"))
        .unwrap()
        .count();
    assert_eq!(incoming, 0, "uploads left in incoming/");
    let report = server.fsck();
    assert!(report.contains("clean"), "{report}");
    server.set_env(Vec::new());
    server.restart();
    let p = converge(&[&a, &b]);
    assert_eq!(token::all_in(&p).len(), 13, "{:?}", p.names());
}

// --- The client's upload: several commits from one plan ---

fn many_files(a: &Device, n: u64) {
    for i in 0..n {
        a.write(&format!("d{}/f{i}", i % 5), token::make(1000 + i, 64));
    }
}

#[test]
fn a_big_upload_goes_in_several_commits_from_one_scan() {
    let sb = Sandbox::fast("commits-one-scan");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    many_files(&a, 250);
    let log = sb.path("steps");
    let run = a
        .sync_env(&[
            ("CAIRN_FP_LOG", log.to_str().unwrap()),
            ("CAIRN_FP_TUNE", "sync.ops_a_commit=40"),
        ])
        .expect_ok();
    let steps = steps(&log);
    // 255 operations, 40 a commit; one round sends them all and one more confirms.
    assert_eq!(count(&steps, "sync.committed"), 7, "{}", run.text());
    assert_eq!(count(&steps, "sync.scanned"), 2, "{}", run.text());
    assert!(run.stdout.contains(" 7 commits"), "{}", run.text());
    let p = converge(&[&a, &b]);
    assert_eq!(token::all_in(&p).len(), 250);
}

#[test]
fn a_change_elsewhere_between_the_commits_of_one_plan_is_kept() {
    let sb = Sandbox::on_disk("between-commits");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for d in 0..3 {
        a.write(&format!("d{d}/keep"), token::make(1900 + d, 40));
    }
    a.synced();
    b.synced();
    for i in 0..120u64 {
        a.write(&format!("d{}/n{i}", i % 3), token::make(2000 + i, 80));
    }
    let run = sync_with_gap(
        &sb,
        &a,
        "sync.dequeued",
        &[("CAIRN_FP_TUNE", "sync.ops_a_commit=30")],
        || {
            // The same names a later commit of a's plan makes, and an edit a's plan knows nothing of.
            b.write("d2/n119", token::make(3119, 80));
            b.write("d1/n100", token::make(3100, 80));
            b.write("d0/keep", token::make(3000, 40));
            b.synced();
        },
    );
    assert!(run.ok(), "{}", run.text());
    let p = converge(&[&a, &b]);
    let found = token::all_in(&p);
    for t in (2000..2120).chain([3119, 3100, 3000, 1901, 1902]) {
        assert!(found.contains(&t), "version {t} lost: {:?}", p.names());
    }
    assert_eq!(a.read("d0/keep"), token::make(3000, 40));
}

/// A commit refused partway through a plan stops the rest of it: the commits after it were planned on a
/// tree the refused one would have left, and what they would record as shared with the server is not.
#[test]
fn a_refused_commit_stops_the_rest_of_its_plan() {
    let sb = Sandbox::on_disk("refused-stops");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    a.write("d0/x", token::make(1, 60));
    a.write("d1/keep", token::make(2, 60));
    a.synced();
    b.synced();
    a.write("d0/x", token::make(3, 60));
    std::fs::rename(a.path("d0/x"), a.path("d0/x-moved")).unwrap();
    for i in 0..60u64 {
        a.write(&format!("d1/n{i}"), token::make(100 + i, 60));
    }
    let run = sync_with_gap(
        &sb,
        &a,
        "sync.uploaded",
        &[("CAIRN_FP_TUNE", "sync.ops_a_commit=20")],
        || {
            // With the folder's time put back, so that only the name is in a's way, and the commits
            // after the refused one would go through.
            let (sec, nsec) = mtime_of(&b.path("d0"));
            b.write("d0/x-moved", token::make(4, 60));
            set_mtime(&b.path("d0"), sec, nsec);
            b.synced();
        },
    );
    assert!(run.ok(), "{}", run.text());
    let p = converge(&[&a, &b]);
    let found = token::all_in(&p);
    for t in [2, 3, 4].into_iter().chain(100..160) {
        assert!(found.contains(&t), "version {t} lost: {:?}", p.names());
    }
    // The move here stands, beside what took its name.
    assert!(!a.exists("d0/x"), "a move undone: {:?}", p.names());
}

/// Edited once its commit has gone up, a file is edited from what went up, not a second version of it.
#[test]
fn an_edit_after_a_big_upload_is_an_edit_of_what_went_up() {
    let sb = Sandbox::on_disk("edit-after-commits");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    many_files(&a, 250);
    let run = sync_with_gap(
        &sb,
        &a,
        "sync.pushed",
        &[("CAIRN_FP_TUNE", "sync.ops_a_commit=40")],
        || {
            for i in (0..250u64).step_by(7) {
                a.write(&format!("d{}/f{i}", i % 5), token::make(5000 + i, 64));
            }
        },
    );
    assert!(run.ok(), "{}", run.text());
    let p = converge(&[&a, &b]);
    let names = p.names();
    assert!(!names.iter().any(|n| n.contains("(conflict ")), "{names:?}");
    let found = token::all_in(&p);
    for i in 0..250u64 {
        let want = if i % 7 == 0 { 5000 + i } else { 1000 + i };
        assert!(found.contains(&want), "version {want} lost: {names:?}");
    }
    assert_eq!(found.len(), 250, "{names:?}");
}

// --- The download: manifests and rows many at a time ---

#[test]
fn manifests_come_down_many_to_a_request() {
    let sb = Sandbox::fast("manifest-batches");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    many_files(&a, 30);
    a.synced();
    let log = sb.path("steps");
    b.sync_env(&[
        ("CAIRN_FP_LOG", log.to_str().unwrap()),
        ("CAIRN_FP_TUNE", "apply.manifests_a_request=7"),
    ])
    .expect_ok();
    // 30 manifests, 7 to a request and a transaction.
    assert_eq!(count(&steps(&log), "apply.manifests.stored"), 5);
    let c = Device::new(&sb, &server, "c");
    let log = sb.path("steps-c");
    c.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
        .expect_ok();
    assert_eq!(count(&steps(&log), "apply.manifests.stored"), 1);
    converge(&[&a, &b, &c]);
}

/// Killed before the rows of what arrived are stored, a device must still tell what it received
/// from an edit of its own: a later change from elsewhere stands, and nothing comes back as a copy.
fn a_download_killed_before_its_rows_are_stored_is_not_this_devices_change(shard: Shard) {
    let probe = Sandbox::fast("rows-probe");
    let server = Server::start(probe.path("server"));
    let a = Device::new(&probe, &server, "a");
    let fill = |a: &Device| {
        for i in 0..14 {
            a.write(&format!("fill/{i}"), format!("{i}"));
        }
        for i in 0..10u64 {
            a.write(&format!("x/f{i}"), token::make(i, 50));
        }
    };
    fill(&a);
    a.synced();
    // A few rows to a transaction, and all of them in the last, with the marks they answer.
    for (n, tune) in [
        ("3", "apply.rows_a_transaction=3"),
        ("all", "apply.rows_a_transaction=100000"),
    ] {
        let log = probe.path(&format!("steps-{n}"));
        let c = Device::new(&probe, &server, &format!("c-{n}"));
        c.sync_env(&[
            ("CAIRN_FP_LOG", log.to_str().unwrap()),
            ("CAIRN_FP_TUNE", tune),
        ])
        .expect_ok();
        let steps = steps(&log);
        let stores = count(&steps, "apply.rows.store");
        assert!(n == "all" || stores >= 9, "rows a few at a time: {steps:?}");
        for_each_step(shard, stores, |k| {
            let sb = Sandbox::fast("rows-killed");
            let server = Server::start(sb.path("server"));
            let a = Device::new(&sb, &server, "a");
            let b = Device::new(&sb, &server, "b");
            fill(&a);
            a.synced();
            let at = nth_step(&steps, "apply.rows.store", k).to_string();
            let run = b.sync_env(&[("CAIRN_FP_CRASH", &at), ("CAIRN_FP_TUNE", tune)]);
            let context = format!("{tune}: killed at the store of rows {k}");
            assert!(run.was_killed(), "{context}: {}", run.text());
            // Elsewhere, meanwhile: an edit, a delete, a rename.
            a.write("x/f3", token::make(103, 50));
            std::fs::remove_file(a.path("x/f5")).unwrap();
            std::fs::rename(a.path("x/f7"), a.path("x/f7-renamed")).unwrap();
            a.synced();
            let p = converge(&[&a, &b]);
            let names = p.names();
            assert!(
                !names.iter().any(|n| n.contains("(conflict ")),
                "{context}: {names:?}"
            );
            assert!(
                !names
                    .iter()
                    .any(|n| n.ends_with("x/f5") || n.ends_with("x/f7")),
                "{context}: {names:?}"
            );
            assert_eq!(a.read("x/f3"), token::make(103, 50), "{context}");
        });
    }
}

shards!(a_download_killed_before_its_rows_are_stored_is_not_this_devices_change: s01 s02 s03
    s04);

// --- The download: in slices of the cache ---

fn a_download_in_slices_killed_at_every_step_loses_nothing(shard: Shard) {
    let make = |a: &Device| {
        for i in 0..8u64 {
            a.write(&format!("s{}/f{i}", i % 2), token::make(500 + i, 300_000));
        }
        a.write("two-chunks", token::make(600, 1_500_000));
    };
    let tune = ("CAIRN_FP_TUNE", "apply.slice_bytes=400000");
    let probe = Sandbox::fast("slices-probe");
    let server = Server::start(probe.path("server"));
    let a = Device::new(&probe, &server, "a");
    let b = Device::new(&probe, &server, "b");
    make(&a);
    a.synced();
    let log = probe.path("steps");
    b.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap()), tune])
        .expect_ok();
    let steps = steps(&log);
    assert!(
        count(&steps, "apply.slice.fetched") >= 5,
        "in slices: {steps:?}"
    );
    for_each_step(shard, steps.len(), |n| {
        let sb = Sandbox::fast("slices-killed");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        make(&a);
        a.synced();
        let n_s = n.to_string();
        let run = b.sync_env(&[("CAIRN_FP_CRASH", &n_s), tune]);
        let context = format!("killed at step {n} ({})", steps[n - 1]);
        assert!(run.was_killed() || run.ok(), "{context}: {}", run.text());
        let _ = token::all_in(&Picture::of(&b.root));
        b.sync_env(&[tune]).expect_ok();
        let p = converge(&[&a, &b]);
        assert_eq!(token::all_in(&p).len(), 9, "{context}: {:?}", p.names());
        assert!(leftover_temps(&b.root).is_empty(), "{context}");
        let cached = std::fs::read_dir(b.path(".cairn/chunks")).unwrap().count();
        assert_eq!(cached, 0, "{context}: chunks left in the cache");
    });
}

shards!(a_download_in_slices_killed_at_every_step_loses_nothing: s01 s02 s03 s04 s05 s06 s07 s08
    s09 s10 s11 s12 s13 s14 s15 s16);

#[test]
fn a_download_bigger_than_its_cache_needs_about_its_own_size_free() {
    let sb = Sandbox::on_disk("room");
    let dir = sb.path("small");
    std::fs::create_dir(&dir).unwrap();
    if let Some(ok) = in_namespace("inner_room", Some((&dir, "size=96m"))) {
        assert!(ok, "see the output above");
    }
}

#[test]
#[ignore = "runs inside the namespace, through the test above"]
fn inner_room() {
    let small = PathBuf::from(std::env::var_os("CAIRN_INNER").expect("run through the wrapper"));
    // 40 MiB free on the small disk.
    let ballast = small.join("ballast");
    std::fs::write(&ballast, vec![1u8; 56 * 1024 * 1024]).unwrap();
    let sb = Sandbox::fast("room-outer");
    let server = Server::start(sb.path("server"));
    let writer = Device::new(&sb, &server, "writer");
    let root = small.join("reader");
    std::fs::create_dir(&root).unwrap();
    let token_file = server.token_file.to_str().unwrap().to_string();
    run(
        &[
            "init",
            "--server",
            &server.url,
            "--device-name",
            "reader",
            "--token-file",
            &token_file,
            root.to_str().unwrap(),
        ],
        &[],
    )
    .expect_ok();
    let reader = Device {
        root,
        name: "reader".into(),
    };
    let tune = ("CAIRN_FP_TUNE", "apply.slice_bytes=4194304");

    // 24 MiB of files that share no chunk: with their chunks all cached at once, 48 MiB.
    for i in 0..12u64 {
        writer.write(&format!("a/f{i}"), token::make(700 + i, 2 * 1024 * 1024));
    }
    writer.synced();
    reader.sync_env(&[tune]).expect_ok();
    assert_eq!(token::all_in(&Picture::of(&reader.root)).len(), 12);

    // More than is left: refused before anything moves, and said so.
    for i in 0..10u64 {
        writer.write(&format!("b/f{i}"), token::make(800 + i, 2 * 1024 * 1024));
    }
    writer.write("a/f0", token::make(900, 2 * 1024 * 1024));
    writer.synced();
    let run = reader.sync_env(&[tune]);
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains("would be full"), "{}", run.text());
    assert!(!reader.exists("b"), "nothing came down");
    assert_eq!(reader.read("a/f0"), token::make(700, 2 * 1024 * 1024));
    assert!(leftover_temps(&reader.root).is_empty());
    let cached = std::fs::read_dir(reader.path(".cairn/chunks"))
        .unwrap()
        .count();
    assert_eq!(cached, 0, "nothing was fetched");

    // Room appears: it all comes down.
    std::fs::remove_file(&ballast).unwrap();
    reader.sync_env(&[tune]).expect_ok();
    let p = converge(&[&reader, &writer]);
    assert_eq!(token::all_in(&p).len(), 22);
}

// --- The download: not read back ---

/// Linux only: elsewhere no watch is had, and every placed file is read again, as before.
#[cfg(target_os = "linux")]
#[test]
fn a_download_is_not_read_again() {
    let sb = Sandbox::on_disk("not-read-again");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    many_files(&a, 20);
    a.synced();
    let log = sb.path("steps");
    // Long enough between the last file and its record that a later write would show.
    let run = sync_with_gap(
        &sb,
        &b,
        "apply.created",
        &[("CAIRN_FP_LOG", log.to_str().unwrap())],
        || std::thread::sleep(Duration::from_millis(300)),
    );
    assert!(run.ok(), "{}", run.text());
    let first = steps(&log);
    assert_eq!(count(&first, "scan.file.reading"), 0, "{first:?}");
    // And the next sync reads nothing either.
    let log = sb.path("steps-2");
    b.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
        .expect_ok();
    assert_eq!(count(&steps(&log), "scan.file.reading"), 0);
    converge(&[&a, &b]);
}

/// A program writing a file that has just arrived, while the sync that placed it is busy, as one blocked in
/// an fsync, a long apply, or paused here: it waits for nothing, and its edit, size and time kept, goes up.
#[test]
fn an_edit_as_files_arrive_while_the_sync_is_busy_neither_waits_nor_is_lost() {
    for point in ["apply.create.renamed", "apply.created"] {
        let sb = Sandbox::on_disk("edit-while-busy");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        for i in 0..6u64 {
            a.write(&format!("f{i}"), token::make(i, 5000));
        }
        a.synced();
        let mut edited = Vec::new();
        let run = sync_with_gap(
            &sb,
            &b,
            point,
            // Watched for a second: the edit falls inside, and the sync goes on after it.
            &[("CAIRN_FP_TUNE", "apply.watch_window_ns=1000000000")],
            || {
                for i in 0..6u64 {
                    let path = b.path(&format!("f{i}"));
                    if !path.exists() {
                        continue;
                    }
                    let started = std::time::Instant::now();
                    edit_keeping_size_and_time(&path);
                    let took = started.elapsed();
                    assert!(
                        took < Duration::from_millis(500),
                        "{point}: the edit waited {took:?}"
                    );
                    edited.push((format!("f{i}"), std::fs::read(&path).unwrap()));
                }
                std::thread::sleep(Duration::from_millis(1500));
            },
        );
        assert!(run.ok(), "{point}: {}", run.text());
        assert!(!edited.is_empty(), "{point}");
        let p = converge(&[&a, &b]);
        for (name, data) in &edited {
            assert_eq!(
                &a.read(name),
                data,
                "{point}: the edit of {name} on b went up"
            );
        }
        assert!(
            p.names().iter().all(|n| !n.contains("(conflict ")),
            "{point}"
        );
    }
}

/// After the watch and before the record: the timestamps moved, so the record does not vouch.
#[test]
fn an_edit_between_the_watch_and_the_record_is_sent() {
    let sb = Sandbox::on_disk("edit-before-record");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    a.write("f", token::make(1, 5000));
    a.synced();
    let run = sync_with_gap(
        &sb,
        &b,
        "apply.dirs.synced",
        &[("CAIRN_FP_TUNE", "apply.watch_window_ns=0")],
        || {
            // Past the tick of the last change, as the window the watch waits out would be.
            std::thread::sleep(Duration::from_millis(50));
            edit_keeping_size_and_time(&b.path("f"))
        },
    );
    assert!(run.ok(), "{}", run.text());
    let edited = b.read("f");
    assert_ne!(edited, token::make(1, 5000));
    converge(&[&a, &b]);
    assert_eq!(a.read("f"), edited, "the edit on b went up");
}

// --- Metadata set in place: not read back either ---

/// What a device's change of metadata alone sets here: the mode, on Unix, and the mtime.
fn set_on(a: &Device, mode: bool, name: &str) {
    if mode {
        chmod(&a.path(name), 0o640);
    } else {
        set_mtime(&a.path(name), 1_700_000_000, 123_400);
    }
}

fn metadata_kinds() -> Vec<bool> {
    if cfg!(unix) {
        vec![true, false]
    } else {
        vec![false]
    }
}

/// Linux only: elsewhere no watch is had, and a file whose metadata a sync set is read again, as before.
#[cfg(target_os = "linux")]
#[test]
fn metadata_set_in_place_is_not_read_again() {
    for mode in metadata_kinds() {
        let sb = Sandbox::on_disk("meta-not-read-again");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        many_files(&a, 20);
        a.synced();
        b.synced();
        // Long enough after b wrote them that its records vouch.
        std::thread::sleep(Duration::from_millis(300));
        for i in 0..20 {
            set_on(&a, mode, &format!("d{}/f{i}", i % 5));
        }
        a.synced();
        let log = sb.path("steps");
        b.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
            .expect_ok();
        let first = steps(&log);
        assert_eq!(count(&first, "apply.meta.done"), 20, "mode {mode}");
        assert_eq!(count(&first, "scan.file.reading"), 0, "mode {mode}");
        let log = sb.path("steps-2");
        b.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
            .expect_ok();
        assert_eq!(count(&steps(&log), "scan.file.reading"), 0, "mode {mode}");
        converge(&[&a, &b]);
    }
}

/// A file whose metadata alone comes from the server, edited in the gaps of the sync that sets it:
/// between its scan and the apply, after the apply checked it against the scan and before its metadata
/// is set, just after, and once its watch has settled. An edit that keeps the size and the mtime is
/// told from the content the scan vouched for only by a watch from before the check, or a read, as the
/// sync's own change then moves the ctime as the edit did: it is not taken for that content, and goes
/// up. A plain edit moves the mtime, which, unless the sync set it, must not be taken for the pair of
/// the server's: it goes up too.
fn an_edit_while_a_files_metadata_is_set_is_sent(shard: Shard) {
    let cases: Vec<(bool, bool, &str)> = metadata_kinds()
        .into_iter()
        .flat_map(|mode| {
            [true, false].into_iter().flat_map(move |keep_time| {
                [
                    "sync.identified",
                    "apply.meta",
                    "apply.meta.done",
                    "apply.dirs.synced",
                ]
                .into_iter()
                .map(move |point| (mode, keep_time, point))
            })
        })
        .collect();
    for &(mode, keep_time, point) in shard.share(&cases) {
        let context = format!("mode {mode}, keeping the mtime {keep_time}, edited at {point}");
        let sb = Sandbox::on_disk("edit-while-meta");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        a.write("f", token::make(1, 5000));
        a.write("other", "o");
        a.synced();
        b.synced();
        std::thread::sleep(Duration::from_millis(300));
        set_on(&a, mode, "f");
        a.synced();
        let run = sync_with_gap(
            &sb,
            &b,
            point,
            // The watch settles at once, so that an edit at the last point comes after it.
            &[("CAIRN_FP_TUNE", "apply.watch_window_ns=0")],
            || {
                // Past the tick of the last change, as the window the watch waits out would be.
                std::thread::sleep(Duration::from_millis(50));
                if keep_time {
                    edit_keeping_size_and_time(&b.path("f"));
                } else {
                    std::fs::write(b.path("f"), token::make(2, 5000)).unwrap();
                }
            },
        );
        let edited = b.read("f");
        assert_ne!(edited, token::make(1, 5000), "{context}");
        let mtime = mtime_of(&b.path("f"));
        // Whatever the gap made of it, the trees settle on the edit, and on each side's metadata.
        converge(&[&a, &b]);
        assert_eq!(a.read("f"), edited, "{context}: {}", run.text());
        if mode {
            assert_eq!(mode_of(&a.path("f")) & 0o777, 0o640, "{context}");
        }
        if !keep_time {
            assert_eq!(
                mtime_of(&a.path("f")),
                mtime,
                "{context}: the edit's time went up"
            );
        }
    }
}

shards!(an_edit_while_a_files_metadata_is_set_is_sent: s01 s02 s03 s04 s05 s06 s07 s08);

/// A shared writable mapping of a file, in this process, from before the sync.
#[cfg(target_os = "linux")]
struct Mapping {
    at: *mut u8,
    len: usize,
    _file: std::fs::File,
}

#[cfg(target_os = "linux")]
impl Mapping {
    fn new(path: &Path, len: usize) -> Self {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn mmap(
                addr: *mut std::ffi::c_void,
                len: usize,
                prot: std::ffi::c_int,
                flags: std::ffi::c_int,
                fd: std::ffi::c_int,
                offset: i64,
            ) -> *mut std::ffi::c_void;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        // PROT_READ | PROT_WRITE, MAP_SHARED.
        // SAFETY: a new mapping of an open file, of no more than its length.
        let at = unsafe { mmap(std::ptr::null_mut(), len, 3, 1, file.as_raw_fd(), 0) };
        assert_ne!(at as isize, -1, "mmap: {}", std::io::Error::last_os_error());
        Self {
            at: at.cast(),
            len,
            _file: file,
        }
    }

    /// A write through the mapping: no system call, so nothing a watch sees.
    fn write(&self, data: &[u8]) {
        assert!(data.len() <= self.len);
        // SAFETY: within the mapping, which lives as long as `self`.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.at, data.len()) };
    }
}

#[cfg(target_os = "linux")]
impl Drop for Mapping {
    fn drop(&mut self) {
        extern "C" {
            fn munmap(addr: *mut std::ffi::c_void, len: usize) -> std::ffi::c_int;
        }
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe { munmap(self.at.cast(), self.len) };
    }
}

/// A program that has had the file mapped to write since before the sync writes through the mapping
/// while the sync sets the file's mode and mtime: no system call, so no watch sees the write, and the
/// sync's own change then puts the mtime and ctime past it. The file is read again all the same, as Linux
/// grants no read lease on a file someone has open to write, and the edit goes up once the program lets go
/// of it.
#[cfg(target_os = "linux")]
#[test]
fn a_write_through_a_mapping_held_from_before_the_sync_is_sent() {
    let sb = Sandbox::on_disk("mapped-while-meta");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    a.write("f", token::make(1, 8192));
    a.write("other", "o");
    a.synced();
    b.synced();
    let map = Mapping::new(&b.path("f"), 8192);
    chmod(&a.path("f"), 0o640);
    set_mtime(&a.path("f"), 1_700_000_000, 123_400);
    a.synced();
    // Older than the window in which a file open to write reads as being written, so that the scan
    // takes it as it is.
    std::thread::sleep(Duration::from_millis(2_300));
    let edit = b"edited through a mapping";
    let run = sync_with_gap(&sb, &b, "apply.meta", &[], || map.write(edit));
    // While it is held, the sync may leave the file for later; let go of, it goes up.
    drop(map);
    converge(&[&a, &b]);
    assert!(
        a.read("f").starts_with(edit),
        "the edit through the mapping went up: {}",
        run.text()
    );
    assert_eq!(mode_of(&a.path("f")) & 0o777, 0o640);
}

// --- A new device reads a big journal in several answers ---

#[test]
fn a_journal_bigger_than_one_answer_comes_in_several() {
    let sb = Sandbox::fast("journal-answers");
    let log = sb.path("steps");
    std::fs::write(&log, "").unwrap();
    let server = Server::start_with(
        sb.path("server"),
        vec![
            ("CAIRN_FP_LOG".into(), log.to_str().unwrap().into()),
            ("CAIRN_FP_TUNE".into(), "server.changes_bytes=1".into()),
        ],
    );
    let a = Device::new(&sb, &server, "a");
    for i in 0..6u64 {
        a.write(&format!("f{i}"), token::make(i, 100));
        a.synced();
    }
    std::fs::write(&log, "").unwrap();
    let b = Device::new(&sb, &server, "b");
    b.synced();
    // Six commits, one an answer each: the last of them is at the head, so nothing more is asked, and
    // the round that confirms what arrived reads no new commits.
    assert!(
        count(&steps(&log), "server.changes.answered") >= 6,
        "{:?}",
        steps(&log)
    );
    let p = converge(&[&a, &b]);
    assert_eq!(token::all_in(&p).len(), 6);
}
