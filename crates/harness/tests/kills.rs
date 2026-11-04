//! What a kill or a full disk must not change: where things end up. The matrix checks that nothing is
//! lost at every step; this checks that a device stopped partway through taking something on does not
//! read what it received as its own change, so that a later change from elsewhere still stands.

mod common;
use common::*;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

/// Enough that a few vanished entries do not look like a missing mount.
fn filler(a: &Device) {
    for i in 0..14 {
        a.write(&format!("fill/{i}"), format!("{i}"));
    }
}

#[track_caller]
fn killed_at(d: &Device, point: &str) {
    let run = d.sync_env(&[("CAIRN_FP_CRASH", point)]);
    assert!(run.was_killed(), "never reached {point}: {}", run.text());
}

/// A file, a folder and the like as they are in a tree, without what no two runs share.
#[track_caller]
fn same_tree(reference: &Picture, got: &Picture, context: &str) {
    let diff = reference.diff(
        got,
        Ignore {
            owner: true,
            all_mtime: true,
            ..Ignore::default()
        },
    );
    assert_eq!(diff, Vec::<String>::new(), "{context}");
}

// --- A received update is not this device's own edit ---

#[test]
fn an_update_received_and_killed_does_not_undo_a_revert_elsewhere() {
    for point in [
        "apply.replace.swapped",
        "apply.replace.renamed",
        "sync.applied",
    ] {
        let (_sb, _s, a, b) = pair("killed-update");
        filler(&a);
        a.write("x", "v1");
        a.synced();
        b.synced();
        a.write("x", "v2");
        a.synced();
        killed_at(&b, point);
        assert_eq!(b.read("x"), b"v2", "{point}");
        a.write("x", "v1");
        a.synced();
        let p = converge(&[&a, &b]);
        assert_eq!(a.read("x"), b"v1", "{point}: the revert was undone");
        assert_eq!(p.names().iter().filter(|n| n.contains('x')).count(), 1);
    }
}

#[test]
fn an_update_received_and_killed_conflicts_with_nothing_when_edited_on_elsewhere() {
    let (_sb, _s, a, b) = pair("killed-update-v3");
    filler(&a);
    a.write("x", "v1");
    a.synced();
    b.synced();
    a.write("x", "v2");
    a.synced();
    killed_at(&b, "apply.replace.renamed");
    a.write("x", "v3");
    a.synced();
    converge(&[&a, &b]);
    assert_eq!(a.read("x"), b"v3");
    assert_eq!(b.conflict_copies("", "x"), Vec::<String>::new());
}

#[test]
fn a_full_disk_partway_through_receiving_does_not_undo_a_revert_elsewhere() {
    let setup = |label: &str| {
        let (sb, s, a, b) = pair(label);
        filler(&a);
        a.write("one", "one-v1");
        a.write("two", "two-v1");
        a.synced();
        b.synced();
        a.write("one", "one-v2");
        a.write("two", "two-v2");
        a.synced();
        (sb, s, a, b)
    };

    // The second write of a run, by counting on one that goes through.
    let (sb, _s, _a, b) = setup("full-probe");
    let log = sb.path("steps");
    b.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
        .expect_ok();
    let steps = fs::read_to_string(&log).unwrap();
    let second = steps
        .lines()
        .enumerate()
        .filter(|(_, s)| *s == "apply.temp.write")
        .nth(1)
        .map(|(i, _)| i + 1)
        .expect("two files arrive");

    let (_sb, _s, a, b) = setup("full");
    let run = b.sync_env(&[("CAIRN_FP_ENOSPC", &second.to_string())]);
    assert!(!run.ok(), "the disk never filled: {}", run.text());
    let placed = ["one", "two"]
        .into_iter()
        .find(|n| b.read(n).ends_with(b"v2"))
        .expect("the first file was placed");
    a.write(placed, format!("{placed}-v1"));
    a.synced();
    converge(&[&a, &b]);
    assert_eq!(
        a.read(placed),
        format!("{placed}-v1").as_bytes(),
        "the revert was undone"
    );
    let other = if placed == "one" { "two" } else { "one" };
    assert_eq!(a.read(other), format!("{other}-v2").as_bytes());
    assert_eq!(b.read(other), format!("{other}-v2").as_bytes());
}

#[test]
fn metadata_received_and_killed_does_not_undo_a_chmod_elsewhere() {
    let (_sb, _s, a, b) = pair("killed-meta");
    filler(&a);
    a.write("x", "v1");
    chmod(&a.path("x"), 0o644);
    a.synced();
    b.synced();
    chmod(&a.path("x"), 0o600);
    a.synced();
    killed_at(&b, "apply.meta.done");
    chmod(&a.path("x"), 0o644);
    a.synced();
    let p = converge(&[&a, &b]);
    let x = &p.entries[b"x".as_slice()];
    assert_eq!(x.mode & 0o777, 0o644, "the chmod was undone");
}

#[test]
fn a_chmod_here_after_a_killed_update_still_goes_up() {
    let (_sb, _s, a, b) = pair("killed-then-chmod");
    filler(&a);
    a.write("x", "v1");
    chmod(&a.path("x"), 0o644);
    a.synced();
    b.synced();
    set_mtime(&a.path("x"), 1_000_000, 0);
    a.synced();
    killed_at(&b, "apply.meta.done");
    chmod(&b.path("x"), 0o640);
    converge(&[&a, &b]);
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(a.path("x")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o640, "the chmod here was lost");
}

// --- What arrived, and went elsewhere before this device had finished ---

#[test]
fn a_file_killed_while_arriving_stays_deleted_after_a_delete_elsewhere() {
    let (_sb, _s, a, b) = pair("killed-arrival");
    filler(&a);
    a.write("d/k", "k");
    a.synced();
    b.synced();
    a.write("d/x", "x");
    a.write("d/y", "y");
    a.synced();
    killed_at(&b, "apply.create.renamed");
    fs::remove_file(a.path("d/x")).unwrap();
    fs::remove_file(a.path("d/y")).unwrap();
    a.synced();
    converge(&[&a, &b]);
    assert!(!a.exists("d/x") && !a.exists("d/y"), "a file came back");
    assert!(!b.exists("d/x") && !b.exists("d/y"), "a file stayed");
}

#[test]
fn a_folder_killed_while_arriving_stays_deleted_after_a_delete_elsewhere() {
    let (_sb, _s, a, b) = pair("killed-folder");
    filler(&a);
    a.synced();
    b.synced();
    a.write("d/x", "x");
    a.synced();
    killed_at(&b, "apply.create.renamed");
    fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    converge(&[&a, &b]);
    assert!(!a.exists("d") && !b.exists("d"), "the folder came back");
}

#[test]
fn a_person_writing_at_a_name_that_was_on_its_way_keeps_both() {
    let (_sb, _s, a, b) = pair("killed-name-taken");
    filler(&a);
    a.synced();
    b.synced();
    a.write("x", "from a");
    a.synced();
    // Noted, and killed before it landed.
    killed_at(&b, "apply.noted");
    b.write("x", "from b");
    let p = converge(&[&a, &b]);
    has_version(&p, b"from a");
    has_version(&p, b"from b");
}

fn has_version(p: &Picture, want: &[u8]) {
    assert!(
        p.entries.values().any(|e| e.content == want),
        "{:?} is gone: {:?}",
        String::from_utf8_lossy(want),
        p.names()
    );
}

#[test]
fn a_rename_or_link_received_and_killed_does_not_undo_a_delete_elsewhere() {
    for point in ["apply.move.done", "apply.link.done", "sync.applied"] {
        let (_sb, _s, a, b) = pair("killed-move");
        filler(&a);
        a.write("f", "f");
        a.write("p", "p");
        a.write("d/c", "c");
        a.synced();
        b.synced();
        fs::rename(a.path("f"), a.path("g")).unwrap();
        fs::hard_link(a.path("p"), a.path("p2")).unwrap();
        fs::rename(a.path("d"), a.path("e")).unwrap();
        a.synced();
        killed_at(&b, point);
        for name in ["g", "p", "p2"] {
            fs::remove_file(a.path(name)).unwrap();
        }
        fs::remove_dir_all(a.path("e")).unwrap();
        a.synced();
        let p = converge(&[&a, &b]);
        let left: Vec<String> = p
            .names()
            .into_iter()
            .filter(|n| !n.is_empty() && !n.starts_with("fill"))
            .collect();
        assert_eq!(left, Vec::<String>::new(), "{point}: it came back");
    }
}

#[test]
fn a_move_here_after_a_kill_before_the_move_elsewhere_arrived_is_still_a_conflict() {
    let (_sb, _s, a, b) = pair("killed-both-moved");
    filler(&a);
    a.write("x", "x");
    a.synced();
    b.synced();
    fs::rename(a.path("x"), a.path("y")).unwrap();
    a.synced();
    killed_at(&b, "apply.noted");
    fs::rename(b.path("x"), b.path("z")).unwrap();
    converge(&[&a, &b]);
    assert!(a.exists("y"), "the server's move should stand");
    assert!(!a.exists("z"), "one file came out with both names");
    assert!(!a.exists("x"));
}

#[test]
fn a_restore_killed_while_arriving_leaves_no_duplicate() {
    let (_sb, _s, a, b) = pair("killed-restore");
    filler(&a);
    a.write("f", "base");
    a.write("d/g", "g");
    a.synced();
    b.synced();
    let seq = a.cairn(&["log"]);
    let seq = seq.stdout.lines().last().unwrap().split_whitespace().next();
    let seq = seq.unwrap().to_string();
    fs::remove_file(a.path("f")).unwrap();
    fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    b.synced();
    a.cairn(&["restore", "f", "--at", &seq]).expect_ok();
    a.cairn(&["restore", "d", "--at", &seq]).expect_ok();
    a.synced();
    killed_at(&b, "apply.create.renamed");
    let p = converge(&[&a, &b]);
    let names: Vec<String> = p
        .names()
        .into_iter()
        .filter(|n| !n.is_empty() && !n.starts_with("fill"))
        .collect();
    assert_eq!(names, ["d", "d/g", "f"]);
}

#[test]
fn a_kill_while_a_folder_arrives_makes_no_false_note() {
    let (_sb, _s, a, b) = pair("killed-note");
    filler(&a);
    a.synced();
    b.synced();
    a.write("d/f", "1");
    chmod(&a.path("d"), 0o775);
    a.synced();
    killed_at(&b, "apply.mkdir.done");
    let run = b.synced();
    assert!(!run.text().contains("kept"), "{}", run.text());
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[b"d".as_slice()].mode & 0o777, 0o775);
}

/// A folder is made private until the last pass. Killed again before that pass, on a run that had
/// already found it so and wrote into it, it is still a folder whose mode never arrived, not one this
/// filesystem kept at 0700, or no later run gives it its mode.
#[test]
fn a_folder_killed_twice_while_arriving_still_gets_its_mode() {
    let (_sb, _s, a, b) = pair("killed-twice-folder");
    filler(&a);
    a.synced();
    b.synced();
    a.write("d/f", "1");
    chmod(&a.path("d"), 0o775);
    a.synced();
    killed_at(&b, "apply.mkdir.done");
    // The folder's time moves with what this run puts in it.
    killed_at(&b, "apply.create.renamed");
    b.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[b"d".as_slice()].mode & 0o777, 0o775);
    assert_eq!(b.read("d/f"), b"1");
}

// --- A conflict copy and what comes after it ---

#[test]
fn a_conflict_on_a_hard_link_killed_keeps_every_name() {
    for point in [
        "apply.copy.given_up",
        "apply.copy.renamed",
        "apply.create.renamed",
    ] {
        let (_sb, _s, a, b) = pair("killed-copy");
        filler(&a);
        a.write("x", "base");
        fs::hard_link(a.path("x"), a.path("y")).unwrap();
        a.synced();
        b.synced();
        a.write("x", "a-edit");
        a.synced();
        b.write("x", "b-edit");
        killed_at(&b, point);
        let p = converge(&[&a, &b]);
        assert_eq!(a.read("x"), b"a-edit", "{point}");
        assert_eq!(a.read("y"), b"a-edit", "{point}: the second name went");
        has_version(&p, b"b-edit");
    }
}

// --- An edit in the moment before a swap, and then a kill ---

fn sync_with_a_gap(sb: &Sandbox, d: &Device, pause: &str, kill: &str, act: impl FnOnce()) -> Run {
    let gate = sb.path("gate");
    fs::create_dir(&gate).unwrap();
    let spec = format!("{pause}:{}", gate.display());
    let device = d.clone();
    let kill = kill.to_string();
    let worker = std::thread::spawn(move || {
        device.sync_env(&[("CAIRN_FP_PAUSE", &spec), ("CAIRN_FP_CRASH", &kill)])
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !gate.join("reached").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "never reached {pause}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    act();
    fs::write(gate.join("go"), b"").unwrap();
    let run = worker.join().unwrap();
    fs::remove_dir_all(&gate).unwrap();
    run
}

fn type_in_place(path: &std::path::Path) {
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(b" and typed in place").unwrap();
}

/// The way most editors save: a new file, renamed over the name.
fn save_as_an_editor_does(d: &Device, rel: &str, data: &str) {
    let tmp = d.path(".saving");
    fs::write(&tmp, data).unwrap();
    fs::rename(&tmp, d.path(rel)).unwrap();
}

#[track_caller]
fn sync_killed_with_edit_in_the_gap(sb: &Sandbox, d: &Device, pause: &str, kill: &str, edit: &str) {
    let path = d.path(edit);
    let run = sync_with_a_gap(sb, d, pause, kill, || type_in_place(&path));
    assert!(run.was_killed(), "{}", run.text());
}

#[test]
fn an_edit_caught_by_a_killed_swap_is_kept() {
    let (sb, _s, a, b) = pair("killed-swap");
    filler(&a);
    a.write("f", "base");
    a.synced();
    b.synced();
    a.write("f", "from a");
    a.synced();
    sync_killed_with_edit_in_the_gap(
        &sb,
        &b,
        "apply.replace.before_rename",
        "apply.replace.swapped",
        "f",
    );
    let p = converge(&[&a, &b]);
    assert_eq!(a.read("f"), b"from a");
    has_version(&p, b"base and typed in place");
    assert_eq!(b.conflict_copies("", "f").len(), 1, "{:?}", p.names());
    assert_eq!(leftover_temps(&b.root), Vec::<String>::new());
}

#[test]
fn an_edit_caught_by_a_killed_removal_is_kept() {
    let (sb, _s, a, b) = pair("killed-removal");
    filler(&a);
    a.write("f", "base");
    a.synced();
    b.synced();
    fs::remove_file(a.path("f")).unwrap();
    a.synced();
    sync_killed_with_edit_in_the_gap(
        &sb,
        &b,
        "apply.unlink.before_aside",
        "apply.unlink.aside",
        "f",
    );
    let run = b.synced();
    assert!(
        run.text().contains("kept under this name"),
        "{}",
        run.text()
    );
    let p = converge(&[&a, &b]);
    has_version(&p, b"base and typed in place");
    assert_eq!(leftover_temps(&b.root), Vec::<String>::new());
}

/// The old file is still there under its other name, with the edit in it: nothing needs saving.
#[test]
fn an_edit_through_another_name_after_a_killed_swap_makes_no_extra_copy() {
    let (_sb, _s, a, b) = pair("killed-swap-link");
    filler(&a);
    a.write("x", "base");
    fs::hard_link(a.path("x"), a.path("y")).unwrap();
    a.synced();
    b.synced();
    a.write("x", "from a");
    a.synced();
    killed_at(&b, "apply.replace.swapped");
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(b.path("y"))
        .unwrap();
    f.write_all(b" and typed").unwrap();
    drop(f);
    let p = converge(&[&a, &b]);
    has_version(&p, b"base and typed");
    assert_eq!(a.read("x"), b"from a");
    let names = p.names();
    assert_eq!(
        a.conflict_copies("", "x (conflict"),
        Vec::<String>::new(),
        "{names:?}"
    );
}

#[test]
fn a_swap_killed_with_no_edit_leaves_nothing_behind() {
    for point in [
        "apply.replace.aside",
        "apply.replace.swapped",
        "apply.unlink.aside",
    ] {
        let (_sb, _s, a, b) = pair("killed-swap-clean");
        filler(&a);
        a.write("f", "base");
        a.synced();
        b.synced();
        if point.starts_with("apply.replace") {
            a.write("f", "from a");
        } else {
            fs::remove_file(a.path("f")).unwrap();
        }
        a.synced();
        killed_at(&b, point);
        let run = b.synced();
        assert!(!run.text().contains("kept under"), "{}", run.text());
        let p = converge(&[&a, &b]);
        let names = p.names().into_iter().filter(|n| n.contains("conflict"));
        assert_eq!(names.count(), 0, "{point}");
        assert_eq!(leftover_temps(&b.root), Vec::<String>::new());
    }
}

#[test]
fn an_editor_save_caught_by_a_killed_swap_is_kept() {
    let (sb, _s, a, b) = pair("killed-swap-save");
    filler(&a);
    a.write("f", "base");
    a.synced();
    b.synced();
    a.write("f", "from a");
    a.synced();
    let run = sync_with_a_gap(
        &sb,
        &b,
        "apply.replace.before_rename",
        "apply.replace.swapped",
        || save_as_an_editor_does(&b, "f", "saved by an editor"),
    );
    assert!(run.was_killed(), "{}", run.text());
    let run = b.synced();
    assert!(
        run.text().contains("kept under this name"),
        "{}",
        run.text()
    );
    let p = converge(&[&a, &b]);
    assert_eq!(a.read("f"), b"from a");
    has_version(&p, b"saved by an editor");
    assert_eq!(b.conflict_copies("", "f").len(), 1, "{:?}", p.names());
    assert_eq!(leftover_temps(&b.root), Vec::<String>::new());
}

#[test]
fn an_editor_save_caught_by_a_killed_removal_is_kept() {
    let (sb, _s, a, b) = pair("killed-removal-save");
    filler(&a);
    a.write("f", "base");
    a.synced();
    b.synced();
    fs::remove_file(a.path("f")).unwrap();
    a.synced();
    let run = sync_with_a_gap(
        &sb,
        &b,
        "apply.unlink.before_aside",
        "apply.unlink.aside",
        || save_as_an_editor_does(&b, "f", "saved by an editor"),
    );
    assert!(run.was_killed(), "{}", run.text());
    let p = converge(&[&a, &b]);
    has_version(&p, b"saved by an editor");
    assert_eq!(b.conflict_copies("", "f").len(), 1, "{:?}", p.names());
    assert_eq!(leftover_temps(&b.root), Vec::<String>::new());
}

/// What was saved happens to be what is on its way to another name: still the only copy at this one.
#[test]
fn an_editor_save_equal_to_another_arrival_is_kept() {
    let (sb, _s, a, b) = pair("killed-swap-same");
    filler(&a);
    a.write("f", "base");
    a.synced();
    b.synced();
    a.write("f", "from a");
    a.write("h", "the same words");
    a.synced();
    let run = sync_with_a_gap(
        &sb,
        &b,
        "apply.replace.before_rename",
        "apply.replace.swapped",
        || save_as_an_editor_does(&b, "f", "the same words"),
    );
    assert!(run.was_killed(), "{}", run.text());
    let p = converge(&[&a, &b]);
    assert_eq!(b.conflict_copies("", "f").len(), 1, "{:?}", p.names());
}

/// A folder is nothing a temporary may hide, either.
#[test]
fn a_folder_caught_by_a_killed_removal_is_kept() {
    let (sb, _s, a, b) = pair("killed-removal-folder");
    filler(&a);
    a.write("f", "base");
    a.synced();
    b.synced();
    fs::remove_file(a.path("f")).unwrap();
    a.synced();
    let run = sync_with_a_gap(
        &sb,
        &b,
        "apply.unlink.before_aside",
        "apply.unlink.aside",
        || {
            fs::remove_file(b.path("f")).unwrap();
            b.write("f/inside", "a new folder's file");
        },
    );
    assert!(run.was_killed(), "{}", run.text());
    let p = converge(&[&a, &b]);
    has_version(&p, b"a new folder's file");
    assert_eq!(leftover_temps(&b.root), Vec::<String>::new());
}

/// The same when the name is a hard link's, followed after the first name was swapped.
#[test]
fn an_editor_save_on_another_name_caught_by_a_killed_swap_is_kept() {
    let (sb, _s, a, b) = pair("killed-relink-save");
    filler(&a);
    a.write("f", "base");
    fs::hard_link(a.path("f"), a.path("g")).unwrap();
    a.synced();
    b.synced();
    a.write("f", "from a");
    a.synced();
    let run = sync_with_a_gap(
        &sb,
        &b,
        "apply.replace.relink",
        "apply.replace.relink.swapped",
        || save_as_an_editor_does(&b, "g", "saved by an editor"),
    );
    assert!(run.was_killed(), "{}", run.text());
    let p = converge(&[&a, &b]);
    has_version(&p, b"saved by an editor");
    has_version(&p, b"from a");
    assert_eq!(leftover_temps(&b.root), Vec::<String>::new());
}

#[test]
fn an_edit_while_an_interrupted_sync_is_finished_is_kept() {
    for (kill, how) in [
        ("never", "typed"),
        ("recover.relink.swapped", "typed"),
        ("recover.relink.swapped", "saved"),
    ] {
        let (sb, _s, a, b) = pair("killed-relink-recovery");
        filler(&a);
        a.write("f", "base");
        fs::hard_link(a.path("f"), a.path("g")).unwrap();
        a.synced();
        b.synced();
        a.write("f", "from a");
        a.synced();
        killed_at(&b, "apply.replace.relink");
        let run = sync_with_a_gap(&sb, &b, "recover.relink.linked", kill, || match how {
            "typed" => type_in_place(&b.path("g")),
            _ => save_as_an_editor_does(&b, "g", "base and typed in place"),
        });
        assert_eq!(run.was_killed(), kill != "never", "{}", run.text());
        let p = converge(&[&a, &b]);
        has_version(&p, b"base and typed in place");
        has_version(&p, b"from a");
        assert_eq!(
            leftover_temps(&b.root),
            Vec::<String>::new(),
            "{kill} {how}"
        );
    }
}

/// The record goes first: a second kill finds the file as what it now is, and does not take it for the
/// object it was.
#[test]
fn a_kill_while_keeping_an_edit_keeps_one_copy() {
    for point in ["clean.rescue.noted", "clean.rescued"] {
        let (sb, _s, a, b) = pair("killed-rescue");
        filler(&a);
        a.write("f", "base");
        a.synced();
        b.synced();
        a.write("f", "from a");
        a.synced();
        sync_killed_with_edit_in_the_gap(
            &sb,
            &b,
            "apply.replace.before_rename",
            "apply.replace.swapped",
            "f",
        );
        killed_at(&b, point);
        let p = converge(&[&a, &b]);
        assert_eq!(a.read("f"), b"from a", "{point}");
        has_version(&p, b"base and typed in place");
        assert_eq!(
            b.conflict_copies("", "f").len(),
            1,
            "{point}: {:?}",
            p.names()
        );
        assert_eq!(leftover_temps(&b.root), Vec::<String>::new());
    }
}

/// A swap goes through a staging folder: stopped there, it goes on to where it was going, and is no
/// move of this device's to be reported against the server's.
#[test]
fn a_swap_killed_halfway_finishes_with_nothing_to_report() {
    let setup = |label: &str| {
        let (sb, s, a, b) = pair(label);
        filler(&a);
        a.write("x", "was x");
        a.write("y", "was y");
        a.synced();
        b.synced();
        fs::rename(a.path("x"), a.path("t")).unwrap();
        fs::rename(a.path("y"), a.path("x")).unwrap();
        fs::rename(a.path("t"), a.path("y")).unwrap();
        a.synced();
        (sb, s, a, b)
    };
    let (sb, _s, _a, b) = setup("killed-staged-probe");
    let log = sb.path("steps");
    b.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
        .expect_ok();
    let steps = fs::read_to_string(&log).unwrap();
    let moves: Vec<usize> = steps
        .lines()
        .enumerate()
        .filter(|(_, s)| *s == "apply.move.done")
        .map(|(i, _)| i + 1)
        .collect();
    assert_eq!(moves.len(), 3, "not by way of a staging folder");
    for step in moves {
        let (_sb, _s, a, b) = setup("killed-staged");
        let run = b.sync_env(&[("CAIRN_FP_CRASH", &step.to_string())]);
        assert!(run.was_killed(), "{}", run.text());
        let run = b.synced();
        for word in ["recovered", "moved on"] {
            assert!(!run.text().contains(word), "step {step}: {}", run.text());
        }
        let p = converge(&[&a, &b]);
        assert_eq!(a.read("x"), b"was y", "{:?}", p.names());
        assert_eq!(a.read("y"), b"was x", "{:?}", p.names());
    }
}

// --- Names and fields that are this device's own ---

/// The new content lands at the name this device gave the file, which the server never heard of.
#[test]
fn a_restore_elsewhere_stands_after_an_update_killed_on_a_name_renamed_here() {
    let (_sb, _s, a, b) = pair("killed-renamed-here");
    filler(&a);
    a.write("x", "v1");
    a.synced();
    b.synced();
    let seq = a.cairn(&["log"]);
    let seq = seq.stdout.lines().last().unwrap().split_whitespace().next();
    let seq = seq.unwrap().to_string();
    a.write("x", "v2 from a");
    a.synced();
    fs::rename(b.path("x"), b.path("w")).unwrap();
    killed_at(&b, "apply.replace.renamed");
    assert_eq!(b.read("w"), b"v2 from a");
    a.cairn(&["restore", "x", "--at", &seq]).expect_ok();
    a.synced();
    let p = converge(&[&a, &b]);
    assert_eq!(a.read("w"), b"v1", "{:?}", p.names());
    assert!(!a.exists("x"), "{:?}", p.names());
}

/// Whatever else is moving in the tree, what was seen of this file was seen.
#[test]
fn a_revert_elsewhere_stands_while_the_tree_keeps_moving() {
    let (_sb, _s, a, b) = pair("killed-churn");
    filler(&a);
    a.write("x", "v1");
    a.write("busy/keep", "k");
    a.synced();
    b.synced();
    a.write("x", "v2 from a");
    a.synced();
    killed_at(&b, "apply.replace.renamed");
    a.write("x", "v1");
    a.synced();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn = {
        let (stop, busy) = (stop.clone(), b.path("busy"));
        std::thread::spawn(move || {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let _ = fs::write(busy.join(format!("f{}", i % 5)), b"");
                let _ = fs::remove_file(busy.join(format!("f{}", (i + 2) % 5)));
                i += 1;
            }
        })
    };
    b.sync();
    stop.store(true, Ordering::Relaxed);
    churn.join().unwrap();
    for i in 0..5 {
        let _ = fs::remove_file(b.path(&format!("busy/f{i}")));
    }
    converge(&[&a, &b]);
    assert_eq!(a.read("x"), b"v1", "the revert was undone");
}

/// New content came with its metadata, and a person then changed that: the same as with no kill.
#[test]
fn metadata_changed_after_new_content_was_killed_goes_up() {
    for what in ["mode", "xattr", "mtime", "new file"] {
        let (_sb, _s, a, b) = pair("killed-then-metadata");
        filler(&a);
        if what != "new file" {
            a.write("x", "v1");
            chmod(&a.path("x"), 0o644);
        }
        a.synced();
        b.synced();
        a.write("x", "v2");
        chmod(&a.path("x"), 0o644);
        a.synced();
        killed_at(
            &b,
            if what == "new file" {
                "apply.create.renamed"
            } else {
                "apply.replace.renamed"
            },
        );
        match what {
            "xattr" => set_xattr(&b.path("x"), "user.mine", b"here"),
            "mtime" => set_mtime(&b.path("x"), 981_000_000, 0),
            _ => chmod(&b.path("x"), 0o600),
        }
        let p = converge(&[&a, &b]);
        let x = &p.entries[b"x".as_slice()];
        match what {
            "xattr" => assert_eq!(x.xattrs[b"user.mine".as_slice()], b"here"),
            "mtime" => assert_eq!(x.mtime, (981_000_000, 0)),
            _ => assert_eq!(x.mode & 0o777, 0o600, "{what}"),
        }
    }
}

/// What a filesystem or a device does not hold differs from the record after new content lands, and is
/// nobody's change. Going by what differs would take it off everyone else's disk.
#[test]
fn what_this_device_does_not_hold_is_no_change_after_a_kill() {
    use std::os::unix::fs::PermissionsExt;
    let commits = |d: &Device| d.cairn(&["log"]).stdout.lines().count();
    for point in ["apply.replace.renamed", "apply.create.renamed"] {
        let sb = Sandbox::on_disk("killed-no-mode");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::with_flags(&sb, &server, "b", &["--no-mode"]);
        filler(&a);
        if point == "apply.replace.renamed" {
            a.write("x", "v1");
        }
        a.synced();
        b.synced();
        a.write("x", "v2");
        chmod(&a.path("x"), 0o640);
        a.synced();
        let before = commits(&a);
        killed_at(&b, point);
        for d in [&b, &a, &b] {
            d.synced();
        }
        let mode = fs::metadata(a.path("x")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640, "{point}: taken off the other device");
        assert_eq!(commits(&a), before, "{point}: a commit nobody made");
    }
}

/// The same with what a filesystem cannot take: a big xattr or a time past ext4's end. Where the disk
/// under test can take them this shows nothing, and passes.
#[test]
fn what_this_filesystem_does_not_hold_is_no_change_after_a_kill() {
    let shm = std::path::Path::new("/dev/shm");
    if !shm.is_dir() {
        return;
    }
    let big = vec![b'v'; 20_000];
    let far = (32_503_680_000, 0);
    for point in ["apply.replace.renamed", "apply.create.renamed"] {
        let there = Sandbox::under(shm, "killed-holds-a");
        let sb = Sandbox::on_disk("killed-holds-b");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&there, &server, "a");
        let b = Device::new(&sb, &server, "b");
        filler(&a);
        if point == "apply.replace.renamed" {
            a.write("x", "v1");
            set_xattr(&a.path("x"), "user.k", b"small");
        }
        a.synced();
        b.synced();
        a.write("x", "v2");
        set_xattr(&a.path("x"), "user.k", &big);
        set_mtime(&a.path("x"), far.0, far.1);
        a.synced();
        killed_at(&b, point);
        for d in [&b, &a, &b] {
            d.sync();
        }
        let p = Picture::of(&a.root);
        let x = &p.entries[b"x".as_slice()];
        assert_eq!(x.xattrs[b"user.k".as_slice()], big, "{point}: xattr");
        assert_eq!(x.mtime, far, "{point}: time");
    }
}

/// Two temporaries on one inode, the first in a folder the scan cannot remove from: when the second is
/// gone, the first is the only copy, and is looked at again before it goes the same way.
#[test]
fn an_edit_on_two_temporaries_one_read_only_is_kept() {
    if is_root() {
        return;
    }
    for _ in 0..20 {
        let (sb, _s, a, b) = pair("killed-two-temps");
        filler(&a);
        a.write("a_ro/f", "base");
        a.write("z_w/.keep", "");
        fs::hard_link(a.path("a_ro/f"), a.path("z_w/g")).unwrap();
        chmod(&a.path("a_ro"), 0o555);
        a.synced();
        b.synced();
        chmod(&a.path("a_ro"), 0o755);
        a.write("a_ro/f", "from a");
        chmod(&a.path("a_ro"), 0o555);
        a.synced();
        killed_at(&b, "apply.replace.swapped");
        // Which name goes first follows the random object ids.
        let first_was_read_only = !leftover_temps(&b.path("a_ro")).is_empty();
        if !first_was_read_only {
            chmod(&a.path("a_ro"), 0o755);
            chmod(&b.path("a_ro"), 0o755);
            continue;
        }
        let run = sync_with_a_gap(
            &sb,
            &b,
            "recover.relink.linked",
            "recover.relink.swapped",
            || type_in_place(&b.path("z_w/g")),
        );
        assert!(run.was_killed(), "{}", run.text());
        b.synced();
        let p = converge(&[&a, &b]);
        has_version(&p, b"base and typed in place");
        chmod(&a.path("a_ro"), 0o755);
        chmod(&b.path("a_ro"), 0o755);
        return;
    }
    panic!("the read-only name never went first");
}

/// Rescued while something else moves in the tree, which makes the scan run again.
#[test]
fn an_edit_kept_while_the_tree_keeps_moving_is_one_copy() {
    let (sb, _s, a, b) = pair("killed-rescue-churn");
    filler(&a);
    a.write("f", "base");
    a.write("busy/keep", "k");
    a.synced();
    b.synced();
    a.write("f", "from a");
    a.synced();
    sync_killed_with_edit_in_the_gap(
        &sb,
        &b,
        "apply.replace.before_rename",
        "apply.replace.swapped",
        "f",
    );
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn = {
        let (stop, busy) = (stop.clone(), b.path("busy"));
        std::thread::spawn(move || {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let _ = fs::write(busy.join(format!("f{}", i % 5)), b"");
                let _ = fs::remove_file(busy.join(format!("f{}", (i + 2) % 5)));
                i += 1;
            }
        })
    };
    b.sync();
    stop.store(true, Ordering::Relaxed);
    churn.join().unwrap();
    for i in 0..5 {
        let _ = fs::remove_file(b.path(&format!("busy/f{i}")));
    }
    let p = converge(&[&a, &b]);
    assert_eq!(a.read("f"), b"from a");
    has_version(&p, b"base and typed in place");
    assert_eq!(b.conflict_copies("", "f").len(), 1, "{:?}", p.names());
}

// --- A folder's own timestamp ---

fn folder_with_a_set_time(label: &str) -> (Sandbox, Server, Device, Device) {
    let (sb, s, a, b) = pair(label);
    filler(&a);
    a.write("d/x", "v1");
    set_mtime(&a.path("d"), 981_000_000, 0);
    set_mtime(&a.root, 981_000_000, 0);
    a.synced();
    b.synced();
    // In place, so that the folder's time does not move here.
    a.write("d/x", "v2");
    set_mtime(&a.path("d"), 981_000_000, 0);
    set_mtime(&a.root, 981_000_000, 0);
    a.synced();
    (sb, s, a, b)
}

#[test]
fn a_kill_or_a_full_disk_moves_no_folders_time_for_anyone() {
    let commits = |d: &Device| d.cairn(&["log"]).stdout.lines().count();
    let (sb, _s, a, b) = folder_with_a_set_time("killed-folder-time");
    let before = commits(&a);
    killed_at(&b, "apply.replace.renamed");
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[b"d".as_slice()].mtime, (981_000_000, 0));
    assert_eq!(
        p.entries[b"".as_slice()].mtime,
        (981_000_000, 0),
        "the root"
    );
    assert_eq!(a.read("d/x"), b"v2");
    assert_eq!(commits(&a), before, "a commit nobody made");
    drop(sb);

    let (sb, _s, _a, b) = folder_with_a_set_time("killed-folder-time-probe");
    let log = sb.path("steps");
    b.sync_env(&[("CAIRN_FP_LOG", log.to_str().unwrap())])
        .expect_ok();
    let steps = fs::read_to_string(&log).unwrap();
    let write = 1 + steps
        .lines()
        .position(|s| s == "apply.temp.write")
        .expect("something arrives");

    let (_sb, _s, a, b) = folder_with_a_set_time("killed-folder-time-full");
    let before = commits(&a);
    let run = b.sync_env(&[("CAIRN_FP_ENOSPC", &write.to_string())]);
    assert!(!run.ok(), "the disk never filled: {}", run.text());
    let p = converge(&[&a, &b]);
    assert_eq!(
        p.entries[b"d".as_slice()].mtime,
        (981_000_000, 0),
        "full disk"
    );
    assert_eq!(a.read("d/x"), b"v2");
    assert_eq!(commits(&a), before, "a commit nobody made");
}

/// A time changed here before the sync was this device's own, and stays a change.
#[test]
fn a_folders_time_changed_here_before_a_kill_still_goes_up() {
    let (_sb, _s, a, b) = folder_with_a_set_time("killed-folder-time-here");
    set_mtime(&b.path("d"), 1_000_000_000, 0);
    killed_at(&b, "apply.replace.renamed");
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[b"d".as_slice()].mtime, (1_000_000_000, 0));
}

/// A removal moves a folder's time like anything else, and `rsync -a` puts it back on the other side.
#[test]
fn a_kill_during_a_removal_moves_no_folders_time_for_anyone() {
    let (_sb, _s, a, b) = folder_with_a_set_time("killed-folder-time-removal");
    b.synced();
    let commits = a.cairn(&["log"]).stdout.lines().count();
    fs::remove_file(a.path("d/x")).unwrap();
    set_mtime(&a.path("d"), 981_000_000, 0);
    set_mtime(&a.root, 981_000_000, 0);
    a.synced();
    killed_at(&b, "apply.unlink.done");
    let p = converge(&[&a, &b]);
    assert_eq!(p.entries[b"d".as_slice()].mtime, (981_000_000, 0));
    let log = a.cairn(&["log"]);
    assert_eq!(log.stdout.lines().count(), commits + 1, "{}", log.stdout);
}

// --- The sweep ---

/// What happens around a device's sync, which is killed at each of its steps in turn.
struct Scenario {
    name: &'static str,
    /// In place on both devices before anything else.
    base: fn(&Device, &Device),
    /// On the first device, before the second one's sync.
    elsewhere: fn(&Device),
    /// On the second device, not yet synced.
    here: fn(&Device),
    /// On the first device, once the second one has stopped.
    after: fn(&Device),
}

fn write_many(d: &Device, files: &[(&str, &str)]) {
    for (name, data) in files {
        d.write(name, data);
    }
}

fn nothing(_: &Device) {}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "update then revert",
        base: |a, _| write_many(a, &[("x", "v1"), ("k", "k")]),
        elsewhere: |a| a.write("x", "v2"),
        here: nothing,
        after: |a| a.write("x", "v1"),
    },
    Scenario {
        name: "metadata then back",
        base: |a, _| {
            a.write("x", "v1");
            chmod(&a.path("x"), 0o644);
        },
        elsewhere: |a| chmod(&a.path("x"), 0o600),
        here: nothing,
        after: |a| chmod(&a.path("x"), 0o644),
    },
    Scenario {
        name: "metadata arrives",
        base: |a, _| {
            a.write("x", "v1");
            chmod(&a.path("x"), 0o644);
        },
        elsewhere: |a| chmod(&a.path("x"), 0o600),
        here: nothing,
        after: nothing,
    },
    Scenario {
        name: "update, with a chmod here, then revert",
        base: |a, _| {
            a.write("x", "v1");
            chmod(&a.path("x"), 0o644);
        },
        elsewhere: |a| a.write("x", "v2"),
        here: |b| chmod(&b.path("x"), 0o600),
        after: |a| a.write("x", "v1"),
    },
    Scenario {
        name: "arrival then delete",
        base: |a, _| a.write("d/k", "k"),
        elsewhere: |a| write_many(a, &[("d/x", "x"), ("e/y", "y")]),
        here: nothing,
        after: |a| {
            fs::remove_file(a.path("d/x")).unwrap();
            fs::remove_dir_all(a.path("e")).unwrap();
        },
    },
    Scenario {
        name: "move then delete",
        base: |a, _| write_many(a, &[("x", "x"), ("d/g", "g"), ("k", "k")]),
        elsewhere: |a| {
            fs::rename(a.path("x"), a.path("y")).unwrap();
            fs::rename(a.path("d"), a.path("d2")).unwrap();
        },
        here: nothing,
        after: |a| {
            fs::remove_file(a.path("y")).unwrap();
            fs::remove_dir_all(a.path("d2")).unwrap();
        },
    },
    Scenario {
        name: "swap then edits",
        base: |a, _| write_many(a, &[("u", "u"), ("v", "v"), ("k", "k")]),
        elsewhere: |a| {
            fs::rename(a.path("u"), a.path("t")).unwrap();
            fs::rename(a.path("v"), a.path("u")).unwrap();
            fs::rename(a.path("t"), a.path("v")).unwrap();
        },
        here: nothing,
        after: |a| {
            fs::remove_file(a.path("u")).unwrap();
            a.write("v", "v-a");
        },
    },
    Scenario {
        name: "links updated then a name gone",
        base: |a, _| {
            write_many(a, &[("x", "v1"), ("k", "k")]);
            fs::hard_link(a.path("x"), a.path("y")).unwrap();
            fs::hard_link(a.path("x"), a.path("z")).unwrap();
        },
        elsewhere: |a| a.write("x", "v2"),
        here: nothing,
        after: |a| fs::remove_file(a.path("y")).unwrap(),
    },
    Scenario {
        name: "move and edit then edited again",
        base: |a, _| write_many(a, &[("x", "v1"), ("k", "k")]),
        elsewhere: |a| {
            a.write("x", "v2");
            fs::rename(a.path("x"), a.path("y")).unwrap();
        },
        here: nothing,
        after: |a| a.write("y", "v3"),
    },
    Scenario {
        name: "conflict on a hard link",
        base: |a, _| {
            write_many(a, &[("x", "base"), ("k", "k")]);
            fs::hard_link(a.path("x"), a.path("y")).unwrap();
        },
        elsewhere: |a| a.write("x", "a-edit"),
        here: |b| b.write("x", "b-edit"),
        after: nothing,
    },
    Scenario {
        name: "moved out of view while edited here",
        base: |a, b| {
            b.write(".cairn/ignore", "attic/\n");
            write_many(
                a,
                &[("f", "f"), ("d/g", "g"), ("d/h", "h"), ("attic/x", "x")],
            );
        },
        elsewhere: |a| {
            fs::rename(a.path("f"), a.path("attic/f")).unwrap();
            fs::rename(a.path("d"), a.path("attic/d")).unwrap();
        },
        here: |b| {
            b.write("f", "f-edit");
            b.write("d/g", "g-edit");
        },
        after: nothing,
    },
    Scenario {
        name: "conflict then edited again",
        base: |a, _| write_many(a, &[("x", "v1"), ("k", "k")]),
        elsewhere: |a| a.write("x", "v2-a"),
        here: |b| b.write("x", "v2-b"),
        after: |a| a.write("x", "v3-a"),
    },
];

/// Sync in turn until nothing moves. The trees need not match: what one leaves out by a rule, it leaves out.
#[track_caller]
fn settle(devices: &[&Device]) {
    for _ in 0..12 {
        let mut quiet = true;
        for d in devices {
            let run = d.cairn(&["sync", "--allow-mass-delete"]);
            assert!(run.ok(), "device {}: {}", d.name, run.text());
            quiet &= run.stdout.contains(" 0 steps applied here, 0 commits");
        }
        if quiet {
            return;
        }
    }
    panic!("the devices did not settle in 12 passes");
}

struct Ended {
    a: Picture,
    b: Picture,
}

/// The whole story, the second device's sync logged or killed at step `kill`.
fn story(s: &Scenario, kill: Option<usize>, log: Option<&std::path::Path>) -> Ended {
    let sb = Sandbox::fast("sweep");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (s.base)(&a, &b);
    filler(&a);
    a.synced();
    b.synced();
    (s.elsewhere)(&a);
    a.synced();
    (s.here)(&b);
    let step = kill.map(|n| n.to_string());
    let mut env = Vec::new();
    if let Some(n) = &step {
        env.push(("CAIRN_FP_CRASH", n.as_str()));
    }
    let log_path = log.map(|p| p.to_str().unwrap().to_string());
    if let Some(p) = &log_path {
        env.push(("CAIRN_FP_LOG", p.as_str()));
    }
    let run = b.sync_env(&env);
    assert!(run.ok() || run.was_killed(), "{}", run.text());
    (s.after)(&a);
    a.synced();
    settle(&[&b, &a]);
    Ended {
        a: Picture::of(&a.root),
        b: Picture::of(&b.root),
    }
}

/// Steps that say nothing a neighbouring step does not, and that there are many of.
const SAME_AS_A_NEIGHBOUR: &[&str] = &[
    "scan.dir",
    "scan.file",
    "apply.temp.created",
    "apply.temp.written",
    "apply.temp.synced",
    "apply.temp.meta",
    "apply.temp.write",
];

/// Comparing where things end up finds what the per-step matrix cannot: nothing lost, yet the wrong
/// version current, a name gone everywhere, or something back that was deleted.
#[test]
fn a_kill_at_any_step_does_not_change_where_things_end_up() {
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut jobs: Vec<(usize, usize, String)> = Vec::new();
    let mut references = Vec::new();
    for (i, s) in SCENARIOS.iter().enumerate() {
        let probe = Sandbox::fast("sweep-probe");
        let log = probe.path("steps");
        references.push(story(s, None, Some(&log)));
        let steps = fs::read_to_string(&log).unwrap();
        for (n, step) in steps.lines().enumerate() {
            if !SAME_AS_A_NEIGHBOUR.contains(&step) {
                jobs.push((i, n + 1, format!("{}: step {} ({step})", s.name, n + 1)));
            }
        }
    }
    eprintln!("{} kills across {} scenarios", jobs.len(), SCENARIOS.len());
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(12);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                let Some((i, n, context)) = jobs.get(at) else {
                    return;
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let got = story(&SCENARIOS[*i], Some(*n), None);
                    same_tree(
                        &references[*i].a,
                        &got.a,
                        &format!("{context}, first device"),
                    );
                    same_tree(
                        &references[*i].b,
                        &got.b,
                        &format!("{context}, second device"),
                    );
                }));
                if let Err(e) = result {
                    let message = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|m| (*m).to_string()))
                        .unwrap_or_default();
                    failures.lock().unwrap().push(message);
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(
        failures.is_empty(),
        "{} of {} kills changed the outcome; first: {}\nall: {:#?}",
        failures.len(),
        jobs.len(),
        failures[0],
        failures
            .iter()
            .map(|f| f.lines().next().unwrap_or(""))
            .collect::<Vec<_>>()
    );
}
