//! A sync that carries a change, at the cost of what changed (engine incr.rs): every round it takes
//! that way is decided the full way as well in a harness build, on the same scan, journal and state,
//! and the sync fails where the two differ; `CAIRN_FP_INCR_ORACLE=0` turns that off, to see what a
//! release build does on its own. Symlinks and xattrs on Unix only, which Windows does not hold here.

mod common;
use common::*;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// As a release build runs: the incremental round's answer is the answer.
const RELEASE: (&str, &str) = ("CAIRN_FP_INCR_ORACLE", "0");

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

/// A sync with every failpoint it passes written down.
fn logged(d: &Device, env: &[(&str, &str)]) -> (Run, Vec<String>) {
    let log = d
        .root
        .with_extension(format!("steps-{:016x}", rand::random::<u64>()));
    let mut all: Vec<(&str, &str)> = env.to_vec();
    let path = log.to_str().unwrap().to_string();
    all.push(("CAIRN_FP_LOG", &path));
    let run = d.sync_env(&all);
    let steps = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let _ = std::fs::remove_file(&log);
    (run, steps)
}

fn took(steps: &[String], step: &str) -> bool {
    steps.iter().any(|s| s == step)
}

fn count(steps: &[String], step: &str) -> usize {
    steps.iter().filter(|s| *s == step).count()
}

/// A sync whose every round took what changed alone, the oracle agreeing.
#[track_caller]
fn incremental(d: &Device) -> Run {
    let (run, steps) = logged(d, &[]);
    assert!(run.ok(), "{}: {}", d.name, run.text());
    assert!(
        !took(&steps, "sync.incr.declined") && !run.text().contains("looked at the whole tree"),
        "{}: a round took the whole tree: {}",
        d.name,
        run.text()
    );
    let rounds = count(&steps, "sync.round");
    let fast = took(&steps, "sync.fast");
    // A sync the fast path took is run in full by its own oracle, which may go either way.
    if !fast {
        assert_eq!(count(&steps, "sync.incr.round"), rounds, "{steps:?}");
        assert_eq!(count(&steps, "sync.incr.confirmed"), rounds, "{steps:?}");
    }
    run
}

#[track_caller]
fn same_trees(a: &Device, b: &Device, context: &str) {
    let diff = Picture::of(&a.root).diff(
        &Picture::of(&b.root),
        Ignore {
            owner: true,
            ..Ignore::default()
        },
    );
    assert_eq!(diff, Vec::<String>::new(), "{context}");
}

/// Something of every kind, so that each change below has something to change.
fn everything(a: &Device) {
    a.write("f1", "first file");
    a.write("d/f2", "second file");
    a.write("d/e/f3", "third file");
    std::fs::create_dir_all(a.path("d/sub")).unwrap();
    a.write("h1", "hard linked");
    std::fs::hard_link(a.path("h1"), a.path("h2")).unwrap();
    #[cfg(unix)]
    symlink("f1", a.path("l")).unwrap();
    for i in 0..12 {
        a.write(&format!("fill/{i}"), format!("{i}"));
    }
    // Far from the root, where nothing else a round loads is next to it.
    a.write("deep/a/b/c/leaf", "deep down");
    a.write("deep/a/b/other", "beside");
}

/// Both devices through their first syncs, which take the whole tree and write the ledger.
fn started(label: &str) -> (Sandbox, Server, Device, Device) {
    let (sb, s, a, b) = pair(label);
    everything(&a);
    a.synced();
    b.synced();
    a.synced();
    b.synced();
    (sb, s, a, b)
}

/// How many entries a sync's output says it looked at, of how many.
fn looked_at(run: &Run) -> (u64, u64) {
    let line = run
        .stdout
        .lines()
        .find(|l| l.starts_with("looked at ") && l.contains(" entries"))
        .unwrap_or_else(|| panic!("no count of what was looked at: {}", run.text()));
    let words: Vec<&str> = line.split_whitespace().collect();
    (words[2].parse().unwrap(), words[4].parse().unwrap())
}

/// The whole tree, and why.
#[track_caller]
fn whole(run: &Run, why: &str) {
    assert!(run.ok(), "{}", run.text());
    assert!(
        run.stdout
            .lines()
            .any(|l| l.starts_with("looked at the whole tree: ") && l.contains(why)),
        "expected the whole tree, because {why}: {}",
        run.text()
    );
}

#[test]
fn a_change_goes_the_incremental_way_on_both_devices() {
    let (_sb, _s, a, b) = started("incr-basic");
    a.write("d/f2", "edited");
    let run = incremental(&a);
    let (seen, of) = looked_at(&run);
    // The file, its folder and what is beside it, the root and what is in it: not the whole tree.
    assert!(seen < of && seen <= 10, "{}", run.text());
    incremental(&b);
    assert_eq!(b.read("d/f2"), b"edited");
    same_trees(&a, &b, "after one edit");
    // And as a release build runs it, with no full round beside it.
    a.write("d/e/f3", "edited too");
    let (run, steps) = logged(&a, &[RELEASE]);
    assert!(
        run.ok() && took(&steps, "sync.incr.round"),
        "{}",
        run.text()
    );
    assert!(!took(&steps, "sync.incr.confirmed"), "{steps:?}");
    let (run, _) = logged(&b, &[RELEASE]);
    assert!(run.ok(), "{}", run.text());
    assert_eq!(b.read("d/e/f3"), b"edited too");
}

#[test]
fn every_kind_of_change_goes_the_incremental_way_and_reaches_the_other_device() {
    type Make = fn(&Device, &Device);
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut changes: Vec<(&str, Make)> = vec![
        ("an edit that keeps the size and the time", |a, _| {
            let (sec, nsec) = mtime_of(&a.path("f1"));
            a.write("f1", "FIRST FILE");
            set_mtime(&a.path("f1"), sec, nsec);
        }),
        ("a folder's time", |a, _| {
            set_mtime(&a.path("d"), 1_000_000, 5)
        }),
        ("a file's time", |a, _| {
            set_mtime(&a.path("f1"), 1_000_000, 5)
        }),
        ("a new file", |a, _| a.write("d/e/new", "new")),
        ("a new empty file", |a, _| a.write("d/e/empty", "")),
        ("a new folder", |a, _| {
            std::fs::create_dir(a.path("d/new")).unwrap()
        }),
        ("a new folder with things in it", |a, _| {
            a.write("n/m/one", "1");
            a.write("n/two", "2");
        }),
        ("a removed file", |a, _| {
            std::fs::remove_file(a.path("d/f2")).unwrap()
        }),
        ("a removed folder", |a, _| {
            std::fs::remove_dir(a.path("d/sub")).unwrap()
        }),
        ("a removed folder with things in it", |a, _| {
            std::fs::remove_dir_all(a.path("d")).unwrap()
        }),
        ("a rename", |a, _| {
            std::fs::rename(a.path("f1"), a.path("f1-renamed")).unwrap()
        }),
        ("a move", |a, _| {
            std::fs::rename(a.path("d/e/f3"), a.path("f3")).unwrap()
        }),
        ("a folder renamed", |a, _| {
            std::fs::rename(a.path("d"), a.path("d2")).unwrap()
        }),
        ("a folder moved into another", |a, _| {
            std::fs::rename(a.path("d/e"), a.path("fill/e")).unwrap()
        }),
        ("two names swapped", |a, _| {
            std::fs::rename(a.path("f1"), a.path("tmp")).unwrap();
            std::fs::rename(a.path("d/f2"), a.path("f1")).unwrap();
            std::fs::rename(a.path("tmp"), a.path("d/f2")).unwrap();
        }),
        ("a file replaced by a folder", |a, _| {
            std::fs::remove_file(a.path("f1")).unwrap();
            a.write("f1/inside", "now a folder");
        }),
        ("a hard link more", |a, _| {
            std::fs::hard_link(a.path("h1"), a.path("d/h3")).unwrap()
        }),
        ("a hard link less", |a, _| {
            std::fs::remove_file(a.path("h2")).unwrap()
        }),
        ("a hard-linked file edited", |a, _| {
            a.write("h2", "edited through h2")
        }),
        ("a mode", |a, _| chmod(&a.path("f1"), 0o444)),
        ("a folder's mode", |a, _| chmod(&a.path("d"), 0o700)),
        ("a folder's mode deep down", |a, _| {
            chmod(&a.path("deep/a/b/c"), 0o700)
        }),
        ("a file's mode deep down", |a, _| {
            chmod(&a.path("deep/a/b/c/leaf"), 0o444)
        }),
        ("a file's time deep down", |a, _| {
            set_mtime(&a.path("deep/a/b/c/leaf"), 1_000_000, 5)
        }),
        ("a change on the other device", |_, b| {
            b.write("d/from-b", "b");
            b.write("f1", "edited on b");
            std::fs::rename(b.path("fill"), b.path("d/fill")).unwrap();
            b.synced();
        }),
        ("a removal on the other device", |_, b| {
            std::fs::remove_dir_all(b.path("d")).unwrap();
            b.synced();
        }),
        ("many changes at once", |a, _| {
            for i in 0..12 {
                a.write(&format!("fill/{i}"), format!("edited {i}"));
            }
            a.write("d/e/f3", "edited");
            std::fs::rename(a.path("d/sub"), a.path("sub")).unwrap();
            std::fs::remove_file(a.path("h1")).unwrap();
        }),
    ];
    // What Windows does not hold here: xattrs, symlinks.
    #[cfg(unix)]
    changes.extend::<[(&str, Make); 4]>([
        ("an xattr", |a, _| {
            set_xattr(&a.path("f1"), "user.cairn", b"x")
        }),
        ("an xattr on a folder", |a, _| {
            set_xattr(&a.path("d"), "user.cairn", b"x")
        }),
        ("an xattr on a folder deep down", |a, _| {
            set_xattr(&a.path("deep/a/b"), "user.cairn", b"x")
        }),
        ("a symlink made to point elsewhere", |a, _| {
            std::fs::remove_file(a.path("l")).unwrap();
            symlink("d/f2", a.path("l")).unwrap();
        }),
    ]);
    for (what, make) in &changes {
        let (_sb, _s, a, b) = started("incr-changes");
        make(&a, &b);
        incremental(&a);
        incremental(&b);
        incremental(&a);
        same_trees(&a, &b, what);
    }
}

/// What both devices change at once meets in the merge, among neighbours: each side's syncs take what
/// changed alone, and the trees come out as the full sync's would, which the oracle checks round by
/// round.
#[test]
fn changes_that_meet_go_the_incremental_way() {
    type Both = fn(&Device, &Device);
    let cases: Vec<(&str, Both)> = vec![
        ("the same file edited on both", |a, b| {
            a.write("d/f2", "edited on a");
            b.write("d/f2", "edited on b");
        }),
        ("the same new name, different content", |a, b| {
            a.write("d/same", "made on a");
            b.write("d/same", "made on b");
        }),
        ("the same new name, the same content", |a, b| {
            a.write("d/same", "the same");
            b.write("d/same", "the same");
        }),
        ("the same new folder on both", |a, b| {
            a.write("n/one", "1");
            b.write("n/two", "2");
        }),
        (
            "a folder removed where the other edits inside it",
            |a, b| {
                std::fs::remove_dir_all(a.path("d")).unwrap();
                b.write("d/e/f3", "edited on b");
            },
        ),
        (
            "a folder removed here, the server's edit far inside it",
            |a, b| {
                // The edit reaches a first, where only the top of what is gone and the edited file's own
                // folder are next to a change: the folders between come from above the file.
                b.write("deep/a/b/c/leaf", "edited on b");
                b.synced();
                std::fs::remove_dir_all(a.path("deep/a")).unwrap();
            },
        ),
        ("a folder renamed where the other adds to it", |a, b| {
            std::fs::rename(a.path("d"), a.path("d2")).unwrap();
            b.write("d/e/new", "new on b");
        }),
        ("moves that would make a cycle", |a, b| {
            std::fs::rename(a.path("d"), a.path("fill/d")).unwrap();
            std::fs::rename(b.path("fill"), b.path("d/fill")).unwrap();
        }),
        ("one renamed to a name the other makes", |a, b| {
            std::fs::rename(a.path("f1"), a.path("d/taken")).unwrap();
            b.write("d/taken", "made on b");
        }),
        ("a hard link removed where the other edits it", |a, b| {
            std::fs::remove_file(a.path("h2")).unwrap();
            b.write("h1", "edited on b");
        }),
        ("the same file moved to two places", |a, b| {
            std::fs::rename(a.path("f1"), a.path("d/f1")).unwrap();
            std::fs::rename(b.path("f1"), b.path("fill/f1")).unwrap();
        }),
        ("a mode on one, an edit on the other", |a, b| {
            chmod(&a.path("f1"), 0o444);
            b.write("f1", "edited on b");
        }),
        ("a conflict copy's name already taken beside it", |a, b| {
            // Edited in place on both: nothing moves the folder's times, and the journal says nothing of
            // the folder, so only the round's own rule about a changed file's folder sees the name.
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let date = cairn_engine::sync::utc_date(secs);
            for host in ["a", "b"] {
                a.write(&format!("d/f2 (conflict {host} {date})"), "already here");
            }
            a.synced();
            b.synced();
            a.synced();
            b.synced();
            std::fs::write(a.path("d/f2"), "edited on a").unwrap();
            std::fs::write(b.path("d/f2"), "edited on b").unwrap();
        }),
        ("the same mode set differently", |a, b| {
            chmod(&a.path("f1"), 0o444);
            chmod(&b.path("f1"), 0o400);
        }),
    ];
    for (what, both) in &cases {
        let (_sb, _s, a, b) = started("incr-meet");
        both(&a, &b);
        for d in [&a, &b, &a, &b, &a] {
            incremental(d);
        }
        same_trees(&a, &b, what);
    }
}

/// Rules by path: what the server has out of view here stays in sight of every round, a folder the
/// server moves takes everything in it out of view or into it, and a folder removed here that holds what
/// a rule leaves out comes back, empty of that.
#[test]
fn what_rules_leave_out_goes_the_incremental_way() {
    type Step = fn(&Device, &Device);
    let cases: Vec<(&str, Step)> = vec![
        ("an edit beside what is out of view", |a, _| {
            a.write("d/f2", "edited")
        }),
        ("a folder moved out of view on the server", |_, b| {
            std::fs::rename(b.path("d/e"), b.path("d/cache/e")).unwrap();
            b.synced();
        }),
        ("a folder moved into view on the server", |_, b| {
            std::fs::rename(b.path("d/cache/inside"), b.path("d/inside")).unwrap();
            b.synced();
        }),
        ("a folder renamed into what a rule leaves out", |_, b| {
            std::fs::rename(b.path("fill"), b.path("cache")).unwrap();
            b.synced();
        }),
        (
            "a folder removed here holding what a rule leaves out",
            |a, _| std::fs::remove_dir_all(a.path("d")).unwrap(),
        ),
        ("a new file in a folder out of view", |_, b| {
            b.write("d/cache/inside/new", "new");
            b.synced();
        }),
    ];
    for (what, step) in &cases {
        let (_sb, _s, a, b) = pair("incr-rules");
        everything(&a);
        a.write("d/cache/inside/x", "out of view on a");
        a.synced();
        b.synced();
        std::fs::write(a.path(".cairn/ignore"), "cache/\n").unwrap();
        a.synced();
        a.synced();
        b.synced();
        step(&a, &b);
        for d in [&a, &b, &a, &b] {
            incremental(d);
        }
        let ignore = Ignore {
            owner: true,
            ..Ignore::default()
        };
        // Everything b has, a has, but for what a leaves out.
        let pa = Picture::of(&a.root);
        let pb = Picture::of(&b.root);
        let diff: Vec<String> = pa
            .diff(&pb, ignore)
            .into_iter()
            .filter(|l| !l.contains("cache"))
            .collect();
        assert_eq!(diff, Vec::<String>::new(), "{what}");
    }
}

/// A folder the server moves takes what a rule by path leaves out deep inside it here where the rule
/// does not match: the round sees that folder, however far down, and it stays out by inode, said.
#[test]
fn a_rule_by_path_deep_in_a_folder_moved_elsewhere_keeps_out_what_it_left_out() {
    let (_sb, _s, a, b) = pair("incr-rule-moves");
    everything(&a);
    a.write("d/e/g/cache/x", "left out");
    a.write("d/e/g/kept", "in view");
    std::fs::write(a.path(".cairn/ignore"), "/d/e/g/cache/\n").unwrap();
    // Everything settles first, so that the ledger vouches for the folder and nothing but the round's
    // own rule about what is excluded brings it in.
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    a.synced();
    std::fs::rename(b.path("d"), b.path("d2")).unwrap();
    incremental(&b);
    let (run, steps) = logged(&a, &[]);
    assert!(run.ok(), "{}", run.text());
    assert!(took(&steps, "sync.incr.confirmed"), "{steps:?}");
    assert!(
        run.stdout.contains(
            "d/e/g/cache: its folder is moving, and the rule that leaves it out would not match there"
        ),
        "{}",
        run.text()
    );
    assert_eq!(a.read("d2/e/g/cache/x"), b"left out");
    assert!(!b.exists("d2/e/g/cache"));
}

/// The mass-delete check counts what a round left out as there, as it is: a folder of ten of sixty
/// files gone is no mass delete, though it is most of what the round looked at, and most of the tree is,
/// as for the full sync.
#[test]
fn the_mass_delete_check_counts_what_a_round_leaves_out() {
    let (_sb, _s, a, b) = pair("incr-vanished");
    for i in 0..60 {
        a.write(&format!("many/{}/{i}", i % 6), format!("{i}"));
    }
    a.synced();
    b.synced();
    a.synced();
    for i in (0..60).step_by(6) {
        std::fs::remove_file(a.path(&format!("many/0/{i}"))).unwrap();
    }
    let run = incremental(&a);
    let (seen, of) = looked_at(&run);
    assert!(seen < 25 && of > 60, "{}", run.text());
    for i in (0..60).filter(|i| i % 6 != 0 && i % 6 != 5) {
        std::fs::remove_file(a.path(&format!("many/{}/{i}", i % 6))).unwrap();
    }
    let (run, steps) = logged(&a, &[]);
    assert_eq!(run.code, Some(2), "{}", run.text());
    assert!(run.stdout.contains("have vanished"), "{}", run.text());
    assert!(took(&steps, "sync.incr.confirmed"), "{steps:?}");
    let run = a.cairn(&["sync", "--allow-mass-delete"]);
    assert!(run.ok(), "{}", run.text());
    incremental(&b);
    assert!(!b.exists("many/1/1") && b.exists("many/5/5"));
}

/// A manifest two files share stays while one of them still holds it, whatever the other does.
#[test]
fn a_manifest_another_file_holds_stays() {
    let (_sb, _s, a, b) = started("incr-manifests");
    a.write("twin1", "the same bytes in both");
    a.write("d/twin2", "the same bytes in both");
    incremental(&a);
    incremental(&a);
    a.write("twin1", "now different");
    incremental(&a);
    incremental(&a);
    std::fs::rename(a.path("d/twin2"), a.path("d/twin3")).unwrap();
    incremental(&a);
    incremental(&b);
    same_trees(&a, &b, "after the twins went their ways");
}

#[test]
fn a_folder_of_thousands_renamed_looks_at_what_is_in_it_and_no_more() {
    let (_sb, _s, a, b) = pair("incr-rename");
    for i in 0..2_000 {
        a.write(&format!("big/{}/{i}", i % 40), format!("{i}"));
        a.write(&format!("other/{}/{i}", i % 40), format!("other {i}"));
    }
    a.synced();
    b.synced();
    a.synced();
    b.synced();
    // One edit deep in it: the file, its folder and what is in it, the folder above and what is in
    // that, as the edit moved its folder's times.
    a.write("big/7/7", "edited");
    let (took, of) = looked_at(&incremental(&a));
    assert!(of > 4_000 && took < 150, "{took} of {of}");
    let (took, _) = looked_at(&incremental(&b));
    assert!(took < 150, "{took} of {of}");
    // The rename: the folder, and what is in it, one level down.
    std::fs::rename(a.path("big"), a.path("renamed")).unwrap();
    let (took, of) = looked_at(&incremental(&a));
    assert!(took < 100, "{took} of {of}");
    let (took, _) = looked_at(&incremental(&b));
    assert!(took < 100, "{took} of {of}");
    assert_eq!(b.read("renamed/7/7"), b"edited");
    same_trees(&a, &b, "after the rename");
}

#[test]
fn the_whole_tree_is_taken_and_said_where_the_ledger_cannot_vouch() {
    let (_sb, _s, a, b) = pair("incr-whole");
    everything(&a);
    whole(&a.sync(), "no full sync has agreed");
    whole(&b.sync(), "no full sync has agreed");
    incremental(&a);
    // A rule.
    std::fs::write(a.path(".cairn/ignore"), "d/sub/\n").unwrap();
    whole(&a.sync(), "the rules");
    incremental(&a);
    // A dry run.
    a.write("d/f2", "edited");
    let run = a.cairn(&["sync", "--dry-run"]);
    whole(&run, "a dry run");
    incremental(&a);
    // Too much at once for what the tree is.
    a.write("f1", "edited");
    let run = a.sync_env(&[("CAIRN_FP_INCR_LIMIT", "2")]);
    whole(&run, "more than taking the whole tree costs");
    let run = b.sync_env(&[("CAIRN_FP_INCR_LIMIT", "1")]);
    whole(&run, "the server's journal brings");
    assert_eq!(b.read("f1"), b"edited");
    incremental(&b);
    // The device's name.
    let path = a.path(".cairn/config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["device_name"] = "renamed".into();
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    whole(&a.sync(), "the configuration");
    incremental(&a);
    // A copy kept here by `cairn exclude`, found by inode wherever it goes.
    let run = a.cairn(&["exclude", "fill"]);
    assert!(run.ok(), "{}", run.text());
    a.write("f1", "edited again");
    whole(&a.sync(), "copies kept here");
    whole(&a.sync(), "copies kept here");
    incremental(&b);
    assert_eq!(b.read("f1"), b"edited again");
}

/// Writes to the state by anything else, an older build or a person with sqlite3, mark what they touch,
/// and the next round takes it up: here a row made racy, which a full scan reads again and a row that
/// was trusted would not.
#[test]
fn a_row_written_by_another_build_is_taken_up() {
    let (_sb, _s, a, b) = started("incr-foreign");
    let db = a.path(".cairn/state.sqlite");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let n = conn
        .execute(
            "UPDATE rows SET row = replace(row, '\"racy\":false', '\"racy\":true')",
            [],
        )
        .unwrap();
    assert!(n > 5, "{n}");
    drop(conn);
    a.write("d/f2", "edited");
    // Every row was written: every object is looked at.
    let run = incremental(&a);
    let (took, of) = looked_at(&run);
    assert_eq!(took, of, "{}", run.text());
    incremental(&b);
    same_trees(&a, &b, "after the rows were written");
    // A missing trigger means writes went unmarked: the ledger goes, and the whole tree is looked at.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("DROP TRIGGER dirty_rows_update;")
        .unwrap();
    drop(conn);
    whole(&a.sync(), "no full sync has agreed");
    incremental(&a);
}

/// A sync that must succeed, every incremental round of it confirmed by the oracle: whether it took the
/// whole tree, and its output.
#[track_caller]
fn checked(d: &Device) -> (bool, Run) {
    let (run, steps) = logged(d, &[]);
    assert!(run.ok(), "{}: {}", d.name, run.text());
    let whole = run.text().contains("looked at the whole tree");
    if !whole && !took(&steps, "sync.fast") {
        assert_eq!(
            count(&steps, "sync.incr.round"),
            count(&steps, "sync.incr.confirmed"),
            "{steps:?}"
        );
    }
    (whole, run)
}

/// With the oracle, and as a release build runs, with none.
fn both_ways() -> [Vec<(&'static str, &'static str)>; 2] {
    [vec![], vec![RELEASE]]
}

/// A folder's mode and an xattr deep in the tree, here and there, straight after the sync that recorded
/// the last, again and again: the case the oracle once failed, under load, from a stale build.
#[cfg(unix)]
#[test]
fn a_folders_metadata_deep_down_changed_again_and_again_on_both_sides() {
    for i in 0..2 {
        let (_sb, _s, a, b) = started("incr-deep-mode");
        chmod(&a.path("deep/a/b/c"), 0o700);
        set_xattr(&a.path("deep/a/b"), "user.incr", format!("{i}").as_bytes());
        for d in [&a, &b, &a, &b] {
            checked(d);
        }
        same_trees(&a, &b, "a folder's mode and xattr deep down");
        chmod(&b.path("deep/a/b/c"), 0o750);
        chmod(&b.path("deep/a"), 0o700);
        for d in [&b, &a, &b, &a] {
            checked(d);
        }
        same_trees(&a, &b, "again from the other side");
    }
}

/// A folder deleted elsewhere, far deeper than the four widenings a round allows itself: the journal
/// names everything in it.
#[test]
fn a_folder_deleted_elsewhere_deeper_than_the_widenings_goes_here_too() {
    let (_sb, _s, a, b) = pair("incr-deep-delete");
    everything(&a);
    a.write("p/1/2/3/4/5/6/7/leaf", "deep");
    a.write("p/1/2/3/4/5/side", "side");
    a.write("p/1/x", "x");
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    std::fs::remove_dir_all(b.path("p")).unwrap();
    checked(&b);
    let (_, run) = checked(&a);
    assert!(!a.exists("p"), "{}", run.text());
    same_trees(&a, &b, "after a deep delete");
}

/// The other device moves a deep folder into one this device excludes by identity, then one out of it:
/// what goes out of view goes from here, unchanged, and what comes into view comes down. There is no rule
/// to take the moved folder whole: the copy `cairn exclude` keeps sends such a root the whole way.
#[test]
fn moves_into_and_out_of_a_folder_excluded_by_identity() {
    let (_sb, _s, a, b) = pair("incr-excluded-moves");
    everything(&a);
    a.write("proj/1/2/3/4/5/6/leaf", "deep");
    a.write("proj/1/2/3/side", "side");
    a.write("vault/old/1/2/3/4/5/6/kept", "kept");
    a.write("vault/old/1/top", "top");
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    let run = a.cairn(&["exclude", "vault", "--remove-local"]);
    assert!(run.ok(), "{}", run.text());
    assert!(!a.exists("vault"));
    for d in [&a, &a, &b] {
        checked(d);
    }
    std::fs::rename(b.path("proj"), b.path("vault/proj")).unwrap();
    checked(&b);
    let (_, run) = checked(&a);
    assert!(
        !a.exists("proj"),
        "moved out of view elsewhere: {}",
        run.text()
    );
    std::fs::rename(b.path("vault/old"), b.path("old")).unwrap();
    checked(&b);
    let (_, run) = checked(&a);
    assert_eq!(a.read("old/1/2/3/4/5/6/kept"), b"kept", "{}", run.text());
    checked(&a);
    let diff: Vec<String> = Picture::of(&a.root)
        .diff(
            &Picture::of(&b.root),
            Ignore {
                owner: true,
                ..Ignore::default()
            },
        )
        .into_iter()
        .filter(|l| !l.starts_with("vault"))
        .collect();
    assert_eq!(diff, Vec::<String>::new());
}

/// A folder moved elsewhere deep under what a rule leaves out here, then out again.
#[test]
fn a_folder_moved_under_a_rule_elsewhere_and_out_again() {
    let (_sb, _s, a, b) = pair("incr-rule-moves-back");
    everything(&a);
    a.write("proj/1/2/3/4/5/6/leaf", "deep");
    a.write("proj/1/2/x.log", "log");
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    std::fs::write(a.path(".cairn/ignore"), "*.log\n/cache/\n").unwrap();
    for d in [&a, &a, &b] {
        checked(d);
    }
    std::fs::create_dir_all(b.path("cache")).unwrap();
    std::fs::rename(b.path("proj"), b.path("cache/proj")).unwrap();
    checked(&b);
    let (_, run) = checked(&a);
    assert!(!a.exists("proj/1/2/3/4/5/6/leaf"), "{}", run.text());
    std::fs::rename(b.path("cache/proj"), b.path("proj2")).unwrap();
    checked(&b);
    let (_, run) = checked(&a);
    assert_eq!(a.read("proj2/1/2/3/4/5/6/leaf"), b"deep", "{}", run.text());
    checked(&a);
}

/// A hard-linked file with its names in two folders: one name renamed elsewhere onto a name this device
/// has just made in the other folder, while this device edits it through the first name.
#[test]
fn hard_links_across_folders_meet_new_names() {
    let (_sb, _s, a, b) = pair("incr-links-meet");
    everything(&a);
    a.write("p/h", "linked");
    std::fs::create_dir_all(a.path("q")).unwrap();
    std::fs::hard_link(a.path("p/h"), a.path("q/h2")).unwrap();
    a.write("q/other", "other");
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    std::fs::rename(b.path("q/h2"), b.path("q/taken")).unwrap();
    std::fs::rename(b.path("q/other"), b.path("p/other")).unwrap();
    checked(&b);
    a.write("q/taken", "made on a");
    std::fs::write(a.path("p/h"), "edited through p/h").unwrap();
    for d in [&a, &b, &a, &b, &a] {
        checked(d);
    }
    same_trees(&a, &b, "after the links met new names");
}

/// A conflict whose copy's name is taken, on an edit elsewhere met by one here that keeps the size and
/// the time, so that only the file's ctime says it changed.
#[cfg(unix)]
#[test]
fn a_conflict_on_an_edit_that_keeps_size_and_time() {
    let (_sb, _s, a, b) = started("incr-conflict-time");
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let date = cairn_engine::sync::utc_date(secs);
    a.write(&format!("d/f2 (conflict a {date})"), "already here");
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    b.write("d/f2", "EDITED ON b");
    checked(&b);
    let (sec, nsec) = mtime_of(&a.path("d/f2"));
    std::fs::write(a.path("d/f2"), "SECOND FILE").unwrap();
    set_mtime(&a.path("d/f2"), sec, nsec);
    for d in [&a, &b, &a, &b] {
        checked(d);
    }
    same_trees(&a, &b, "after the conflict");
}

/// Whether a folder holds this name in exactly this case.
#[cfg(any(target_os = "macos", windows))]
fn exists_exact(d: &Device, dir: &str, name: &str) -> bool {
    std::fs::read_dir(d.path(dir))
        .map(|r| r.flatten().any(|e| e.file_name().to_str() == Some(name)))
        .unwrap_or(false)
}

/// On a filesystem that folds case: a new name here that the server has in another case, in a round
/// that takes what changed alone, waits, said; renamed to the server's form, the two merge.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_name_in_another_case_waits_in_an_incremental_round() {
    let (_sb, _s, a, b) = started("incr-case");
    a.write("d/Readme.txt", "from a");
    checked(&a);
    b.write("d/README.txt", "from b");
    b.write("d/sub/NEW.txt", "new on b");
    let (whole, run) = checked(&b);
    assert!(!whole, "{}", run.text());
    let skipped = |run: &Run| {
        run.stdout
            .lines()
            .any(|l| l.starts_with("skipped: d/README.txt"))
    };
    assert!(skipped(&run), "{}", run.text());
    checked(&a);
    assert!(
        !exists_exact(&a, "d", "README.txt"),
        "went up as a second name"
    );
    // Again, with nothing new: still said, still nothing up.
    let (_, run) = checked(&b);
    assert!(skipped(&run), "{}", run.text());
    std::fs::rename(b.path("d/README.txt"), b.path("d/Readme.txt")).unwrap();
    for d in [&b, &a, &b, &a] {
        checked(d);
    }
    same_trees(&a, &b, "after the rename to the server's case");
}

/// On APFS: the decomposed form of a name the server has composed.
#[cfg(target_os = "macos")]
#[test]
fn a_decomposed_name_waits_in_an_incremental_round() {
    let (_sb, _s, a, b) = started("incr-nfd");
    let nfc = "d/Caf\u{e9}";
    let nfd = "d/Cafe\u{301}";
    a.write(&format!("{nfc}/menu"), "soup");
    checked(&a);
    b.write(&format!("{nfd}/menu"), "soup");
    b.write(&format!("{nfd}/mine"), "mine");
    let (whole, run) = checked(&b);
    assert!(!whole, "{}", run.text());
    assert!(run.stdout.contains("skipped: "), "{}", run.text());
    checked(&a);
    let (_, run) = checked(&b);
    assert!(run.stdout.contains("skipped: "), "{}", run.text());
    std::fs::rename(b.path(nfd), b.path(nfc)).unwrap();
    for d in [&b, &a, &b, &a] {
        checked(d);
    }
    assert_eq!(a.read(&format!("{nfc}/mine")), b"mine");
}

/// NTFS lets a program put back every time of an entry, its change time too, as backup and copy tools
/// can: here a rename, a move and a removal whose file and folders keep their times. The full sync reads
/// the new names from the scan; the incremental round sees them by the names alone, as no time moved.
#[cfg(windows)]
#[test]
fn a_rename_with_every_time_put_back_is_seen_by_its_name() {
    let (_sb, _s, a, b) = started("incr-ntfs-times");
    let file = a.path("d/f2");
    let folder = a.path("d");
    let (tf, td) = (ntfs::times(&file), ntfs::times(&folder));
    std::fs::rename(&file, a.path("d/renamed")).unwrap();
    ntfs::restore(&a.path("d/renamed"), tf);
    ntfs::restore(&folder, td);
    assert_eq!(ntfs::times(&folder), td);
    for d in [&a, &b, &a] {
        checked(d);
    }
    assert!(b.exists("d/renamed") && !b.exists("d/f2"));
    let file = a.path("d/e/f3");
    let (from, to) = (a.path("d/e"), a.path("fill"));
    let (tf, t1, t2) = (ntfs::times(&file), ntfs::times(&from), ntfs::times(&to));
    std::fs::rename(&file, a.path("fill/f3")).unwrap();
    ntfs::restore(&a.path("fill/f3"), tf);
    ntfs::restore(&from, t1);
    ntfs::restore(&to, t2);
    for d in [&a, &b, &a] {
        checked(d);
    }
    assert!(b.exists("fill/f3") && !b.exists("d/e/f3"));
    let folder = a.path("fill");
    let t = ntfs::times(&folder);
    std::fs::remove_file(a.path("fill/3")).unwrap();
    ntfs::restore(&folder, t);
    for d in [&a, &b, &a] {
        checked(d);
    }
    assert!(!b.exists("fill/3"));
    same_trees(&a, &b, "after renames with every time put back");
}

/// An edit with every time put back: the full sync misses it too, as both trust the fingerprint, and a
/// release build's incremental round must leave what the full sync does.
#[cfg(windows)]
#[test]
fn an_edit_with_every_time_put_back_goes_as_the_full_sync_goes() {
    let (_sb, _s, a, b) = started("incr-ntfs-edit");
    let file = a.path("d/f2");
    let t = ntfs::times(&file);
    std::fs::write(&file, "SECOND FILE").unwrap();
    ntfs::restore(&file, t);
    for d in [&a, &b] {
        let run = d.sync_env(&[RELEASE]);
        assert!(run.ok(), "{}", run.text());
    }
    let incremental = b.read("d/f2");
    for d in [&a, &b] {
        let run = d.sync_env(&[("CAIRN_FP_INCR", "0")]);
        assert!(run.ok(), "{}", run.text());
    }
    assert_eq!(b.read("d/f2"), incremental);
}

#[cfg(windows)]
mod ntfs {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, GetFileInformationByHandleEx, SetFileInformationByHandle, FILE_BASIC_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_WRITE_ATTRIBUTES,
    };

    fn open(p: &Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)
            .unwrap()
    }

    fn basic(f: &std::fs::File) -> FILE_BASIC_INFO {
        // SAFETY: the buffer is the struct this class fills, for the call's length.
        unsafe {
            let mut b: FILE_BASIC_INFO = std::mem::zeroed();
            assert!(
                GetFileInformationByHandleEx(
                    f.as_raw_handle() as _,
                    FileBasicInfo,
                    (&mut b as *mut FILE_BASIC_INFO).cast(),
                    std::mem::size_of::<FILE_BASIC_INFO>() as u32,
                ) != 0
            );
            b
        }
    }

    /// Last write, change and creation time.
    pub fn times(p: &Path) -> (i64, i64, i64) {
        let b = basic(&open(p));
        (b.LastWriteTime, b.ChangeTime, b.CreationTime)
    }

    /// Every one of them put back, the change time too.
    pub fn restore(p: &Path, (write, change, creation): (i64, i64, i64)) {
        let f = open(p);
        let mut b = basic(&f);
        b.LastWriteTime = write;
        b.ChangeTime = change;
        b.CreationTime = creation;
        b.LastAccessTime = 0;
        // SAFETY: the buffer is the struct this class reads, for the call's length.
        unsafe {
            assert!(
                SetFileInformationByHandle(
                    f.as_raw_handle() as _,
                    FileBasicInfo,
                    (&b as *const FILE_BASIC_INFO).cast(),
                    std::mem::size_of::<FILE_BASIC_INFO>() as u32,
                ) != 0
            );
        }
    }
}

/// A folder whose name this filesystem refuses (NTFS a reserved or `:` name, APFS one not UTF-8), with
/// things in it, is out of view here; renamed elsewhere to a name it takes, it comes down whole, in a
/// round that takes what changed alone. The journal names the folder alone, so only the rule that opens
/// a folder decided about sees what is in it, and only widening what is further down.
#[test]
fn a_folder_this_device_cannot_hold_renamed_elsewhere_comes_down_whole() {
    for env in both_ways() {
        let (_sb, _s, a, b) = pair("incr-refused-folder");
        let refuse = ("CAIRN_FP_REFUSE_NAME", "refused");
        everything(&a);
        a.write("refused/inner/deep", "under a folder\n");
        a.write("refused/top", "top\n");
        a.synced();
        for _ in 0..3 {
            assert!(b.sync_env(&[refuse]).ok());
        }
        assert!(!b.exists("refused"));
        std::fs::rename(a.path("refused"), a.path("accepted")).unwrap();
        a.synced();
        let mut env = env.clone();
        env.push(refuse);
        let (run, steps) = logged(&b, &env);
        assert!(run.ok(), "{}", run.text());
        assert!(took(&steps, "sync.incr.round"), "{steps:?}");
        assert_eq!(b.read("accepted/top"), b"top\n", "{}", run.text());
        assert_eq!(
            b.read("accepted/inner/deep"),
            b"under a folder\n",
            "{}",
            run.text()
        );
    }
}

/// Something out of view here, deep in a folder a rule leaves out, edited elsewhere: the round needs the
/// folders above it, which nothing here has on disk, to know it is out of view.
#[test]
fn an_edit_elsewhere_deep_in_what_a_rule_leaves_out_stays_out() {
    let (_sb, _s, a, b) = pair("incr-hidden-edit");
    everything(&a);
    a.write("d/e/cache/x", "out of view on a");
    a.write("d/e/cache/y/z", "deeper");
    for d in [&a, &b] {
        d.synced();
    }
    std::fs::write(a.path(".cairn/ignore"), "cache/\n").unwrap();
    for d in [&a, &a, &b, &a] {
        checked(d);
    }
    assert!(a.exists("d/e/cache/x"), "a rule keeps what is here");
    std::fs::remove_dir_all(a.path("d/e/cache")).unwrap();
    for d in [&a, &a] {
        checked(d);
    }
    b.write("d/e/cache/x", "edited on b");
    b.write("d/e/cache/y/z", "edited deeper on b");
    b.write("d/e/cache/new", "new on b");
    checked(&b);
    for d in [&a, &a] {
        checked(d);
    }
    assert!(
        !a.exists("d/e/cache"),
        "came down though a rule leaves it out"
    );
    assert_eq!(b.read("d/e/cache/x"), b"edited on b");
}

/// A rule that leaves out only some files deep in a folder: the server moves the folder to where the
/// rule matches them, and back. Nothing else in the folder changes, so only the rule that takes a moved
/// folder whole sees them, both ways.
#[test]
fn a_rule_matching_only_files_deep_in_a_folder_moved_elsewhere() {
    for env in both_ways() {
        let (_sb, _s, a, b) = pair("incr-pattern");
        everything(&a);
        a.write("archive/keep", "keep");
        a.write("proj/1/2/3/x.log", "log");
        a.write("proj/1/2/3/y.txt", "text");
        a.write("proj/1/z.log", "log2");
        for d in [&a, &b] {
            d.synced();
        }
        std::fs::write(a.path(".cairn/ignore"), "/archive/**/*.log\n").unwrap();
        for d in [&a, &a, &b, &a] {
            checked(d);
        }
        std::fs::rename(b.path("proj"), b.path("archive/proj")).unwrap();
        checked(&b);
        for _ in 0..2 {
            let (run, _) = logged(&a, &env);
            assert!(run.ok(), "{}", run.text());
        }
        // Out of view here now, and the server has it: it goes from here, as in the full sync.
        assert!(!a.exists("archive/proj/1/2/3/x.log"));
        assert_eq!(a.read("archive/proj/1/2/3/y.txt"), b"text");
        // b edits the log while a leaves it out, then moves the folder back out of the rule.
        b.write("archive/proj/1/2/3/x.log", "log edited on b");
        std::fs::rename(b.path("archive/proj"), b.path("proj2")).unwrap();
        checked(&b);
        let (run, _) = logged(&a, &env);
        assert!(run.ok(), "{}", run.text());
        assert!(
            a.exists("proj2/1/2/3/x.log"),
            "after one sync: {}",
            run.text()
        );
        let (run, _) = logged(&a, &env);
        assert!(run.ok(), "{}", run.text());
        assert_eq!(
            a.read("proj2/1/2/3/x.log"),
            b"log edited on b",
            "{}",
            run.text()
        );
        assert_eq!(a.read("proj2/1/z.log"), b"log2");
    }
}

/// With no failpoint, on a Mac: a device on a case-sensitive volume (`CAIRN_CASE_SENSITIVE_DIR`, say a
/// mounted disk image) holds two folders whose names differ only in case, each with things in it, and
/// this one, on the ordinary case-insensitive volume, can hold only one. The other is out of view here
/// until the first device renames it: then it comes down whole, in a round that takes what changed alone.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs a case-sensitive volume, at CAIRN_CASE_SENSITIVE_DIR"]
fn a_case_twin_folder_renamed_on_a_case_sensitive_volume_comes_down_whole() {
    let cs = std::env::var("CAIRN_CASE_SENSITIVE_DIR").expect("CAIRN_CASE_SENSITIVE_DIR");
    for env in both_ways() {
        let sb = Sandbox::on_disk("incr-case-twin");
        let server = Server::start(sb.path("server"));
        let other = Sandbox::under(Path::new(&cs), "incr-case-twin-a");
        let a = Device::new(&other, &server, "a");
        let b = Device::new(&sb, &server, "b");
        everything(&a);
        a.write("Proj/1/2/inner", "upper");
        a.write("Proj/top", "upper top");
        a.write("proj/1/2/inner", "lower");
        a.write("proj/top", "lower top");
        assert!(
            a.exists("Proj/top") && a.exists("proj/top"),
            "not a case-sensitive volume"
        );
        a.synced();
        for _ in 0..3 {
            let run = b.sync();
            assert!(run.ok(), "{}", run.text());
        }
        let held: Vec<String> = std::fs::read_dir(&b.root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.eq_ignore_ascii_case("proj"))
            .collect();
        assert_eq!(held.len(), 1, "{held:?}");
        let away = if held[0] == "Proj" { "proj" } else { "Proj" };
        std::fs::rename(a.path(away), a.path("renamed")).unwrap();
        a.synced();
        let (run, steps) = logged(&b, &env);
        assert!(run.ok(), "{}", run.text());
        assert!(took(&steps, "sync.incr.round"), "{steps:?}");
        assert!(
            b.exists("renamed/1/2/inner"),
            "after one sync: {}",
            run.text()
        );
        assert!(b.exists("renamed/top"), "after one sync: {}", run.text());
    }
}

/// One inode at two places in a scan the sync goes on with: a file moved, while the walk went, from a
/// folder it had been through to one it had not, and no second scan to settle it. The full sync takes the
/// first place for the file and the second for something new; the round, which keeps one place an inode,
/// goes the whole way instead, and says so.
#[test]
fn an_inode_the_scan_finds_twice_sends_the_round_the_whole_way() {
    let (sb, _s, a, b) = started("incr-inode-twice");
    a.write("p/m", "moves while the walk goes");
    std::fs::create_dir_all(a.path("q")).unwrap();
    for d in [&a, &b, &a, &b] {
        d.synced();
    }
    // Past the racy window, so that the walk reads no file before the new one, in p after p/m.
    std::thread::sleep(std::time::Duration::from_millis(300));
    a.synced();
    a.write("p/n", "new: read while the walk is in p");
    let gate = sb.path("gate-inode-twice");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("scan.file.reading:{}", gate.display());
    let device = a.clone();
    let worker = std::thread::spawn(move || {
        device.sync_env(&[
            ("CAIRN_FP_PAUSE", &pause),
            ("CAIRN_FP_TUNE", "sync.scan_attempts=1"),
        ])
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !gate.join("reached").exists() {
        assert!(std::time::Instant::now() < deadline, "no file was read");
        assert!(
            !worker.is_finished(),
            "the sync ended before reading a file"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::fs::rename(a.path("p/m"), a.path("q/m")).unwrap();
    std::fs::write(gate.join("go"), b"").unwrap();
    let run = worker.join().unwrap();
    whole(
        &run,
        "the scan found q/m at an inode it had found elsewhere",
    );
    for d in [&a, &b, &a, &b] {
        checked(d);
    }
    assert_eq!(b.read("q/m"), b"moves while the walk goes");
    assert!(!b.exists("p/m"));
    same_trees(&a, &b, "after the move during the walk");
}

/// A kill at any step of a sync that takes what changed alone, then syncs until both devices agree:
/// both trees as with no kill, and every later sync the oracle checks.
#[test]
fn a_kill_at_any_step_of_an_incremental_sync_loses_nothing() {
    #[derive(Clone, Copy, Debug)]
    enum Phase {
        Here,
        There,
        Both,
    }
    fn story(phase: Phase, kill: Option<usize>, log: Option<&Path>) -> (Picture, Picture) {
        let sb = Sandbox::fast("incr-sweep");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        everything(&a);
        a.synced();
        b.synced();
        a.synced();
        b.synced();
        match phase {
            Phase::Here => {
                a.write("d/e/f3", "edited");
                a.write("d/new", "new");
                std::fs::rename(a.path("fill"), a.path("d/fill")).unwrap();
                chmod(&a.path("f1"), 0o640);
            }
            Phase::There => {
                b.write("d/from-b", "b");
                b.write("f1", "edited on b");
                std::fs::remove_file(b.path("h2")).unwrap();
                std::fs::rename(b.path("d/e"), b.path("e")).unwrap();
                b.synced();
            }
            Phase::Both => {
                a.write("d/f2", "edited on a");
                b.write("d/f2", "edited on b");
                std::fs::rename(a.path("f1"), a.path("d/taken")).unwrap();
                b.write("d/taken", "made on b");
                b.synced();
            }
        }
        let step = kill.map(|n| n.to_string());
        let mut env = Vec::new();
        if let Some(n) = &step {
            env.push(("CAIRN_FP_CRASH", n.as_str()));
        }
        let path = log.map(|p| p.to_str().unwrap().to_string());
        if let Some(p) = &path {
            env.push(("CAIRN_FP_LOG", p.as_str()));
        }
        let run = a.sync_env(&env);
        assert!(run.ok() || run.was_killed(), "{phase:?}: {}", run.text());
        for _ in 0..3 {
            converge(&[&a, &b]);
        }
        (Picture::of(&a.root), Picture::of(&b.root))
    }
    let mut jobs = Vec::new();
    let mut references = Vec::new();
    for (i, phase) in [Phase::Here, Phase::There, Phase::Both]
        .into_iter()
        .enumerate()
    {
        let probe = Sandbox::fast("incr-sweep-probe");
        let log = probe.path("steps");
        references.push(story(phase, None, Some(&log)));
        let steps = std::fs::read_to_string(&log).unwrap();
        assert!(steps.contains("sync.incr.confirmed"), "{phase:?}: {steps}");
        assert!(!steps.contains("sync.incr.declined"), "{phase:?}: {steps}");
        for (n, step) in steps.lines().enumerate() {
            if !["scan.dir", "scan.file"].contains(&step) {
                jobs.push((
                    i,
                    phase,
                    n + 1,
                    format!("{phase:?}: step {} ({step})", n + 1),
                ));
            }
        }
    }
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(12);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                let Some((i, phase, n, context)) = jobs.get(at) else {
                    return;
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let (a, b) = story(*phase, Some(*n), None);
                    let ignore = Ignore {
                        owner: true,
                        all_mtime: true,
                        ..Ignore::default()
                    };
                    // Conflict copies are named by the day, the same in both stories.
                    assert_eq!(
                        references[*i].0.diff(&a, ignore),
                        Vec::<String>::new(),
                        "{context}"
                    );
                    assert_eq!(
                        references[*i].1.diff(&b, ignore),
                        Vec::<String>::new(),
                        "{context}"
                    );
                }));
                if let Err(e) = result {
                    let text = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    failures.lock().unwrap().push(format!("{context}: {text}"));
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
