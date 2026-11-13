//! Ignore rules and selective sync against real binaries. What can go wrong here is one thing above all:
//! an entry a device is not looking at reading as an entry someone deleted.

mod common;
use common::*;
use std::collections::BTreeSet;

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

fn rules(d: &Device, text: &str) {
    std::fs::write(d.path(".cairn/ignore"), text).unwrap();
}

/// What a device without rules holds, after syncing: the server's tree, as near as a test can see it.
fn on_server(sb: &Sandbox, server: &Server, label: &str) -> BTreeSet<String> {
    let probe = Device::new(sb, server, label);
    probe.synced();
    Picture::of(&probe.root)
        .names()
        .into_iter()
        .filter(|n| !n.is_empty())
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn what_a_rule_covers_never_reaches_the_server() {
    let (sb, server, a, _b) = pair("sel-never");
    rules(&a, "*.log\nbuild/\n/secret\n");
    a.write("keep.txt", "k");
    a.write("run.log", "l");
    a.write("sub/deep.log", "l");
    a.write("build/out", "o");
    a.write("sub/build/out", "o");
    a.write("secret", "s");
    a.write("sub/secret", "not anchored here");
    let run = a.synced();
    assert!(
        run.stdout.contains("by this device's choice"),
        "{}",
        run.text()
    );

    assert_eq!(
        on_server(&sb, &server, "probe"),
        set(&["keep.txt", "sub", "sub/secret"])
    );
    // And nothing here was touched.
    for path in ["run.log", "sub/deep.log", "build/out", "secret"] {
        assert!(a.exists(path), "{path}");
    }
    let status = a.cairn(&["status"]).expect_ok();
    assert!(status.stdout.contains("run.log"), "{}", status.text());
    assert!(status.stdout.contains("build"), "{}", status.text());
}

/// No built-in names, ever: with no rules, the names people expect to be ignored all sync.
#[test]
fn with_no_rules_nothing_is_left_out_for_its_name() {
    let (_sb, _s, a, b) = pair("sel-none");
    let names = [
        ".git/config",
        "node_modules/x/index.js",
        ".venv/bin/python",
        ".DS_Store",
        "Thumbs.db",
        "a.swp",
        "b~",
        "desktop.ini",
        ".directory",
        ".gitignore",
        ".cairnignore",
    ];
    for name in names {
        a.write(name, name);
    }
    // Even a project's own ignore files mean nothing here.
    a.write(".gitignore", "*\n");
    a.write(".cairnignore", "*\n");
    a.synced();
    b.synced();
    for name in names {
        assert!(b.exists(name), "{name}");
    }
}

#[test]
fn a_rule_added_later_deletes_nothing_on_either_side() {
    let (sb, server, a, b) = pair("sel-later");
    a.write("data/one.log", "one");
    a.write("data/two.txt", "two");
    a.synced();
    b.synced();

    rules(&a, "*.log\n");
    a.synced();
    b.synced();
    assert_eq!(a.read("data/one.log"), b"one");
    assert_eq!(b.read("data/one.log"), b"one");

    // From here on, changes there and here pass each other by.
    a.write("data/one.log", "changed on a");
    b.write("data/one.log", "changed on b");
    a.synced();
    b.synced();
    a.synced();
    assert_eq!(a.read("data/one.log"), b"changed on a");
    assert_eq!(b.read("data/one.log"), b"changed on b");

    // Deleting a's copy means nothing to the server.
    std::fs::remove_file(a.path("data/one.log")).unwrap();
    a.synced();
    b.synced();
    assert_eq!(b.read("data/one.log"), b"changed on b");
    assert!(on_server(&sb, &server, "probe").contains("data/one.log"));
}

/// The one this design exists for: `(base, nothing here, server)` is what a delete looks like.
#[test]
fn lifting_a_rule_never_reads_as_deleting_what_was_not_here() {
    let (sb, server, a, b) = pair("sel-lift");
    rules(&a, "big/\n");
    b.write("big/file", "from b");
    b.write("big/sub/file", "from b too");
    b.write("other", "x");
    b.synced();
    a.synced();
    assert!(!a.exists("big"), "it was excluded and should not have come");
    assert!(a.exists("other"));
    // The base has moved past it here, so that it is in the base and not on disk.
    a.synced();

    rules(&a, "");
    let run = a.synced();
    assert_eq!(a.read("big/file"), b"from b", "{}", run.text());
    assert_eq!(a.read("big/sub/file"), b"from b too");
    b.synced();
    assert_eq!(b.read("big/file"), b"from b");
    assert!(on_server(&sb, &server, "probe").contains("big/sub/file"));
}

#[test]
fn lifting_a_rule_merges_what_happened_here_meanwhile_as_new() {
    let (_sb, _s, a, b) = pair("sel-lift2");
    a.write("same", "same");
    a.write("edited", "old");
    a.write("deleted", "deleted");
    a.synced();
    b.synced();

    rules(&a, "same\nedited\ndeleted\nextra\n");
    a.synced();
    a.write("edited", "edited while ignored");
    a.write("extra", "only here");
    std::fs::remove_file(a.path("deleted")).unwrap();
    a.synced();

    rules(&a, "");
    let run = a.synced();
    // Unchanged: nothing to do. Deleted while no one was looking: it comes back. New: it goes up.
    assert_eq!(a.read("same"), b"same", "{}", run.text());
    assert_eq!(a.read("deleted"), b"deleted");
    // Edited: the server's stands, and this one is kept beside it.
    assert_eq!(a.read("edited"), b"old");
    let copies = a.conflict_copies("", "edited");
    assert_eq!(copies.len(), 1, "{:?}", Picture::of(&a.root).names());

    b.synced();
    assert_eq!(b.read("same"), b"same");
    assert_eq!(b.read("deleted"), b"deleted");
    assert_eq!(b.read("extra"), b"only here");
    assert_eq!(b.read(&copies[0]), b"edited while ignored");
}

/// The record goes as soon as the rule is seen, not at the end of a sync that may never finish: a delete
/// here while no one was looking must not surface as a delete when the rule is lifted.
#[test]
fn a_rule_lifted_after_a_sync_that_never_finished_still_deletes_nothing() {
    let (sb, server, a, b) = pair("sel-unfinished");
    a.write("one.log", "one");
    a.write("plain", "p");
    a.synced();
    b.synced();

    rules(&a, "*.log\n");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.finalize.begin")]);
    assert!(run.was_killed(), "{}", run.text());
    std::fs::remove_file(a.path("one.log")).unwrap();
    rules(&a, "");
    let run = a.synced();
    assert_eq!(a.read("one.log"), b"one", "{}", run.text());
    b.synced();
    assert_eq!(b.read("one.log"), b"one");
    assert!(on_server(&sb, &server, "probe").contains("one.log"));
}

/// The same for a folder excluded by identity.
#[test]
fn an_include_after_an_exclude_that_never_finished_still_deletes_nothing() {
    let (_sb, _s, a, b) = pair("sel-unfinished2");
    b.write("photos/one", "1");
    b.write("plain", "p");
    b.synced();
    a.synced();

    let run = a.cairn_env(
        &["exclude", "photos"],
        &[("CAIRN_FP_CRASH", "selection.excluded")],
    );
    assert!(run.was_killed(), "{}", run.text());
    std::fs::remove_dir_all(a.path("photos")).unwrap();
    let run = a.cairn(&["include", "photos"]).expect_ok();
    assert_eq!(a.read("photos/one"), b"1", "{}", run.text());
    b.synced();
    assert_eq!(b.read("photos/one"), b"1");
}

/// The same for one name of a hard link, which has a record that stays.
#[test]
fn a_rule_lifted_after_an_unfinished_sync_keeps_the_hidden_name_of_a_hard_link() {
    let (_sb, _s, a, b) = pair("sel-unfinished-hl");
    b.write("outside", "v1");
    std::fs::create_dir(b.path("d")).unwrap();
    std::fs::hard_link(b.path("outside"), b.path("d/a")).unwrap();
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    rules(&a, "d/\n");
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.finalize.begin")]);
    assert!(run.was_killed(), "{}", run.text());
    std::fs::remove_dir_all(a.path("d")).unwrap();
    rules(&a, "");
    let run = a.synced();
    b.synced();
    assert!(b.exists("d/a"), "{}", run.text());
    assert_eq!(a.read("d/a"), b"v1", "{}", run.text());
}

/// An empty folder is as real as a file: beside something ignored, it still syncs, and so does what
/// holds it.
#[test]
fn an_empty_folder_beside_something_ignored_still_syncs() {
    let (_sb, _s, a, b) = pair("sel-empty");
    rules(&a, "*.log\n");
    a.write("n/x.log", "l");
    std::fs::create_dir(a.path("n/empty")).unwrap();
    a.write("only/x.log", "l");
    a.synced();
    b.synced();
    assert!(b.path("n/empty").is_dir());
    assert!(!b.exists("n/x.log"));
    assert!(!b.exists("only"), "nothing here but what is ignored");
}

#[test]
fn a_rule_covering_most_of_a_tree_is_not_a_mass_delete() {
    let (_sb, _s, a, b) = pair("sel-mass");
    for i in 0..40 {
        a.write(&format!("data/f{i}.log"), "x");
    }
    a.write("kept", "x");
    a.synced();
    b.synced();
    rules(&a, "data/\n");
    let run = a.synced();
    assert!(!run.stdout.contains("paused"), "{}", run.text());
    b.synced();
    assert!(b.exists("data/f7.log"));
}

/// A delete elsewhere of a folder holding what this device ignores: that has no other copy.
#[test]
fn a_folder_deleted_elsewhere_keeps_what_is_ignored_in_it_and_does_not_come_back() {
    let (sb, server, a, b) = pair("sel-holds");
    rules(&a, "*.log\n");
    a.write("proj/sub/code", "c");
    a.write("proj/sub/run.log", "only here");
    a.write("proj/top.log", "only here too");
    a.write("proj/plain", "p");
    a.synced();
    b.synced();
    assert!(b.exists("proj/sub/code"));

    std::fs::remove_dir_all(b.path("proj")).unwrap();
    b.cairn(&["sync", "--allow-mass-delete"]).expect_ok();
    let run = a.synced();
    assert_eq!(a.read("proj/sub/run.log"), b"only here", "{}", run.text());
    assert_eq!(a.read("proj/top.log"), b"only here too");
    assert!(!a.exists("proj/sub/code"));
    assert!(!a.exists("proj/plain"));

    // Settled, and the delete stands: nothing brings the folder back for anyone.
    a.synced();
    a.synced();
    b.synced();
    assert!(!b.exists("proj"));
    assert!(!on_server(&sb, &server, "probe").contains("proj"));

    // Something real in it later makes it a real folder again.
    a.write("proj/new", "n");
    a.synced();
    b.synced();
    assert_eq!(b.read("proj/new"), b"n");
    assert!(!b.exists("proj/top.log"));
}

/// The server would refuse deleting a folder that is not empty there.
#[test]
fn deleting_a_folder_here_keeps_what_the_server_holds_in_it_for_others() {
    let (_sb, _s, a, b) = pair("sel-pinned");
    rules(&a, "*.log\n");
    b.write("proj/run.log", "from b");
    b.write("proj/sub/deep.log", "from b too");
    b.write("proj/code", "c");
    b.synced();
    a.synced();
    assert!(a.exists("proj/code") && !a.exists("proj/run.log"));

    std::fs::remove_dir_all(a.path("proj")).unwrap();
    let run = a.cairn(&["sync", "--allow-mass-delete"]).expect_ok();
    assert!(a.exists("proj"), "{}", run.text());
    assert!(!run.stdout.contains("refused"), "{}", run.text());
    a.synced();
    b.synced();
    assert_eq!(b.read("proj/run.log"), b"from b");
    assert_eq!(b.read("proj/sub/deep.log"), b"from b too");
    assert!(
        !b.exists("proj/code"),
        "the delete of what a could see goes through"
    );
}

/// A rule for directories leaves a file of the same name in view, and the name is taken on the server.
#[test]
fn a_name_taken_on_the_server_by_something_out_of_view_keeps_both() {
    let (_sb, _s, a, b) = pair("sel-reserved");
    rules(&a, "build/\n");
    b.write("build/out", "dir on b");
    b.synced();
    a.write("build", "file on a");
    let run = a.synced();
    assert!(!run.stdout.contains("refused"), "{}", run.text());
    let copies = a.conflict_copies("", "build");
    assert_eq!(copies.len(), 1, "{}", run.text());
    assert_eq!(a.read(&copies[0]), b"file on a");
    b.synced();
    assert_eq!(b.read("build/out"), b"dir on b");
    assert_eq!(b.read(&copies[0]), b"file on a");
}

#[test]
fn something_moved_out_of_view_goes_from_here_and_stays_on_the_server() {
    let (sb, server, a, b) = pair("sel-out");
    rules(&a, "attic/\n");
    b.write("attic/old", "x");
    b.write("file", "f");
    b.write("dir/inner", "i");
    b.synced();
    a.synced();
    assert!(a.exists("file") && a.exists("dir/inner"));

    std::fs::rename(b.path("file"), b.path("attic/file")).unwrap();
    std::fs::rename(b.path("dir"), b.path("attic/dir")).unwrap();
    b.synced();
    let run = a.synced();
    assert!(!a.exists("file") && !a.exists("dir"), "{}", run.text());
    assert_eq!(b.read("attic/file"), b"f");
    let there = on_server(&sb, &server, "probe");
    assert!(there.contains("attic/dir/inner") && there.contains("attic/file"));
}

/// Moved out of view on one device, edited on the other: the edit must not vanish with the move.
#[test]
fn an_edit_of_something_moved_out_of_view_stays_as_a_new_entry() {
    let (_sb, _s, a, b) = pair("sel-out-edit");
    rules(&a, "attic/\n");
    b.write("attic/old", "x");
    b.write("file", "base");
    b.write("dir/edited", "base");
    b.write("dir/untouched", "base");
    b.synced();
    a.synced();

    a.write("file", "edited on a");
    a.write("dir/edited", "edited on a too");
    std::fs::rename(b.path("file"), b.path("attic/file")).unwrap();
    std::fs::rename(b.path("dir"), b.path("attic/dir")).unwrap();
    b.synced();
    let run = a.synced();
    assert_eq!(a.read("file"), b"edited on a", "{}", run.text());
    assert_eq!(a.read("dir/edited"), b"edited on a too");
    assert!(!a.exists("dir/untouched"), "{}", run.text());
    assert!(!run.stdout.contains("refused"), "{}", run.text());
    assert!(run.stdout.contains("synced at sequence"), "{}", run.text());

    // Everyone else has both: the moved originals and the edits.
    b.synced();
    assert_eq!(b.read("attic/file"), b"base");
    assert_eq!(b.read("attic/dir/edited"), b"base");
    assert_eq!(b.read("attic/dir/untouched"), b"base");
    assert_eq!(b.read("file"), b"edited on a");
    assert_eq!(b.read("dir/edited"), b"edited on a too");
}

#[test]
fn something_moved_into_view_arrives() {
    let (_sb, _s, a, b) = pair("sel-in");
    rules(&a, "attic/\n");
    b.write("attic/file", "f");
    b.write("attic/dir/inner", "i");
    b.synced();
    a.synced();
    assert!(!a.exists("attic"));
    std::fs::rename(b.path("attic/file"), b.path("file")).unwrap();
    std::fs::rename(b.path("attic/dir"), b.path("dir")).unwrap();
    b.synced();
    a.synced();
    assert_eq!(a.read("file"), b"f");
    assert_eq!(a.read("dir/inner"), b"i");
}

/// `mv` out of an ignored folder here, of a file the server also holds there.
#[test]
fn moving_a_file_out_of_an_ignored_folder_here_leaves_the_servers_copy_alone() {
    let (_sb, _s, a, b) = pair("sel-mv-out");
    a.write("attic/file", "body");
    a.synced();
    b.synced();
    rules(&a, "attic/\n");
    // Twice, so that its record here has gone: the worst case for telling what this file is.
    a.synced();
    a.synced();
    std::fs::rename(a.path("attic/file"), a.path("file")).unwrap();
    let run = a.synced();
    assert!(!run.stdout.contains("refused"), "{}", run.text());
    b.synced();
    assert_eq!(b.read("attic/file"), b"body");
    assert_eq!(b.read("file"), b"body");
}

/// The same, while its record is still here: it would otherwise push an object that already exists.
#[test]
fn moving_a_file_out_of_view_in_the_same_sync_a_rule_appears_keeps_both() {
    let (_sb, _s, a, b) = pair("sel-mv-now");
    a.write("attic/file", "body");
    a.synced();
    b.synced();
    rules(&a, "attic/\n");
    std::fs::rename(a.path("attic/file"), a.path("file")).unwrap();
    let run = a.synced();
    assert!(!run.stdout.contains("refused"), "{}", run.text());
    b.synced();
    assert_eq!(b.read("attic/file"), b"body");
    assert_eq!(b.read("file"), b"body");
}

/// Anything wrong with the rules stops the sync: "no rules" would upload what they keep back.
#[test]
fn rules_that_cannot_be_used_stop_the_sync_and_upload_nothing() {
    let (sb, server, a, _b) = pair("sel-bad");
    a.write("secret.key", "s");
    a.write("plain", "p");
    rules(&a, "*.key\n[z-a\n");
    let run = a.sync();
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains(".cairn/ignore"), "{}", run.text());
    std::fs::write(a.path(".cairn/ignore"), b"*.key\n\xff\xfe\n").unwrap();
    assert_eq!(a.sync().code, Some(1));
    // A symlink is not followed, and is an error too, not an absence. A file symlink on Windows needs a
    // privilege or developer mode.
    #[cfg(unix)]
    {
        std::fs::remove_file(a.path(".cairn/ignore")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", a.path(".cairn/ignore")).unwrap();
        assert_eq!(a.sync().code, Some(1));
    }
    assert!(on_server(&sb, &server, "probe").is_empty());
}

// --- Selective sync ---

#[test]
fn an_excluded_folder_stays_excluded_through_a_rename_and_comes_back_when_included() {
    let (sb, server, a, b) = pair("sel-exclude");
    b.write("photos/one", "1");
    b.write("photos/sub/two", "2");
    b.write("plain", "p");
    b.synced();
    a.synced();

    let run = a.cairn(&["exclude", "photos"]).expect_ok();
    assert!(run.stdout.contains("no longer synced"), "{}", run.text());
    assert!(a.exists("photos/one"), "the copy here stays unless asked");

    // Not looked at any more, in either direction.
    a.write("photos/one", "changed on a");
    a.synced();
    b.synced();
    assert_eq!(b.read("photos/one"), b"1");

    // A rename elsewhere does not bring it down under the new name.
    std::fs::rename(b.path("photos"), b.path("pictures")).unwrap();
    b.write("pictures/three", "3");
    b.synced();
    a.synced();
    assert!(!a.exists("pictures"), "{:?}", Picture::of(&a.root).names());
    let status = a.cairn(&["status"]);
    assert!(
        status
            .stdout
            .contains("excluded with `cairn-sync exclude`: pictures"),
        "{}",
        status.text()
    );

    // The copy here did not turn into a new folder for everyone just because the name came free.
    assert!(!b.exists("photos"), "{:?}", Picture::of(&b.root).names());
    assert!(!on_server(&sb, &server, "probe").contains("photos"));
    assert_eq!(a.read("photos/one"), b"changed on a");

    // Deleting the copy here means nothing to the server.
    std::fs::remove_dir_all(a.path("photos")).unwrap();
    a.synced();
    assert!(on_server(&sb, &server, "probe2").contains("pictures/sub/two"));

    a.cairn(&["include", "pictures"]).expect_ok();
    assert_eq!(a.read("pictures/one"), b"1");
    assert_eq!(a.read("pictures/sub/two"), b"2");
    assert_eq!(a.read("pictures/three"), b"3");
    b.synced();
    assert_eq!(b.read("pictures/one"), b"1");
}

/// Something else from the server at the name a copy was left at: the copy does not join uninvited,
/// sync says what is in the way, and it is one command to let it in. Then it is an ordinary folder.
#[test]
fn a_new_folder_at_the_name_of_a_kept_copy_waits_for_a_person_then_merges_for_good() {
    let (sb, server, a, b) = pair("sel-kept-name");
    b.write("photos/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "photos"]).expect_ok();
    a.write("photos/private", "only here");
    std::fs::rename(b.path("photos"), b.path("pictures")).unwrap();
    b.synced();
    a.synced();

    b.write("photos/one", "another one");
    b.write("photos/two", "2");
    b.synced();
    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());
    assert!(
        run.stdout.contains("something this device excludes"),
        "{}",
        run.text()
    );
    b.synced();
    assert!(!b.exists("photos/private"), "it went up uninvited");

    let run = a.cairn(&["include", "photos", "--merge"]).expect_ok();
    assert_eq!(a.read("photos/two"), b"2", "{}", run.text());
    assert_eq!(a.read("photos/one"), b"another one", "{}", run.text());
    assert_eq!(
        a.conflict_copies("photos", "one").len(),
        1,
        "{}",
        run.text()
    );
    assert!(!a.exists("pictures"));
    b.synced();
    assert_eq!(b.read("photos/private"), b"only here");

    // Moved on elsewhere: it must not be taken for a delete because the old name stays special.
    std::fs::rename(b.path("photos"), b.path("album")).unwrap();
    b.synced();
    let run = a.synced();
    b.synced();
    assert_eq!(b.read("album/two"), b"2", "{}", run.text());
    assert_eq!(b.read("album/one"), b"another one");
    assert!(a.exists("album/two"));
    assert!(on_server(&sb, &server, "probe").contains("album/two"));
}

/// Where an excluded folder moves on the server is no reason to stop syncing what is at that name here.
#[test]
fn an_excluded_folder_moving_onto_a_synced_folders_name_deletes_nothing() {
    let (_sb, _s, a, b) = pair("sel-rotate");
    b.write("current/c", "c");
    b.write("backup/precious1", "1");
    b.write("backup/precious2", "2");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "current"]).expect_ok();

    std::fs::rename(b.path("backup"), b.path("backup.1")).unwrap();
    std::fs::rename(b.path("current"), b.path("backup")).unwrap();
    b.synced();
    a.synced();
    a.synced();
    b.synced();
    assert_eq!(b.read("backup.1/precious1"), b"1");
    assert_eq!(b.read("backup.1/precious2"), b"2");
    assert_eq!(b.read("backup/c"), b"c");
    assert_eq!(a.read("backup.1/precious1"), b"1");
}

/// Something known moved here to where a copy once was, and the copy gone: an ordinary rename.
#[test]
fn a_known_file_moved_to_the_old_path_of_a_kept_copy_is_just_a_rename() {
    let (_sb, _s, a, b) = pair("sel-to-kept");
    b.write("photos/one", "1");
    b.write("doc", "precious");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "photos"]).expect_ok();
    std::fs::rename(b.path("photos"), b.path("pictures")).unwrap();
    b.synced();
    a.synced();

    std::fs::remove_dir_all(a.path("photos")).unwrap();
    std::fs::rename(a.path("doc"), a.path("photos")).unwrap();
    let run = a.synced();
    b.synced();
    assert_eq!(b.read("photos"), b"precious", "{}", run.text());
    assert!(!b.exists("doc"));
}

/// The copy is followed by what it is, not by where it was.
#[test]
fn a_kept_copy_stays_left_alone_when_it_or_its_parent_is_renamed_here() {
    let (sb, server, a, b) = pair("sel-copy-moves");
    b.write("p/d/one", "1");
    b.write("q-other", "x");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "p/d"]).expect_ok();

    std::fs::rename(a.path("p"), a.path("q")).unwrap();
    let run = a.synced();
    assert_eq!(a.read("q/d/one"), b"1", "{}", run.text());
    assert_eq!(a.conflict_copies("q", "d").len(), 0, "{}", run.text());
    std::fs::rename(a.path("q/d"), a.path("elsewhere")).unwrap();
    a.synced();
    let status = a.cairn(&["status"]);
    assert!(
        status
            .stdout
            .contains("left here when it was excluded: elsewhere"),
        "{}",
        status.text()
    );
    b.synced();
    assert_eq!(
        b.read("q/d/one"),
        b"1",
        "the server's stays, under the move"
    );
    assert!(!b.exists("elsewhere"));
    assert!(!on_server(&sb, &server, "probe").contains("elsewhere"));
}

/// Including under the server's new name must not upload the old copy at its old one.
#[test]
fn including_a_renamed_folder_leaves_the_old_copy_alone_and_says_so() {
    let (_sb, _s, a, b) = pair("sel-include-renamed");
    b.write("photos/one", "1");
    b.synced();
    a.synced();
    a.cairn(&["exclude", "photos"]).expect_ok();
    std::fs::rename(b.path("photos"), b.path("pictures")).unwrap();
    b.synced();
    a.synced();

    let run = a.cairn(&["include", "pictures"]).expect_ok();
    assert_eq!(a.read("pictures/one"), b"1", "{}", run.text());
    assert_eq!(a.read("photos/one"), b"1");
    b.synced();
    assert!(!b.exists("photos"), "{:?}", Picture::of(&b.root).names());
    let status = a.cairn(&["status"]);
    assert!(
        status.stdout.contains("left here when it was excluded"),
        "{}",
        status.text()
    );

    // And there is always a way out: it can be merged in, once asked.
    a.write("photos/extra", "new work");
    a.cairn(&["include", "photos", "--merge"]).expect_ok();
    b.synced();
    assert_eq!(b.read("photos/extra"), b"new work");
}

/// With its folder gone from the server, a copy must not become a place where work silently stops.
#[test]
fn a_copy_of_a_folder_deleted_elsewhere_can_always_be_brought_back() {
    let (_sb, _s, a, b) = pair("sel-copy-orphan");
    b.write("d/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "d"]).expect_ok();
    std::fs::remove_dir_all(b.path("d")).unwrap();
    b.synced();
    a.write("d/new", "new work");
    let run = a.synced();
    assert!(
        run.stdout.contains("by this device's choice"),
        "{}",
        run.text()
    );
    let status = a.cairn(&["status"]);
    assert!(
        status.stdout.contains("cairn-sync include --merge d"),
        "{}",
        status.text()
    );
    a.cairn(&["include", "d", "--merge"]).expect_ok();
    b.synced();
    assert_eq!(b.read("d/new"), b"new work");
}

/// Deleted elsewhere while excluded here, then edited here, in a folder that had merged in as ordinary.
#[test]
fn an_edit_in_a_folder_merged_from_a_copy_survives_a_delete_elsewhere() {
    let (_sb, _s, a, b) = pair("sel-merged-edit");
    b.write("photos/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "photos"]).expect_ok();
    std::fs::rename(b.path("photos"), b.path("pictures")).unwrap();
    b.write("photos/two", "2");
    b.synced();
    a.cairn(&["include", "photos", "--merge"]).expect_ok();
    a.synced();
    assert_eq!(a.read("photos/two"), b"2");

    a.write("photos/two", "edited on a");
    std::fs::remove_dir_all(b.path("photos")).unwrap();
    b.synced();
    let run = a.synced();
    b.synced();
    assert_eq!(a.read("photos/two"), b"edited on a", "{}", run.text());
    assert_eq!(b.read("photos/two"), b"edited on a", "{}", run.text());
}

#[test]
fn excluding_twice_says_so_and_does_nothing() {
    let (_sb, _s, a, b) = pair("sel-twice");
    b.write("d/one", "1");
    b.synced();
    a.synced();
    a.cairn(&["exclude", "d"]).expect_ok();
    let run = a.cairn(&["exclude", "d", "--remove-local"]);
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains("already excluded"), "{}", run.text());
    assert_eq!(a.read("d/one"), b"1");
}

// --- A removal this device made is not a delete this device made ---

/// No crash: one thing stuck keeps every sync from finishing. Moved out of view and back, and the
/// copies here were removed in between by this device.
#[test]
fn moved_out_of_view_and_back_during_syncs_that_never_finish_deletes_nothing() {
    let (_sb, _s, a, b) = pair("sel-out-back");
    rules(&a, "attic/\nblocked/\n");
    a.write("blocked/mine", "mine");
    b.write("attic/x", "x");
    b.write("f", "precious");
    b.write("d/g", "precious too");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    // A file arrives at the name of a folder a excludes: its syncs end partial from here on, with a
    // report that says what is in the way.
    b.write("blocked", "blocker");
    b.synced();
    let stuck = a.sync();
    assert_eq!(stuck.code, Some(3), "{}", stuck.text());
    assert!(
        stuck.stdout.contains("something this device excludes"),
        "{}",
        stuck.text()
    );

    std::fs::rename(b.path("f"), b.path("attic/f")).unwrap();
    std::fs::rename(b.path("d"), b.path("attic/d")).unwrap();
    b.synced();
    assert_eq!(a.sync().code, Some(3));
    assert!(!a.exists("f") && !a.exists("d"));

    std::fs::rename(b.path("attic/f"), b.path("f")).unwrap();
    std::fs::rename(b.path("attic/d"), b.path("d")).unwrap();
    b.synced();
    assert_eq!(a.sync().code, Some(3));
    b.synced();
    assert_eq!(b.read("f"), b"precious");
    assert_eq!(b.read("d/g"), b"precious too");
    assert_eq!(a.read("f"), b"precious");
    assert_eq!(a.read("d/g"), b"precious too");
}

/// The hole this closes has nothing to do with rules: a delete applied here and undone elsewhere.
#[test]
fn a_delete_undone_elsewhere_before_this_device_finished_is_not_a_delete_from_here() {
    let (_sb, _s, a, b) = pair("sel-restored");
    b.write("f", "precious");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    let at = seq_of(&b.cairn(&["sync"]));
    std::fs::remove_file(b.path("f")).unwrap();
    b.synced();
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.applied")]);
    assert!(run.was_killed(), "{}", run.text());
    assert!(!a.exists("f"));
    b.cairn(&["restore", "f", "--at", &at.to_string()])
        .expect_ok();
    let run = a.synced();
    b.synced();
    assert_eq!(b.read("f"), b"precious", "{}", run.text());
    assert_eq!(a.read("f"), b"precious", "{}", run.text());
}

fn seq_of(run: &Run) -> u64 {
    run.stdout
        .split("synced at sequence ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("{}", run.text()))
}

/// The other way round: given up just before a removal that a kill stopped. The file is still here, and
/// must not read as something new that undoes the delete.
#[test]
fn a_kill_between_giving_up_and_removing_does_not_undo_a_delete_from_elsewhere() {
    let (_sb, _s, a, b) = pair("sel-given-up");
    b.write("f", "x");
    b.write("d/g", "y");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    std::fs::remove_file(b.path("f")).unwrap();
    std::fs::remove_dir_all(b.path("d")).unwrap();
    b.synced();
    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "apply.given_up")]);
    assert!(run.was_killed(), "{}", run.text());
    assert!(a.exists("f"), "killed before it could remove anything");
    a.synced();
    a.synced();
    b.synced();
    assert!(!a.exists("f") && !a.exists("d"), "the delete stands");
    assert!(!b.exists("f") && !b.exists("d"), "it did not come back");
}

/// A delete from elsewhere, undone, while a sync here has not finished: a restore must hold.
/// Not on Windows, which makes no FIFOs in a folder.
#[cfg(unix)]
#[test]
fn a_restore_holds_while_another_device_cannot_finish_its_sync() {
    let (_sb, _s, a, b) = pair("sel-restore-stuck");
    a.write("plain", "p");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    let at = seq_of(&a.cairn(&["sync"]));
    // A fifo at a name that arrives keeps b from finishing, and it did not come from a rule.
    assert!(std::process::Command::new("mkfifo")
        .arg(b.path("stuck"))
        .status()
        .unwrap()
        .success());
    a.write("stuck", "blocks b");
    a.write("f", "precious");
    a.synced();
    let at_f = seq_of(&a.cairn(&["sync"]));
    assert_eq!(b.sync().code, Some(3));
    assert_eq!(b.read("f"), b"precious");

    std::fs::remove_file(a.path("f")).unwrap();
    a.synced();
    assert_eq!(b.sync().code, Some(3));
    assert!(!b.exists("f"));

    a.cairn(&["restore", "f", "--at", &at_f.to_string()])
        .expect_ok();
    assert_eq!(a.read("f"), b"precious");
    assert_eq!(b.sync().code, Some(3));
    a.synced();
    assert_eq!(a.read("f"), b"precious", "b deleted what it never deleted");
    assert_eq!(b.read("f"), b"precious");
    let _ = at;
}

/// Given up, then the removal failed because the folder could not be read: still held, not new.
#[test]
fn a_delete_elsewhere_stands_after_a_folder_was_briefly_unreadable_here() {
    if skip_as_root() {
        return;
    }
    let (_sb, _s, a, b) = pair("sel-unreadable");
    a.write("d/f", "victim");
    a.write("d/g", "other");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    chmod(&b.path("d"), 0o000);
    std::fs::remove_file(a.path("d/f")).unwrap();
    a.synced();
    let _ = b.sync();
    let _ = b.sync();
    chmod(&b.path("d"), 0o755);
    let run = b.synced();
    a.synced();
    b.synced();
    assert!(!a.exists("d/f"), "it came back: {}", run.text());
    assert!(!b.exists("d/f"), "{}", run.text());
    assert_eq!(a.read("d/g"), b"other");
}

/// Killed between the names of a hard link it was removing: the delete from elsewhere stands.
#[test]
fn a_kill_between_the_names_of_a_hard_link_does_not_undo_a_delete() {
    let (_sb, _s, a, b) = pair("sel-hl-kill");
    a.write("x", "victim");
    std::fs::hard_link(a.path("x"), a.path("y")).unwrap();
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::remove_file(a.path("x")).unwrap();
    std::fs::remove_file(a.path("y")).unwrap();
    a.synced();
    let run = b.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "apply.unlink.done")]);
    assert!(run.was_killed(), "{}", run.text());
    let run = b.synced();
    a.synced();
    assert!(!a.exists("x") && !a.exists("y"), "{}", run.text());
    assert!(!b.exists("x") && !b.exists("y"), "{}", run.text());
    assert!(!run.stdout.contains("changed here"), "{}", run.text());
}

/// With a parent renamed here as well: the link is found by what it is in, not by a stale path.
#[test]
fn a_kill_while_removing_a_file_in_a_folder_renamed_here_does_not_undo_a_delete() {
    let (_sb, _s, a, b) = pair("sel-renamed-kill");
    a.write("d/x", "victim");
    a.write("d/y", "stays");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::remove_file(a.path("d/x")).unwrap();
    a.synced();
    std::fs::rename(b.path("d"), b.path("d2")).unwrap();
    let run = b.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "apply.given_up")]);
    assert!(run.was_killed(), "{}", run.text());
    let run = b.synced();
    a.synced();
    assert!(!a.exists("d2/x"), "{}", run.text());
    assert!(!b.exists("d2/x"), "{}", run.text());
    assert_eq!(a.read("d2/y"), b"stays");
}

/// `rm -rf` and generate it again: an excluded path stays out for what is new there.
#[test]
fn a_folder_deleted_and_regenerated_stays_excluded() {
    let (sb, server, a, b) = pair("sel-regen");
    b.write("node_modules/x", "from b");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "node_modules"]).expect_ok();
    std::fs::remove_dir_all(a.path("node_modules")).unwrap();
    a.write("node_modules/x", "generated");
    a.write("node_modules/.npmrc", "a token");
    let run = a.synced();
    assert_eq!(a.read("node_modules/x"), b"generated", "{}", run.text());
    assert_eq!(
        a.conflict_copies("", "node_modules").len(),
        0,
        "{}",
        run.text()
    );
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains(".npmrc")), "{there:?}");
    b.synced();
    assert_eq!(b.read("node_modules/x"), b"from b");
}

/// An excluded single file saved the usual way, by a new file renamed over it.
#[test]
fn an_excluded_file_saved_by_rename_stays_excluded() {
    let (sb, server, a, b) = pair("sel-file-save");
    b.write("config", "shared");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "config"]).expect_ok();
    a.write("config.new", "local secret");
    std::fs::rename(a.path("config.new"), a.path("config")).unwrap();
    let run = a.synced();
    assert_eq!(a.read("config"), b"local secret", "{}", run.text());
    b.synced();
    assert_eq!(b.read("config"), b"shared");
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains("conflict")), "{there:?}");
}

/// A copy forgotten for having gone out of sight would then be uploaded.
#[test]
fn a_kept_copy_is_not_forgotten_when_it_goes_out_of_sight_for_a_while() {
    let (sb, server, a, b) = pair("sel-out-of-sight");
    b.write("d/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "d"]).expect_ok();
    a.write("d/private", "private");
    std::fs::rename(b.path("d"), b.path("e")).unwrap();
    b.synced();
    a.synced();

    // Under a rule for one sync, then back; then the rule itself covering it for one sync.
    rules(&a, "attic/\n");
    std::fs::create_dir(a.path("attic")).unwrap();
    std::fs::rename(a.path("d"), a.path("attic/d")).unwrap();
    a.synced();
    std::fs::rename(a.path("attic/d"), a.path("d")).unwrap();
    a.synced();
    rules(&a, "d/\nattic/\n");
    a.synced();
    rules(&a, "attic/\n");
    let run = a.synced();
    assert_eq!(a.conflict_copies("", "d").len(), 0, "{}", run.text());
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains("private")), "{there:?}");
}

/// Restored from a backup, every inode is new: the path still says what it is.
#[test]
fn a_kept_copy_stays_out_when_the_root_is_restored_from_a_backup() {
    let (sb, server, a, b) = pair("sel-backup");
    b.write("d/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "d"]).expect_ok();
    a.write("d/private", "private");
    a.synced();
    let held = sb.path("backup");
    copy_tree(&a.root, &held);
    std::fs::remove_dir_all(&a.root).unwrap();
    std::fs::rename(&held, &a.root).unwrap();
    let run = a.synced();
    assert_eq!(a.read("d/private"), b"private", "{}", run.text());
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains("private")), "{there:?}");
}

/// Excluding what is only here must not be how it reaches the server.
#[test]
fn excluding_something_only_here_uploads_nothing_and_says_what_to_do_instead() {
    let (sb, server, a, _b) = pair("sel-new-local");
    a.write("secret/key", "do not upload");
    let run = a.cairn(&["exclude", "secret"]);
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains(".cairn/ignore"), "{}", run.text());
    assert!(on_server(&sb, &server, "probe").is_empty());
}

/// New work in a folder being excluded stays here: that is what excluding it asks for.
#[test]
fn excluding_a_folder_does_not_upload_what_is_new_in_it_first() {
    let (sb, server, a, b) = pair("sel-new-in");
    b.write("d/one", "1");
    b.synced();
    a.synced();
    a.write("d/private", "new here");
    a.cairn(&["exclude", "d"]).expect_ok();
    assert_eq!(a.read("d/private"), b"new here");
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains("private")), "{there:?}");
}

/// A class that the matcher would quietly not match is refused, rather than uploading what it names.
#[test]
fn a_rule_this_matcher_cannot_honour_stops_the_sync() {
    let (sb, server, a, _b) = pair("sel-class");
    rules(&a, "[[:digit:]]*.key\n");
    a.write("1.key", "secret");
    let run = a.sync();
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(on_server(&sb, &server, "probe").is_empty());
}

#[test]
fn status_lists_what_it_says_it_lists() {
    let (_sb, _s, a, b) = pair("sel-status");
    rules(&a, "*.log\n");
    b.write("far.log", "x");
    b.synced();
    let run = a.synced();
    assert!(run.stdout.contains("1 on the server"), "{}", run.text());
    let status = a.cairn(&["status"]);
    assert!(
        status
            .stdout
            .contains("on the server, not synced here: far.log"),
        "{}",
        status.text()
    );
}

/// A folder left only for what it holds, recreated elsewhere: they merge, and nothing is renamed, which
/// could take an ignored file out from under the rule that names its path.
#[test]
fn a_folder_kept_for_what_is_ignored_merges_with_one_recreated_elsewhere() {
    let (_sb, _s, a, b) = pair("sel-recreated");
    rules(&a, "d/x.log\n");
    a.write("d/x.log", "ignored");
    a.write("d/y", "y");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::remove_dir_all(b.path("d")).unwrap();
    b.synced();
    a.synced();
    assert_eq!(a.read("d/x.log"), b"ignored");
    b.write("d/new", "n");
    b.synced();
    let run = a.synced();
    assert_eq!(a.read("d/new"), b"n", "{}", run.text());
    assert_eq!(a.read("d/x.log"), b"ignored");
    assert_eq!(a.conflict_copies("", "d").len(), 0, "{}", run.text());
    b.synced();
    assert!(!b.exists("d/x.log"), "{:?}", Picture::of(&b.root).names());
}

/// A save by rename to another name of a hard-linked file, in the last moment before it is replaced.
#[test]
fn a_save_to_the_second_name_of_a_file_being_replaced_is_kept() {
    let (sb, _s, a, b) = pair("sel-hl-save");
    a.write("x", "base");
    std::fs::hard_link(a.path("x"), a.path("y")).unwrap();
    a.synced();
    b.synced();
    a.write("x", "edit from a");
    a.synced();
    std::thread::sleep(std::time::Duration::from_millis(400));

    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("apply.replace.before_rename:{}", gate.display());
    std::thread::scope(|s| {
        let syncing = s.spawn(|| b.cairn_env(&["sync"], &[("CAIRN_FP_PAUSE", &spec)]));
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed().as_secs() < 60, "never reached");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        b.write("y.swp", "typed on b, only copy");
        std::fs::rename(b.path("y.swp"), b.path("y")).unwrap();
        std::fs::write(gate.join("go"), "").unwrap();
        let _ = syncing.join();
    });
    b.synced();
    a.synced();
    b.synced();
    let found = |d: &Device| {
        Picture::of(&d.root).names().iter().any(|n| {
            !n.is_empty() && std::fs::read(d.path(n)).is_ok_and(|c| c == b"typed on b, only copy")
        })
    };
    assert!(found(&b), "gone: {:?}", Picture::of(&b.root).names());
    assert!(found(&a), "not on a: {:?}", Picture::of(&a.root).names());
}

// --- `exclude` on a device not in step with the server ---

/// What the server has at a path may not be what is here: leaving the wrong one out reads the right one
/// as deleted, for everyone.
#[test]
fn excluding_a_path_that_means_something_else_on_the_server_is_refused() {
    let (_sb, _s, a, b) = pair("sel-out-of-step");
    a.write("d/f", "work");
    a.write("d/g", "more");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::rename(a.path("d"), a.path("e")).unwrap();
    a.write("d/new", "new");
    a.synced();

    let run = b.cairn(&["exclude", "d"]);
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains("do not agree yet"), "{}", run.text());
    b.synced();
    a.synced();
    assert_eq!(a.read("e/f"), b"work");
    assert_eq!(a.read("e/g"), b"more");
    // In step, it is fine.
    b.cairn(&["exclude", "d"]).expect_ok();
    a.synced();
    assert_eq!(a.read("e/f"), b"work");
}

/// Renamed here, so that what the server calls that path is at another one here.
#[test]
fn excluding_a_path_renamed_here_is_refused_and_uploads_nothing() {
    let (sb, server, a, b) = pair("sel-renamed-first");
    b.write("proj/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    std::fs::rename(a.path("proj"), a.path("proj2")).unwrap();
    a.write("proj2/private", "private");
    let run = a.cairn(&["exclude", "proj"]);
    assert_eq!(run.code, Some(1), "{}", run.text());
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains("private")), "{there:?}");
    assert!(there.contains("proj/one"));
}

/// Something known moved to that path here is not the server's, nor a copy.
#[test]
fn excluding_a_path_something_else_was_moved_to_is_refused() {
    let (_sb, _s, a, b) = pair("sel-swapped");
    b.write("d/one", "1");
    b.write("e", "precious");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    std::fs::rename(a.path("d"), a.path("old-d")).unwrap();
    std::fs::rename(a.path("e"), a.path("d")).unwrap();
    let run = a.cairn(&["exclude", "d"]);
    assert_eq!(run.code, Some(1), "{}", run.text());
    b.synced();
    assert_eq!(b.read("e"), b"precious");
}

/// A file regenerated at an excluded path, on an inode number just freed by something this device
/// knows: a reused number is not that thing, so it stays out. Not on Windows: an NTFS file ID carries a
/// sequence number, so a freed one never comes back.
#[cfg(unix)]
#[test]
fn a_regenerated_copy_on_a_reused_inode_stays_out() {
    // Its inode numbers apart from the other tests', or one of theirs takes the number it frees.
    let sb = Sandbox::apart("sel-reuse");
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    b.write("conf.local", "shared");
    b.write("old.txt", "old");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "conf.local"]).expect_ok();
    let old = inode(&a.path("old.txt"));
    std::fs::remove_file(a.path("old.txt")).unwrap();
    std::fs::remove_file(a.path("conf.local")).unwrap();
    // Until the number comes back, as it soon does on ext4. Each miss is kept until the end: removed at
    // once, its number would be the next one's, and conf.local's number, freed too, would come back
    // every time.
    let mut reused = false;
    let mut made = Vec::new();
    for i in 0..2000 {
        let tmp = a.path(&format!("tmp{i}"));
        std::fs::write(&tmp, "API_KEY=secret").unwrap();
        if inode(&tmp) == old {
            std::fs::rename(&tmp, a.path("conf.local")).unwrap();
            reused = true;
            break;
        }
        made.push(tmp);
    }
    for tmp in made {
        std::fs::remove_file(tmp).unwrap();
    }
    if !reused {
        skip("the inode number did not come back on this filesystem");
        return;
    }
    let run = a.synced();
    let there = on_server(&sb, &server, "probe");
    assert!(
        !there.iter().any(|n| n.contains("conflict")),
        "{there:?}: {}",
        run.text()
    );
}

/// Regenerated before this device has seen that the server has something new at that path: the same
/// as if it had seen it first.
#[test]
fn a_regenerated_copy_does_not_go_up_when_the_server_gets_that_name_too() {
    let (sb, server, a, b) = pair("sel-regen-server");
    b.write("build/out", "o");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "build"]).expect_ok();
    std::fs::remove_dir_all(a.path("build")).unwrap();
    a.write("build/secret", "secret");
    std::fs::rename(b.path("build"), b.path("build-old")).unwrap();
    b.write("build/new", "n");
    b.synced();

    let run = a.sync();
    assert_eq!(run.code, Some(3), "{}", run.text());
    assert!(
        run.stdout.contains("something this device excludes"),
        "{}",
        run.text()
    );
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains("secret")), "{there:?}");
}

/// What `status` says to delete has to be the copy, not a real folder that has come to that path.
#[test]
fn status_does_not_call_a_real_shared_folder_a_copy() {
    let (_sb, _s, a, b) = pair("sel-status-real");
    b.write("d/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "d"]).expect_ok();
    std::fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    std::fs::rename(b.path("d"), b.path("d2")).unwrap();
    b.write("d/shared", "s");
    b.synced();
    // A new folder from the server stands where the copy was, and nothing here is in its way.
    let run = a.synced();
    assert_eq!(a.read("d/shared"), b"s", "{}", run.text());
    let status = a.cairn(&["status"]);
    assert!(!status.stdout.contains("delete it"), "{}", status.text());
}

// --- This device's own old news ---

/// Its own delete, said to the server and not finished here: a restore elsewhere must hold.
#[test]
fn a_devices_own_delete_is_not_made_again_over_a_restore() {
    let (_sb, _s, a, b) = pair("sel-own-delete");
    a.write("f", "precious");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    let at = seq_of(&a.cairn(&["sync"]));
    b.synced();
    std::fs::remove_file(b.path("f")).unwrap();
    let run = b.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.dequeued")]);
    assert!(run.was_killed(), "{}", run.text());
    a.synced();
    assert!(!a.exists("f"));
    a.cairn(&["restore", "f", "--at", &at.to_string()])
        .expect_ok();
    let run = b.synced();
    a.synced();
    assert_eq!(a.read("f"), b"precious", "deleted again: {}", run.text());
    assert_eq!(b.read("f"), b"precious", "{}", run.text());
}

/// A delete the server refused stays this device's delete to make, once it has seen why.
#[test]
fn a_refused_delete_of_something_edited_elsewhere_leaves_the_edit() {
    let (sb, _s, a, b) = pair("sel-refused");
    a.write("f", "base");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::remove_file(b.path("f")).unwrap();
    // The edit lands between b's look at the server and b's commit.
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("sync.enqueued:{}", gate.display());
    std::thread::scope(|s| {
        let syncing = s.spawn(|| b.cairn_env(&["sync"], &[("CAIRN_FP_PAUSE", &spec)]));
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed().as_secs() < 60, "never reached");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        a.write("f", "edited on a");
        a.synced();
        std::fs::write(gate.join("go"), "").unwrap();
        let _ = syncing.join();
    });
    let run = b.synced();
    a.synced();
    assert_eq!(a.read("f"), b"edited on a", "{}", run.text());
    assert_eq!(b.read("f"), b"edited on a", "{}", run.text());
}

/// A rename that arrived here, then a kill, then a delete elsewhere: not a change made here.
#[test]
fn a_rename_that_arrived_before_a_kill_does_not_undo_a_later_delete() {
    let (_sb, _s, a, b) = pair("sel-arrived-move");
    a.write("f", "f");
    a.write("d/inner", "i");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::rename(a.path("f"), a.path("g")).unwrap();
    std::fs::rename(a.path("d"), a.path("e")).unwrap();
    a.synced();
    let run = b.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.applied")]);
    assert!(run.was_killed(), "{}", run.text());
    assert!(b.exists("g") && b.exists("e/inner"));
    std::fs::remove_file(a.path("g")).unwrap();
    std::fs::remove_dir_all(a.path("e")).unwrap();
    a.synced();
    let run = b.synced();
    a.synced();
    assert!(!a.exists("g") && !a.exists("e"), "{}", run.text());
    assert!(!b.exists("g") && !b.exists("e"), "{}", run.text());
    assert!(!run.stdout.contains("changed here"), "{}", run.text());
}

/// An editor's save of a file being removed: an edit against a delete, and said to be.
#[test]
fn a_save_made_while_a_file_is_being_removed_is_reported_as_surviving_the_delete() {
    let (sb, _s, a, b) = pair("sel-save-removal");
    a.write("f", "base");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::remove_file(a.path("f")).unwrap();
    a.synced();
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("apply.prefetched:{}", gate.display());
    let run = std::thread::scope(|s| {
        let syncing = s.spawn(|| b.cairn_env(&["sync"], &[("CAIRN_FP_PAUSE", &spec)]));
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed().as_secs() < 60, "never reached");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        b.write("f.swp", "saved");
        std::fs::rename(b.path("f.swp"), b.path("f")).unwrap();
        std::fs::write(gate.join("go"), "").unwrap();
        syncing.join().unwrap()
    });
    assert_eq!(b.read("f"), b"saved", "{}", run.text());
    assert!(
        run.stdout.contains("changed here but deleted"),
        "{}",
        run.text()
    );
    let _ = b.sync();
    a.synced();
    assert_eq!(a.read("f"), b"saved");
}

/// Three names, one saved over in the last moment: all three stay one file, with the other device's
/// version, and the save is a conflict copy beside them.
#[test]
fn a_save_to_one_name_of_three_keeps_all_three_one_file() {
    let (sb, _s, a, b) = pair("sel-hl-three");
    a.write("x", "base");
    std::fs::hard_link(a.path("x"), a.path("y")).unwrap();
    std::fs::hard_link(a.path("x"), a.path("z")).unwrap();
    a.synced();
    b.synced();
    a.write("x", "edit from a");
    a.synced();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("apply.replace.before_rename:{}", gate.display());
    std::thread::scope(|s| {
        let syncing = s.spawn(|| b.cairn_env(&["sync"], &[("CAIRN_FP_PAUSE", &spec)]));
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed().as_secs() < 60, "never reached");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        b.write("y.swp", "typed on b");
        std::fs::rename(b.path("y.swp"), b.path("y")).unwrap();
        std::fs::write(gate.join("go"), "").unwrap();
        let _ = syncing.join();
    });
    let _ = b.sync();
    a.synced();
    b.synced();
    for d in [&a, &b] {
        for n in ["x", "y", "z"] {
            assert_eq!(d.read(n), b"edit from a", "{n} on {}", d.name);
        }
        assert_eq!(nlink(&d.path("x")), 3, "the names split on {}", d.name);
        let copies = d.conflict_copies("", "y");
        assert_eq!(copies.len(), 1, "on {}: {copies:?}", d.name);
        assert_eq!(d.read(&copies[0]), b"typed on b");
    }
}

/// An edit in place to the second name, after the first has been replaced: the names stay one file, with
/// the other device's version, and the edit is a conflict copy beside them, in everyone's inbox. Left at
/// its name, it would split the file in two, and that name would take the edit for everyone.
#[test]
fn an_edit_in_place_of_the_second_name_in_the_last_moment_is_kept() {
    let (sb, _s, a, b) = pair("sel-hl-inplace");
    a.write("x", "base");
    std::fs::hard_link(a.path("x"), a.path("y")).unwrap();
    a.synced();
    b.synced();
    a.write("x", "edit from a");
    a.synced();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("apply.replace.renamed:{}", gate.display());
    std::thread::scope(|s| {
        let syncing = s.spawn(|| b.cairn_env(&["sync"], &[("CAIRN_FP_PAUSE", &spec)]));
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed().as_secs() < 60, "never reached");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::write(b.path("y"), "typed on b, longer than it was").unwrap();
        std::fs::write(gate.join("go"), "").unwrap();
        let run = syncing.join().unwrap();
        assert!(run.stdout.contains(" 1 conflict(s)"), "{}", run.text());
    });
    a.synced();
    b.synced();
    for d in [&a, &b] {
        assert_eq!(d.read("x"), b"edit from a", "on {}", d.name);
        assert_eq!(d.read("y"), b"edit from a", "on {}", d.name);
        assert_eq!(nlink(&d.path("x")), 2, "the names split on {}", d.name);
        let copies = d.conflict_copies("", "y");
        assert_eq!(copies.len(), 1, "on {}: {copies:?}", d.name);
        assert_eq!(d.read(&copies[0]), b"typed on b, longer than it was");
        let inbox = d.cairn(&["conflicts"]).expect_ok();
        // Named by the file's first name, which is the other.
        let line = format!("{}: the competing version of x", copies[0]);
        assert!(inbox.stdout.contains(&line), "{}", inbox.text());
    }
}

/// A rename that arrived, while the edit that came with it did not, because a file here changed in the
/// meantime: the server's version is not what is on disk, and must not be recorded as if it were.
#[test]
fn a_received_move_with_an_edit_it_could_not_apply_keeps_both_versions() {
    let (sb, _s, a, b) = pair("sel-move-edit");
    a.write("x", "original");
    a.synced();
    b.synced();
    std::thread::sleep(std::time::Duration::from_millis(400));
    std::fs::rename(a.path("x"), a.path("y")).unwrap();
    a.write("y", "version from a");
    a.synced();

    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let spec = format!("apply.move.done:{}", gate.display());
    std::thread::scope(|s| {
        let syncing = s.spawn(|| b.cairn_env(&["sync"], &[("CAIRN_FP_PAUSE", &spec)]));
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed().as_secs() < 60, "never reached");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        b.write("y", "version from b");
        std::fs::write(gate.join("go"), "").unwrap();
        let _ = syncing.join();
    });
    let _ = b.sync();
    a.synced();
    b.synced();
    let holds = |d: &Device, want: &[u8]| {
        Picture::of(&d.root)
            .names()
            .iter()
            .any(|n| !n.is_empty() && std::fs::read(d.path(n)).is_ok_and(|c| c == want))
    };
    for d in [&a, &b] {
        assert!(
            holds(d, b"version from a"),
            "lost on {}: {:?}",
            d.name,
            Picture::of(&d.root).names()
        );
        assert!(
            holds(d, b"version from b"),
            "lost on {}: {:?}",
            d.name,
            Picture::of(&d.root).names()
        );
    }
}

/// Refused, then killed before it could say so: the delete is still this device's to make.
#[test]
fn a_refused_commit_with_a_kill_after_it_keeps_the_delete_in_it() {
    let (sb, _s, a, b) = pair("sel-refused-kill");
    a.write("x", "x");
    a.write("y", "base");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::remove_file(b.path("x")).unwrap();
    b.write("y", "edited on b");
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("sync.enqueued:{}", gate.display());
    std::thread::scope(|s| {
        let syncing = s.spawn(|| {
            b.cairn_env(
                &["sync"],
                &[
                    ("CAIRN_FP_PAUSE", &pause),
                    ("CAIRN_FP_CRASH", "sync.dequeued"),
                ],
            )
        });
        let start = std::time::Instant::now();
        while !gate.join("reached").exists() {
            assert!(start.elapsed().as_secs() < 60, "never reached");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        a.write("y", "edited on a");
        a.synced();
        std::fs::write(gate.join("go"), "").unwrap();
        let run = syncing.join().unwrap();
        assert!(run.was_killed(), "{}", run.text());
    });
    let run = b.synced();
    a.synced();
    assert!(!a.exists("x"), "{}", run.text());
    assert!(!b.exists("x"), "{}", run.text());
}

/// Refused when replayed from the queue, after a kill before it was first sent.
#[test]
fn a_commit_refused_on_replay_keeps_the_delete_in_it() {
    let (_sb, _s, a, b) = pair("sel-refused-replay");
    a.write("x", "x");
    a.write("y", "base");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::remove_file(b.path("x")).unwrap();
    b.write("y", "edited on b");
    let run = b.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.enqueued")]);
    assert!(run.was_killed(), "{}", run.text());
    a.write("y", "edited on a");
    a.synced();
    let run = b.synced();
    a.synced();
    assert!(!a.exists("x"), "{}", run.text());
    assert!(!b.exists("x"), "{}", run.text());
}

/// Something under the folder differs on the server: its records here would go with the wrong things.
#[test]
fn excluding_a_folder_that_differs_underneath_is_refused() {
    let (_sb, _s, a, b) = pair("sel-under");
    a.write("d/one", "1");
    a.write("top/g", "g");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::rename(a.path("top/g"), a.path("d/g")).unwrap();
    a.synced();
    let run = b.cairn(&["exclude", "d"]);
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(run.stderr.contains("do not agree yet"), "{}", run.text());
}

/// A file from the server at the path of a copy, then an editor's save of it before a sync finishes.
#[test]
fn a_held_file_at_the_path_of_a_copy_is_not_taken_for_the_copy() {
    let (_sb, _s, a, b) = pair("sel-held-at-copy");
    a.write("f", "old");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    b.cairn(&["exclude", "f"]).expect_ok();
    std::fs::remove_file(b.path("f")).unwrap();
    b.synced();
    std::fs::rename(a.path("f"), a.path("f-old")).unwrap();
    a.write("f", "new from a");
    a.synced();
    let run = b.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.applied")]);
    assert!(run.was_killed(), "{}", run.text());
    assert_eq!(b.read("f"), b"new from a");
    b.write("f.swp", "saved on b");
    std::fs::rename(b.path("f.swp"), b.path("f")).unwrap();
    let run = b.synced();
    a.synced();
    let holds = a.exists("f") || !a.conflict_copies("", "f").is_empty();
    assert!(holds, "deleted for everyone: {}", run.text());
}

/// A copy moved over something this device holds: the user replaced it, and nothing is deleted.
#[test]
fn a_copy_moved_over_a_held_file_does_not_delete_that_file() {
    let (_sb, _s, a, b) = pair("sel-copy-over");
    b.write("ex", "ex");
    b.write("held", "notes");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "ex"]).expect_ok();
    std::fs::rename(a.path("ex"), a.path("held")).unwrap();
    let run = a.synced();
    b.synced();
    assert!(b.exists("held"), "deleted for everyone: {}", run.text());
}

/// A stuck device, or one that has not downloaded a folder, must still be able to leave it out.
#[test]
fn a_folder_this_device_has_nothing_of_can_be_excluded_while_it_is_behind() {
    let (_sb, _s, a, b) = pair("sel-behind");
    a.write("plain", "p");
    a.synced();
    b.synced();
    a.write("huge/f", "too big to come down");
    a.synced();
    b.cairn(&["exclude", "huge"]).expect_ok();
    assert!(!b.exists("huge"));
}

/// Told to move the copy away, or doing so anyway: it does not go up for having moved.
#[test]
fn a_copy_moved_aside_before_including_stays_out() {
    let (sb, server, a, b) = pair("sel-aside");
    b.write("ex/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "ex"]).expect_ok();
    a.write("ex/private", "private notes");
    a.synced();
    std::fs::rename(a.path("ex"), a.path("ex-aside")).unwrap();
    let run = a.cairn(&["include", "ex"]).expect_ok();
    assert_eq!(a.read("ex/one"), b"1", "{}", run.text());
    assert_eq!(a.read("ex-aside/private"), b"private notes");
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains("private")), "{there:?}");
}

/// Moved aside and generated again in one go: one record, two things, and neither goes up.
#[test]
fn a_copy_moved_aside_and_regenerated_at_once_both_stay_out() {
    let (sb, server, a, b) = pair("sel-aside-regen");
    b.write("ex/one", "1");
    for i in 0..12 {
        b.write(&format!("plain{i}"), "p");
    }
    b.synced();
    a.synced();
    a.cairn(&["exclude", "ex"]).expect_ok();
    a.write("ex/private-old", "old");
    a.synced();
    std::fs::rename(a.path("ex"), a.path("aaa-old")).unwrap();
    a.write("ex/private-new", "new");
    a.synced();
    a.synced();
    a.synced();
    let there = on_server(&sb, &server, "probe");
    assert!(!there.iter().any(|n| n.contains("private")), "{there:?}");
}

/// Killed anywhere in `exclude --remove-local`, then changed its mind.
fn remove_local_killed_at_any_step_then_included_deletes_nothing(shard: Shard) {
    for step in each_step_to_the_end(shard) {
        let sb = Sandbox::fast("sel-rm-kill");
        let server = Server::start(sb.path("server"));
        let a = Device::new(&sb, &server, "a");
        let b = Device::new(&sb, &server, "b");
        b.write("d/one", "1");
        b.write("d/sub/two", "2");
        b.write("d/three", "3");
        for i in 0..12 {
            b.write(&format!("plain{i}"), "p");
        }
        b.synced();
        a.synced();
        let run = a.cairn_env(
            &["exclude", "d", "--remove-local"],
            &[("CAIRN_FP_CRASH", &step.to_string())],
        );
        let killed = run.was_killed();
        if !killed {
            assert!(step > 20, "never killed");
        }
        // However far it got: excluded or not, it comes back.
        let _ = a.cairn(&["include", "d", "--merge"]);
        a.synced();
        b.synced();
        for (path, want) in [("d/one", "1"), ("d/sub/two", "2"), ("d/three", "3")] {
            assert_eq!(b.read(path), want.as_bytes(), "step {step}");
            assert_eq!(a.read(path), want.as_bytes(), "step {step}");
        }
        if !killed {
            return;
        }
    }
}

shards!(remove_local_killed_at_any_step_then_included_deletes_nothing: s01 s02 s03 s04 s05 s06
    s07 s08 s09 s10 s11 s12);

// --- Hard links across an exclusion ---

#[test]
fn removing_the_hidden_name_of_a_hard_link_here_removes_nothing_when_the_rule_is_lifted() {
    let (_sb, _s, a, b) = pair("sel-hl-rm");
    b.write("outside", "v1");
    std::fs::create_dir(b.path("d")).unwrap();
    std::fs::hard_link(b.path("outside"), b.path("d/a")).unwrap();
    b.synced();
    a.synced();
    rules(&a, "d/\n");
    a.synced();
    std::fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    rules(&a, "");
    let run = a.synced();
    b.synced();
    assert!(b.exists("d/a"), "{}", run.text());
    assert_eq!(nlink(&b.path("outside")), 2);
    assert_eq!(nlink(&a.path("outside")), 2, "{}", run.text());
}

#[test]
fn an_edit_elsewhere_while_one_name_of_a_hard_link_is_hidden_is_not_rewound() {
    let (_sb, _s, a, b) = pair("sel-hl-edit");
    b.write("outside", "v1");
    std::fs::create_dir(b.path("d")).unwrap();
    std::fs::hard_link(b.path("outside"), b.path("d/a")).unwrap();
    b.synced();
    a.synced();
    rules(&a, "d/\n");
    a.synced();

    b.write("d/a", "v2");
    b.synced();
    a.synced();
    assert_eq!(a.read("outside"), b"v2");
    rules(&a, "");
    let run = a.synced();
    b.synced();
    a.synced();
    assert_eq!(b.read("d/a"), b"v2", "{}", run.text());
    assert_eq!(a.read("d/a"), b"v2", "{}", run.text());
    assert_eq!(b.read("outside"), b"v2", "{}", run.text());
}

#[test]
fn a_rule_file_that_cannot_be_read_stops_the_sync() {
    if skip_as_root() {
        return;
    }
    let (sb, server, a, _b) = pair("sel-unreadable-rules");
    a.write("secret", "s");
    rules(&a, "secret\n");
    chmod(&a.path(".cairn/ignore"), 0o000);
    let run = a.sync();
    assert_eq!(run.code, Some(1), "{}", run.text());
    chmod(&a.path(".cairn/ignore"), 0o600);
    assert!(on_server(&sb, &server, "probe").is_empty());
}

/// Not on Windows, which makes no FIFOs in a folder.
#[cfg(unix)]
#[test]
fn a_rule_file_that_is_a_fifo_stops_the_sync_rather_than_reading_as_no_rules() {
    let (sb, server, a, _b) = pair("sel-fifo");
    a.write("secret", "s");
    let made = std::process::Command::new("mkfifo")
        .arg(a.path(".cairn/ignore"))
        .status()
        .unwrap();
    assert!(made.success());
    let run = a.sync();
    assert_eq!(run.code, Some(1), "{}", run.text());
    assert!(on_server(&sb, &server, "probe").is_empty());
}

#[test]
fn including_again_merges_a_copy_that_stayed_here_and_changed() {
    let (_sb, _s, a, b) = pair("sel-include");
    b.write("photos/same", "same");
    b.write("photos/edited", "server");
    b.synced();
    a.synced();
    a.cairn(&["exclude", "photos"]).expect_ok();
    a.write("photos/edited", "local");
    a.write("photos/new", "new");
    a.synced();

    // A copy is at that path, and merging it can upset everyone else: it has to be asked for.
    let refused = a.cairn(&["include", "photos"]);
    assert_eq!(refused.code, Some(1), "{}", refused.text());
    assert_eq!(a.read("photos/edited"), b"local");
    let run = a.cairn(&["include", "photos", "--merge"]).expect_ok();
    assert_eq!(a.read("photos/same"), b"same", "{}", run.text());
    assert_eq!(a.read("photos/edited"), b"server");
    let copies = a.conflict_copies("photos", "edited");
    assert_eq!(copies.len(), 1, "{:?}", Picture::of(&a.root).names());
    let copy = format!("photos/{}", copies[0]);
    assert_eq!(a.read(&copy), b"local");
    b.synced();
    assert_eq!(b.read("photos/new"), b"new");
    assert_eq!(b.read(&copy), b"local");
    assert_eq!(b.read("photos/edited"), b"server");
}

#[test]
fn a_new_device_can_exclude_before_it_downloads_anything() {
    let (_sb, _s, a, b) = pair("sel-new");
    b.write("huge/file", "h");
    b.write("wanted", "w");
    b.synced();
    a.cairn(&["exclude", "huge"]).expect_ok();
    assert!(!a.exists("huge"));
    assert_eq!(a.read("wanted"), b"w");
}

#[test]
fn remove_local_takes_only_what_the_server_has() {
    let (_sb, _s, a, b) = pair("sel-remove");
    rules(&a, "*.log\n");
    b.write("photos/same", "same");
    b.write("photos/sub/same", "same");
    b.synced();
    a.synced();
    a.write("photos/only-here.log", "ignored, so it has no other copy");

    let run = a
        .cairn(&["exclude", "photos", "--remove-local"])
        .expect_ok();
    assert!(!a.exists("photos/same"), "{}", run.text());
    assert!(!a.exists("photos/sub"), "{}", run.text());
    assert_eq!(
        a.read("photos/only-here.log"),
        b"ignored, so it has no other copy"
    );
    b.synced();
    assert_eq!(b.read("photos/same"), b"same");
    assert_eq!(b.read("photos/sub/same"), b"same");
    assert!(!b.exists("photos/only-here.log"));

    // Settled: another sync finds nothing to do.
    let run = a.synced();
    assert!(
        run.stdout.contains(" 0 steps applied here, 0 commits"),
        "{}",
        run.text()
    );
}

/// `exclude --remove-local` racing an edit made after its first sync: killed just before its last
/// sync does anything, which leaves exactly the state in between.
#[test]
fn remove_local_keeps_an_edit_it_had_not_seen() {
    let (_sb, _s, a, b) = pair("sel-remove-edit");
    b.write("photos/edited", "server");
    b.write("photos/same", "same");
    b.synced();
    a.synced();

    let run = a.cairn_env(
        &["exclude", "photos", "--remove-local"],
        &[("CAIRN_FP_CRASH", "selection.excluded")],
    );
    assert!(run.was_killed(), "{}", run.text());
    a.write("photos/edited", "edited here");
    let run = a.synced();
    assert!(!a.exists("photos/same"), "{}", run.text());
    assert!(run.stdout.contains("synced at sequence"), "{}", run.text());
    // The server's folder holds that name, out of view here, so the edit sits beside it.
    let copies = a.conflict_copies("", "photos");
    assert_eq!(copies.len(), 1, "{:?}", Picture::of(&a.root).names());
    let edit = format!("{}/edited", copies[0]);
    assert_eq!(a.read(&edit), b"edited here", "{}", run.text());
    b.synced();
    assert_eq!(b.read("photos/edited"), b"server");
    assert_eq!(b.read("photos/same"), b"same");
    assert_eq!(b.read(&edit), b"edited here");
}

#[test]
fn only_what_was_excluded_can_be_included() {
    let (_sb, _s, a, b) = pair("sel-only");
    b.write("plain", "p");
    b.synced();
    a.synced();
    assert_eq!(a.cairn(&["include", "plain"]).code, Some(1));
    assert_eq!(a.cairn(&["exclude", "no-such"]).code, Some(1));
    assert_eq!(a.cairn(&["exclude", "."]).code, Some(1));
}

/// One sync with rules to honour, killed at one step, then the rules lifted. Whether it was killed.
fn kill_around_a_rule(step: u64) -> bool {
    let sb = Sandbox::fast("sel-kill");
    let mut server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    b.write("proj/hidden.log", "from b");
    b.write("proj/moved", "from b");
    b.write("proj/plain", "from b");
    b.write("gone/inner", "from b");
    b.synced();
    rules(&a, "*.log\nattic/\n");
    a.write("proj/here.log", "only here");
    a.write("gone/here.log", "only here too");
    a.synced();
    a.write("proj/new", "from a");
    std::fs::create_dir(b.path("attic")).unwrap();
    std::fs::rename(b.path("proj/moved"), b.path("attic/moved")).unwrap();
    std::fs::remove_dir_all(b.path("gone")).unwrap();
    b.synced();

    let run = a.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", &step.to_string())]);
    let killed = run.was_killed();
    // However far it got.
    a.synced();
    assert_eq!(a.read("gone/here.log"), b"only here too", "step {step}");
    assert!(!a.exists("attic"), "step {step}");
    rules(&a, "");
    a.synced();
    b.synced();
    a.synced();
    for (d, path, want) in [
        (&a, "proj/here.log", &b"only here"[..]),
        (&b, "proj/here.log", b"only here"),
        (&a, "proj/hidden.log", b"from b"),
        (&b, "proj/hidden.log", b"from b"),
        (&a, "attic/moved", b"from b"),
        (&b, "proj/new", b"from a"),
        (&b, "gone/here.log", b"only here too"),
    ] {
        assert_eq!(d.read(path), want, "step {step}: {path} on {}", d.name);
    }
    assert!(leftover_temps(&a.root).is_empty(), "step {step}");
    let report = server.fsck();
    assert!(report.contains("clean"), "step {step}: {report}");
    killed
}

fn a_kill_at_any_step_around_a_rule_loses_nothing(shard: Shard) {
    for step in each_step_to_the_end(shard) {
        if !kill_around_a_rule(step) {
            assert!(step > 20, "the sync was never killed");
            return;
        }
    }
}

shards!(a_kill_at_any_step_around_a_rule_loses_nothing: s01 s02 s03 s04 s05 s06 s07 s08 s09 s10
    s11 s12);

/// A copy moved onto a held name became that file, and the record of the copy it once was must not
/// turn a later rename into a delete for everyone, the file's own or its folder's.
#[test]
fn a_copy_moved_onto_a_held_name_then_renamed_is_only_a_rename() {
    for rename_folder in [false, true] {
        let (_sb, _s, a, b) = pair("sel-copy-held-renamed");
        a.write("ex", "from before");
        a.write("p/held", "shared");
        for i in 0..12 {
            a.write(&format!("plain{i}"), "p");
        }
        a.synced();
        b.synced();
        b.cairn(&["exclude", "ex"]).expect_ok();
        b.write("ex", "newer notes");
        std::fs::rename(b.path("ex"), b.path("p/held")).unwrap();
        b.synced();
        a.synced();
        assert_eq!(a.read("p/held"), b"newer notes");

        let now_at = if rename_folder {
            std::fs::rename(b.path("p"), b.path("q")).unwrap();
            "q/held"
        } else {
            std::fs::rename(b.path("p/held"), b.path("p/renamed")).unwrap();
            "p/renamed"
        };
        b.synced();
        a.synced();
        assert_eq!(
            a.read(now_at),
            b"newer notes",
            "renamed folder: {rename_folder}"
        );
        assert_eq!(
            b.read(now_at),
            b"newer notes",
            "renamed folder: {rename_folder}"
        );
    }
}

/// Excluded, the folder is not looked at, so what was moved into it would read as deleted.
#[test]
fn excluding_a_folder_with_something_known_moved_into_it_is_refused() {
    let (_sb, _s, a, b) = pair("sel-moved-in");
    a.write("k/deep/inner", "inner");
    a.write("outside", "precious");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    std::fs::rename(b.path("outside"), b.path("k/deep/outside")).unwrap();
    let run = b.cairn(&["exclude", "k"]);
    assert_eq!(run.code, Some(1), "{}", run.text());

    // Moved back, it can be excluded after all.
    std::fs::rename(b.path("k/deep/outside"), b.path("outside")).unwrap();
    b.cairn(&["exclude", "k"]).expect_ok();
    b.synced();
    a.synced();
    assert_eq!(a.read("outside"), b"precious");
    assert_eq!(a.read("k/deep/inner"), b"inner");
}

#[test]
fn excluding_a_path_twice_takes_one_include_to_undo() {
    let (_sb, _s, a, b) = pair("sel-twice");
    a.write("k/f", "one");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.synced();
    b.synced();
    b.cairn(&["exclude", "k"]).expect_ok();
    std::fs::remove_dir_all(a.path("k")).unwrap();
    a.synced();
    a.write("k/g", "two");
    a.synced();
    // The server has a new folder there, and the copy here is in its way.
    assert_eq!(b.sync().code, Some(3));
    b.cairn(&["exclude", "k"]).expect_ok();
    b.cairn(&["include", "--merge", "k"]).expect_ok();
    b.synced();
    a.synced();
    assert_eq!(b.read("k/g"), b"two");
    assert_eq!(a.read("k/f"), b"one", "the copy merged in");
}

/// A rule names a path, and a sync is about to move the folder away from it. Nobody here chose that,
/// so what the rule covers stays here, through the merge's own rename and another device's.
#[test]
fn a_folder_moved_by_a_sync_keeps_what_its_rule_left_out() {
    for merge_renames_it in [true, false] {
        let (sb, server, a, b) = pair("sel-rule-moved");
        for i in 0..12 {
            a.write(&format!("plain{i}"), "p");
        }
        if !merge_renames_it {
            a.write("secret/shared", "s");
        }
        a.synced();
        b.synced();
        rules(&b, "/secret/key\n");
        b.write("secret/key", "TOP-SECRET");
        b.synced();
        if merge_renames_it {
            a.write("secret", "not a folder");
        } else {
            std::fs::rename(a.path("secret"), a.path("renamed")).unwrap();
        }
        a.synced();
        let run = b.sync();
        assert!(run.stdout.contains("stays out"), "{}", run.text());
        b.synced();
        b.synced();
        a.synced();
        let there = on_server(&sb, &server, "probe");
        assert!(!there.iter().any(|n| n.ends_with("/key")), "{there:?}");

        // It is still here, and it is listed, and a person can bring it in.
        let moved = if merge_renames_it {
            b.conflict_copies("", "secret")[0].clone() + "/key"
        } else {
            "renamed/key".to_string()
        };
        assert_eq!(b.read(&moved), b"TOP-SECRET");
        assert!(b.cairn(&["status"]).stdout.contains(&moved));
        b.cairn(&["include", "--merge", &moved]).expect_ok();
        b.synced();
        a.synced();
        assert_eq!(a.read(&moved), b"TOP-SECRET");
    }
}

/// Dying as the folder has just moved, with the rule already not matching: the mark has to be durable.
#[test]
fn a_kill_as_a_sync_moves_the_folder_does_not_let_what_a_rule_covers_go_up() {
    let (sb, server, a, b) = pair("sel-rule-moved-kill");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.write("secret/shared", "s");
    a.synced();
    b.synced();
    rules(&b, "/secret/key\n");
    b.write("secret/key", "TOP-SECRET");
    b.synced();
    std::fs::rename(a.path("secret"), a.path("renamed")).unwrap();
    a.synced();
    let run = b.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "apply.move.done")]);
    assert!(run.was_killed(), "{}", run.text());
    assert!(b.exists("renamed/key"));
    b.synced();
    let there = on_server(&sb, &server, "probe");
    assert!(!there.contains("renamed/key"), "{there:?}");
    assert_eq!(b.read("renamed/key"), b"TOP-SECRET");
}

/// Two devices move two folders each into the other, and the sync here undoes this device's move. That
/// move had put a synced folder where a rule leaves it out, a delete of it for everyone as any `mv` into
/// an excluded path is, and undoing it takes the folder from under the rule. So it stays out by inode,
/// and this device's record of the folder, from before, must not bring it back into view, where it went
/// the way the server's had: off this disk, with a kept copy for an inode that was gone. A file the same.
#[test]
fn a_folder_a_rule_leaves_out_stays_here_as_a_cycle_of_moves_is_undone() {
    let (sb, server, a, b) = pair("sel-rule-cycle");
    for i in 0..12 {
        a.write(&format!("fill/{i}"), "p");
    }
    a.write("d/f2", "beside");
    a.write("fill/x/y/cache/data", "cached");
    a.write("fill/x/y/cache/z/deeper", "deeper");
    a.write("fill/x/y/note", "a file");
    a.write("fill/x/y/kept", "in view");
    a.synced();
    b.synced();
    rules(&b, "/d/fill/x/y/cache/\n/d/fill/x/y/note\n");
    std::fs::rename(a.path("d"), a.path("fill/d")).unwrap();
    a.synced();
    std::fs::rename(b.path("fill"), b.path("d/fill")).unwrap();
    b.write("d/fill/x/y/cache/data", "edited here");
    let run = b.synced();
    assert!(
        run.stdout.contains("cycle")
            && run
                .stdout
                .contains("d/fill/x/y/cache: its folder is moving"),
        "{}",
        run.text()
    );
    assert_eq!(b.read("fill/x/y/cache/data"), b"edited here");
    assert_eq!(b.read("fill/x/y/cache/z/deeper"), b"deeper");
    assert_eq!(b.read("fill/x/y/note"), b"a file");
    assert_eq!(b.read("fill/d/f2"), b"beside");
    let status = b.cairn(&["status"]);
    assert!(
        status
            .stdout
            .contains("left here when it was excluded: fill/x/y/cache;")
            && status
                .stdout
                .contains("left here when it was excluded: fill/x/y/note;")
            && !status.stdout.contains("d/fill/x/y/"),
        "{}",
        status.text()
    );

    // Nothing comes or goes for it later, and nothing of it went up, the edit least of all.
    let run = b.synced();
    assert!(
        run.stdout.contains(" 0 steps applied here, 0 commits"),
        "{}",
        run.text()
    );
    a.synced();
    let there = on_server(&sb, &server, "probe");
    assert!(
        !there
            .iter()
            .any(|n| n.contains("cache") || n.contains("note")),
        "{there:?}"
    );
    assert!(
        there.contains("fill/x/y/kept") && there.contains("fill/d/f2"),
        "{there:?}"
    );
    assert_eq!(b.read("fill/x/y/cache/data"), b"edited here");

    // A person can bring it in.
    b.cairn(&["include", "--merge", "fill/x/y/cache"])
        .expect_ok();
    b.cairn(&["include", "--merge", "fill/x/y/note"])
        .expect_ok();
    b.synced();
    a.synced();
    assert_eq!(a.read("fill/x/y/cache/data"), b"edited here");
    assert_eq!(a.read("fill/x/y/note"), b"a file");
    assert_eq!(a.read("fill/x/y/cache/z/deeper"), b"deeper");
}

/// The same where the other device has added to the folder meanwhile: the merge keeps the server's for
/// that, and the copy here is in its way. Each sync says so and what to do, and the copy stays, until a
/// person brings the two together.
#[test]
fn a_folder_kept_out_as_a_cycle_of_moves_is_undone_waits_for_what_the_server_added_to_it() {
    let (_sb, _s, a, b) = pair("sel-rule-cycle-added");
    for i in 0..12 {
        a.write(&format!("fill/{i}"), "p");
    }
    a.write("d/f2", "beside");
    a.write("fill/x/y/cache/data", "cached");
    a.synced();
    b.synced();
    rules(&b, "/d/fill/x/y/cache/\n");
    std::fs::rename(a.path("d"), a.path("fill/d")).unwrap();
    a.write("fill/x/y/cache/new", "from a");
    a.synced();
    std::fs::rename(b.path("fill"), b.path("d/fill")).unwrap();
    for _ in 0..2 {
        let run = b.sync();
        assert_eq!(run.code, Some(3), "{}", run.text());
        assert!(
            run.text()
                .contains("fill/x/y/cache: something this device excludes is at that name")
                && run.text().contains("`cairn-sync include --merge`"),
            "{}",
            run.text()
        );
        assert_eq!(b.read("fill/x/y/cache/data"), b"cached");
    }
    b.cairn(&["include", "--merge", "fill/x/y/cache"])
        .expect_ok();
    b.synced();
    a.synced();
    for d in [&a, &b] {
        assert_eq!(d.read("fill/x/y/cache/data"), b"cached");
        assert_eq!(d.read("fill/x/y/cache/new"), b"from a");
    }
}

/// The same for a file with another name in view: by inode, that name would go out of view too, and read
/// as deleted for everyone. It cannot stay out, and comes back in as it was.
#[test]
fn a_file_a_rule_leaves_out_with_another_name_comes_back_as_a_cycle_of_moves_is_undone() {
    let (_sb, _s, a, b) = pair("sel-rule-cycle-link");
    for i in 0..12 {
        a.write(&format!("fill/{i}"), "p");
    }
    a.write("d/f2", "beside");
    a.write("fill/x/y/secret", "linked");
    std::fs::hard_link(a.path("fill/x/y/secret"), a.path("fill/other")).unwrap();
    a.synced();
    b.synced();
    rules(&b, "/d/fill/x/y/secret\n");
    std::fs::rename(a.path("d"), a.path("fill/d")).unwrap();
    a.synced();
    std::fs::rename(b.path("fill"), b.path("d/fill")).unwrap();
    let run = b.synced();
    assert!(
        run.stdout.contains("cycle") && !run.stdout.contains("stays out"),
        "{}",
        run.text()
    );
    b.synced();
    a.synced();
    for d in [&a, &b] {
        assert_eq!(d.read("fill/other"), b"linked");
        assert_eq!(d.read("fill/x/y/secret"), b"linked");
        assert_eq!(nlink(&d.path("fill/other")), 2);
    }
}

/// A rule that still matches where the folder goes needs no record, and lifting it brings the file in.
#[test]
fn a_folder_moved_by_a_sync_leaves_a_rule_that_still_matches_as_it_was() {
    let (_sb, _s, a, b) = pair("sel-rule-still");
    for i in 0..12 {
        a.write(&format!("plain{i}"), "p");
    }
    a.write("d/shared", "s");
    a.synced();
    b.synced();
    rules(&b, "*.log\n");
    b.write("d/run.log", "log");
    b.synced();
    std::fs::rename(a.path("d"), a.path("e")).unwrap();
    a.synced();
    let run = b.synced();
    assert!(!run.stdout.contains("stays out"), "{}", run.text());
    rules(&b, "");
    b.synced();
    a.synced();
    assert_eq!(a.read("e/run.log"), b"log");
}

/// A name the filesystem here cannot hold (APFS refuses one that is not UTF-8, a case-insensitive disk
/// a case twin) is out of view on this device: reported each time, retried by nothing, and no reason
/// to call the sync partial, which would hold every later change back. Nothing goes from the server.
#[test]
fn a_name_this_filesystem_refuses_is_left_out_and_holds_nothing_back() {
    let (sb, server, a, b) = pair("refused");
    let refuse = [("CAIRN_FP_REFUSE_NAME", "refused")];
    a.write("d/refused", "kept on the server\n");
    a.write("d/fine", "fine\n");
    a.write("refused/inner/deep", "under a folder\n");
    a.synced();

    for _ in 0..2 {
        let run = b.sync_env(&refuse);
        assert!(
            run.ok(),
            "a name left out is no partial sync: {}",
            run.text()
        );
        let said = run
            .text()
            .matches("this filesystem cannot hold this name")
            .count();
        assert_eq!(
            said,
            2,
            "the file and the folder, nothing under it: {}",
            run.text()
        );
    }
    assert_eq!(b.read("d/fine"), b"fine\n");
    assert!(!b.exists("d/refused") && !b.exists("refused"));

    // Nothing reads as deleted, here or anywhere.
    b.write("d/new", "new\n");
    assert!(b.sync_env(&refuse).ok());
    a.synced();
    assert_eq!(a.read("d/refused"), b"kept on the server\n");
    assert_eq!(a.read("refused/inner/deep"), b"under a folder\n");
    assert_eq!(a.read("d/new"), b"new\n");

    // Renamed elsewhere, it comes down as anything new does.
    std::fs::rename(a.path("d/refused"), a.path("d/accepted")).unwrap();
    a.synced();
    let run = b.sync_env(&refuse);
    assert!(run.ok(), "{}", run.text());
    assert_eq!(b.read("d/accepted"), b"kept on the server\n");
    assert_eq!(
        run.text()
            .matches("this filesystem cannot hold this name")
            .count(),
        1,
        "{}",
        run.text()
    );
    let names = on_server(&sb, &server, "probe");
    assert!(
        names.contains("refused/inner/deep") && names.contains("d/accepted"),
        "{names:?}"
    );
}
