//! The failure matrix: one rich scenario, killed or starved at every step it has, on the client and
//! on the server, each time checked against the invariants.
//!
//! The steps are not listed by hand: a clean run logs every failpoint it passes, and the matrix then
//! replays the scenario once per step. A new step in the code is covered without touching this file.

mod common;
use common::*;
use std::collections::BTreeSet;
#[cfg(unix)]
use std::os::unix::fs::symlink;

struct World {
    sb: Sandbox,
    server: Server,
    a: Device,
    b: Device,
    /// Versions no one knowingly replaced: all must exist once the dust settles.
    live: BTreeSet<u64>,
}

/// Device `a` holds unsynced edits of every kind, and the server holds changes it has not seen, so
/// one `a` sync exercises every path: conflict copies, moves through the stage, a read-only
/// directory, deletes, new content, hard links, metadata, uploads. On Windows, what it holds of those:
/// no symlinks or xattrs, a file's write bits alone, and no folder that refuses a write.
fn world(server_env: Vec<(String, String)>) -> World {
    let sb = Sandbox::fast("matrix");
    let server = Server::start_with(sb.path("server"), server_env);
    let a = Device::with_flags(&sb, &server, "a", &[]);
    let b = Device::with_flags(&sb, &server, "b", &[]);
    let mut live = BTreeSet::new();
    let mut id = 0;
    let mut tok = |size: usize| {
        id += 1;
        (id, token::make(id, size))
    };

    let mut base = |rel: &str, size: usize, live: &mut BTreeSet<u64>| {
        let (i, data) = tok(size);
        a.write(rel, data);
        live.insert(i);
        i
    };
    let conflict_base = base("conflict.txt", 100, &mut live);
    let swap1 = base("swap/one", 100, &mut live);
    let swap2 = base("swap/two", 100, &mut live);
    base("swap-remote/one", 100, &mut live);
    base("swap-remote/two", 100, &mut live);
    let moved_remote = base("moved-remote", 100, &mut live);
    let moved_local = base("moved-local", 100, &mut live);
    let del_remote = base("delete-remote", 100, &mut live);
    let del_local = base("delete-local", 100, &mut live);
    let edit_delete = base("edit-vs-delete", 100, &mut live);
    let remote_edit = base("edit-remote", 2_000_000, &mut live);
    let local_edit = base("edit-local", 2_000_000, &mut live);
    let meta_remote = base("meta-remote", 50, &mut live);
    base("big-unchanged", 3_000_000, &mut live);
    base("read-only/inside", 50, &mut live);
    let hard = base("hard/first", 50, &mut live);
    base("deep/er/est/f", 50, &mut live);
    let tree_to_delete = base("tree-to-delete/x", 50, &mut live);
    base("keep1", 10, &mut live);
    base("keep2", 10, &mut live);
    std::fs::hard_link(a.path("hard/first"), a.path("hard/second")).unwrap();
    #[cfg(unix)]
    symlink("keep1", a.path("a-link")).unwrap();
    a.synced();
    chmod(&a.path("read-only"), 0o555);
    a.synced();
    b.synced();

    // The server moves on, through b.
    let (i, d) = tok(120);
    b.write("conflict.txt", d);
    live.insert(i);
    live.remove(&conflict_base);
    let (i, d) = tok(2_100_000);
    b.write("edit-remote", d);
    live.insert(i);
    live.remove(&remote_edit);
    std::fs::rename(b.path("moved-remote"), b.path("moved-remote-renamed")).unwrap();
    let _ = moved_remote;
    std::fs::remove_file(b.path("delete-remote")).unwrap();
    live.remove(&del_remote);
    std::fs::remove_file(b.path("edit-vs-delete")).unwrap();
    live.remove(&edit_delete);
    // A swap arriving here has to go through the stage.
    std::fs::rename(b.path("swap-remote/one"), b.path("swap-remote/tmp")).unwrap();
    std::fs::rename(b.path("swap-remote/two"), b.path("swap-remote/one")).unwrap();
    std::fs::rename(b.path("swap-remote/tmp"), b.path("swap-remote/two")).unwrap();
    chmod(&b.path("meta-remote"), other_mode(0o600));
    let (i, d) = tok(160);
    b.write("hard/first", d);
    live.insert(i);
    live.remove(&hard);
    let _ = meta_remote;
    let (i, d) = tok(150);
    b.write("new-remote/sub/file", d);
    live.insert(i);
    let (i, d) = tok(150);
    b.write("hard/third-base", d);
    live.insert(i);
    std::fs::hard_link(b.path("hard/third-base"), b.path("hard/third-link")).unwrap();
    chmod(&b.path("read-only"), 0o755);
    let (i, d) = tok(150);
    b.write("read-only/from-remote", d);
    live.insert(i);
    chmod(&b.path("read-only"), 0o555);
    #[cfg(unix)]
    symlink("keep2", b.path("b-link")).unwrap();
    b.synced();

    // And a has its own work, not yet synced.
    let (i, d) = tok(130);
    a.write("conflict.txt", d);
    live.insert(i);
    let (i, d) = tok(2_200_000);
    a.write("edit-local", d);
    live.insert(i);
    live.remove(&local_edit);
    std::fs::rename(a.path("moved-local"), a.path("moved-local-renamed")).unwrap();
    let _ = moved_local;
    std::fs::remove_file(a.path("delete-local")).unwrap();
    live.remove(&del_local);
    let (i, d) = tok(140);
    a.write("edit-vs-delete", d);
    live.insert(i);
    std::fs::rename(a.path("swap/one"), a.path("swap/tmp")).unwrap();
    std::fs::rename(a.path("swap/two"), a.path("swap/one")).unwrap();
    std::fs::rename(a.path("swap/tmp"), a.path("swap/two")).unwrap();
    let _ = (swap1, swap2);
    let (i, d) = tok(3_000_000);
    a.write("new-local/big", d);
    live.insert(i);
    let (i, d) = tok(40);
    a.write("new-local/small", d);
    live.insert(i);
    chmod(&a.path("keep1"), other_mode(0o640));
    #[cfg(unix)]
    {
        set_xattr(&a.path("keep2"), "user.tag", b"v");
        symlink("/somewhere", a.path("new-link")).unwrap();
    }
    std::fs::remove_dir_all(a.path("tree-to-delete")).unwrap();
    live.remove(&tree_to_delete);
    World {
        sb,
        server,
        a,
        b,
        live,
    }
}

/// At any moment: every file at a synced path is whole.
#[track_caller]
fn check_no_torn_files(d: &Device) {
    // Read-only directories may have been relaxed; the picture reads through either way.
    let p = Picture::of(&d.root);
    let _ = token::all_in(&p);
}

#[track_caller]
fn check_settled(w: &mut World, context: &str) {
    let p = converge(&[&w.a, &w.b]);
    let found = token::all_in(&p);
    let lost: Vec<_> = w.live.difference(&found).collect();
    assert!(
        lost.is_empty(),
        "{context}: versions lost: {lost:?}\n{:?}",
        p.names()
    );
    assert!(
        leftover_temps(&w.a.root).is_empty(),
        "{context}: temporary files left behind: {:?}",
        leftover_temps(&w.a.root)
    );
    let stage = std::fs::read_dir(w.a.path(".cairn/stage")).unwrap().count();
    assert_eq!(stage, 0, "{context}: entries left in the stage");
    // The read-only directory is read-only again, whatever was relaxed.
    assert_eq!(
        mode_of(&w.a.path("read-only")) & 0o777,
        dir_mode(0o555),
        "{context}: a relaxed directory kept its relaxed mode"
    );
    let report = w.server.fsck();
    assert!(report.contains("clean"), "{context}: {report}");
    // Recovery put everything back where it was, rather than only keeping it somewhere.
    let names = p.names();
    assert!(
        !names.iter().any(|n| n.contains("recovered")),
        "{context}: {names:?}"
    );
    // New content reached every name of the hard-linked file, and they are still one file.
    for d in [&w.a, &w.b] {
        assert_eq!(
            inode(&d.path("hard/first")),
            inode(&d.path("hard/second")),
            "{context}: a hard link was split on {}",
            d.name
        );
    }
    // A conflict was recorded exactly because both kept their content.
    assert!(
        p.names().iter().any(|n| n.contains("(conflict ")),
        "{context}: no conflict copy: {:?}",
        p.names()
    );
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

fn logged_steps(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

/// The steps one clean client run passes.
fn client_steps() -> Vec<String> {
    let w = world(Vec::new());
    let log = w.sb.path("steps");
    let log_s = log.to_str().unwrap();
    w.a.sync_env(&[("CAIRN_FP_LOG", log_s)]).expect_ok();
    logged_steps(&log)
}

const REQUIRED_CLIENT_STEPS: &[&str] = &[
    "apply.copy.intent",
    "apply.copy.renamed",
    "apply.create.renamed",
    "apply.dir.meta.done",
    "apply.link.done",
    "apply.meta.done",
    "apply.mkdir.done",
    "apply.move.done",
    // Windows has no folder that refuses a write, to relax.
    #[cfg(unix)]
    "apply.relax.done",
    "apply.replace.before_rename",
    "apply.replace.renamed",
    "apply.temp.written",
    "apply.temp.sync",
    "apply.stage.intent",
    "apply.relink.intent",
    "apply.replace.relink",
    "apply.unlink.done",
    "scan.file.reading",
    "sync.enqueued",
    "sync.committed",
    "sync.dequeued",
    "sync.upload.sent",
    "sync.finalize.base",
];

fn killed_at_every_client_step_nothing_is_lost(shard: Shard) {
    let steps = client_steps();
    eprintln!("client steps: {}", steps.len());
    let seen: BTreeSet<&str> = steps.iter().map(String::as_str).collect();
    for required in REQUIRED_CLIENT_STEPS {
        assert!(
            seen.contains(required),
            "the scenario no longer reaches {required}: {seen:?}"
        );
    }
    for_each_step(shard, steps.len(), |n| {
        let mut w = world(Vec::new());
        let n_s = n.to_string();
        let run = w.a.sync_env(&[("CAIRN_FP_CRASH", &n_s)]);
        let context = format!("killed at step {n} ({})", steps[n - 1]);
        assert!(run.was_killed() || run.ok(), "{context}: {}", run.text());
        check_no_torn_files(&w.a);
        // Recovery itself is killed at its own first steps too.
        let again =
            w.a.sync_env(&[("CAIRN_FP_CRASH", &(1 + n % 7).to_string())]);
        assert!(
            again.was_killed() || again.ok(),
            "{context}, then in recovery: {}",
            again.text()
        );
        check_no_torn_files(&w.a);
        check_settled(&mut w, &context);
    });
}

shards!(killed_at_every_client_step_nothing_is_lost: s01 s02 s03 s04 s05 s06 s07 s08 s09 s10 s11
    s12 s13 s14 s15 s16 s17 s18 s19 s20 s21 s22 s23 s24 s25 s26 s27 s28 s29 s30 s31 s32);

fn killed_at_every_server_step_acknowledged_commits_stay(shard: Shard) {
    // The server's steps during a's sync, counted from its own log.
    let probe = Sandbox::fast("matrix-probe");
    let log = probe.path("steps");
    let w = world(vec![("CAIRN_FP_LOG".into(), log.to_str().unwrap().into())]);
    std::fs::write(&log, "").unwrap();
    w.a.synced();
    let steps = logged_steps(&log);
    drop(w);
    eprintln!("server steps: {}", steps.len());
    for required in [
        "server.chunk.synced",
        "server.chunk.renamed",
        "server.chunk.durable",
        "server.commit.before_commit",
        "server.commit.durable",
        "server.commit.before_ack",
    ] {
        assert!(
            steps.iter().any(|s| s == required),
            "the scenario no longer reaches {required}: {steps:?}"
        );
    }

    for_each_step(shard, steps.len(), |n| {
        let mut w = world(Vec::new());
        let name = steps[n - 1].clone();
        // Counted per server run, so the server is restarted fresh for this sync.
        w.server
            .set_env(vec![("CAIRN_FP_CRASH".into(), n.to_string())]);
        w.server.restart();
        let run = w.a.sync();
        let context = format!("server killed at step {n} ({name})");
        check_no_torn_files(&w.a);
        if run.code != Some(0) {
            assert!(!w.server.alive() || !run.ok(), "{context}: {}", run.text());
        }
        // A commit the client saw acknowledged is on the server after the restart, which the
        // convergence below proves: the client never resends what it has dequeued.
        w.server.set_env(Vec::new());
        w.server.restart();
        check_settled(&mut w, &context);
    });
}

shards!(killed_at_every_server_step_acknowledged_commits_stay: s01 s02 s03 s04 s05 s06 s07 s08
    s09 s10 s11 s12 s13 s14 s15 s16);

fn a_full_disk_at_every_client_write_loses_nothing(shard: Shard) {
    let w = world(Vec::new());
    let log = w.sb.path("steps");
    w.a.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
        .expect_ok();
    let steps = logged_steps(&log);
    drop(w);
    // Only steps that can report a full disk matter; counting all keeps the numbering shared.
    for_each_step(shard, steps.len(), |n| {
        let mut w = world(Vec::new());
        let n_s = n.to_string();
        let run = w.a.sync_env(&[("CAIRN_FP_ENOSPC", &n_s)]);
        let context = format!("disk full from step {n} ({})", steps[n - 1]);
        assert!(!run.was_killed(), "{context}: {}", run.text());
        if !run.ok() {
            assert!(
                run.stderr.contains("full")
                    || run.stderr.contains("space")
                    || run.stdout.contains("full"),
                "{context}: a full disk must be named as such: {}",
                run.text()
            );
        }
        check_no_torn_files(&w.a);
        assert!(
            leftover_temps(&w.a.root).is_empty(),
            "{context}: temporary files left: {:?}",
            leftover_temps(&w.a.root)
        );
        check_settled(&mut w, &context);
    });
}

shards!(a_full_disk_at_every_client_write_loses_nothing: s01 s02 s03 s04 s05 s06 s07 s08 s09 s10
    s11 s12 s13 s14 s15 s16 s17 s18 s19 s20 s21 s22 s23 s24 s25 s26 s27 s28 s29 s30 s31 s32);

fn a_full_disk_at_every_server_write_refuses_the_commit_and_loses_nothing(shard: Shard) {
    let probe = Sandbox::fast("matrix-probe");
    let log = probe.path("steps");
    let w = world(vec![("CAIRN_FP_LOG".into(), log.to_str().unwrap().into())]);
    std::fs::write(&log, "").unwrap();
    w.a.synced();
    let steps = logged_steps(&log);
    drop(w);

    for_each_step(shard, steps.len(), |n| {
        let mut w = world(Vec::new());
        w.server
            .set_env(vec![("CAIRN_FP_ENOSPC".into(), n.to_string())]);
        w.server.restart();
        let run = w.a.sync();
        let context = format!("server disk full from step {n} ({})", steps[n - 1]);
        assert!(!run.was_killed(), "{context}: {}", run.text());
        if !run.ok() {
            assert!(
                run.stderr.contains("disk is full"),
                "{context}: the client must say so: {}",
                run.text()
            );
        }
        check_no_torn_files(&w.a);
        w.server.set_env(Vec::new());
        w.server.restart();
        check_settled(&mut w, &context);
    });
}

shards!(a_full_disk_at_every_server_write_refuses_the_commit_and_loses_nothing: s01 s02 s03 s04
    s05 s06 s07 s08 s09 s10 s11 s12 s13 s14 s15 s16);
