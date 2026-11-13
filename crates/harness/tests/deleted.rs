//! Recently deleted: `cairn-sync deleted` lists what is deleted and still deleted, from the server's history,
//! a folder once with what was deleted with it; `cairn-sync undelete` brings it back as it was when deleted,
//! as a new commit that reaches every device, replacing nothing. A kill leaves nothing or all of it.
mod common;
use common::*;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

/// ENOBUFS, as the network tests fail a request with.
const ENOBUFS: i32 = if cfg!(target_os = "macos") {
    55
} else if cfg!(windows) {
    10055
} else {
    105
};

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[track_caller]
fn listed(d: &Device) -> Vec<serde_json::Value> {
    let run = d.cairn(&["deleted", "--json"]).expect_ok();
    run.stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

#[track_caller]
fn only(list: &[serde_json::Value], path: &str) -> serde_json::Value {
    let found: Vec<_> = list.iter().filter(|d| d["path"] == path).collect();
    assert_eq!(found.len(), 1, "{path} in {list:#?}");
    found[0].clone()
}

fn id(entry: &serde_json::Value) -> String {
    entry["id"].as_str().unwrap().to_string()
}

/// The journal's last sequence.
fn head(d: &Device) -> u64 {
    let run = d.cairn(&["log", "-n", "1"]).expect_ok();
    run.stdout
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn a_deleted_folder_is_one_entry_and_comes_back_whole_on_every_device() {
    let (_sb, _s, a, b) = pair("deleted-folder");
    a.write("d/one", "1");
    a.write("d/sub/two", "22");
    a.write("keep", "k");
    a.synced();
    b.synced();
    let before = now();
    fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    let after = now();

    // The server's history, so the other device lists it the same.
    let list = listed(&b);
    assert_eq!(list.len(), 1, "{list:#?}");
    let d = only(&list, "d");
    assert_eq!(d["kind"], "dir");
    assert_eq!(d["files"], 2);
    assert_eq!(d["bytes"], 3);
    assert_eq!(d["device_name"], "a", "the device that deleted it");
    assert_eq!(d["folder_gone"], false);
    let time = d["time"].as_i64().unwrap();
    assert!(
        before - 1 <= time && time <= after + 1,
        "{time} not in {before}..{after}"
    );
    assert_eq!(listed(&a), list);
    let plain = b.cairn(&["deleted"]).expect_ok();
    assert!(
        plain.stdout.contains("2 files, 3 bytes  d/") && plain.stdout.contains(&id(&d)),
        "{}",
        plain.text()
    );

    b.synced();
    assert!(!b.exists("d"));
    let run = b.cairn(&["undelete", &id(&d)]).expect_ok();
    assert!(
        run.stdout.contains("brought back d (4 object(s))"),
        "{}",
        run.text()
    );
    assert_eq!(b.read("d/one"), b"1");
    assert_eq!(b.read("d/sub/two"), b"22");
    a.synced();
    assert_eq!(a.read("d/sub/two"), b"22");
    assert!(listed(&a).is_empty());
    converge(&[&a, &b]);

    // Asked again, it says so and changes nothing.
    let at = head(&a);
    let run = a.cairn(&["undelete", &id(&d)]).expect_ok();
    assert!(run.stdout.contains("d is not deleted"), "{}", run.text());
    assert_eq!(head(&a), at);
}

/// A sync cuts a large delete into several commits, with nothing to tie them together but the device
/// and the moment: still one entry, and all of it comes back.
#[test]
fn a_folder_deleted_over_several_commits_comes_back_whole() {
    let (_sb, _s, a, b) = pair("deleted-commits");
    // Not all of the tree, which a sync would take for a disk not mounted.
    a.write("keep", "k");
    for i in 0..6 {
        a.write(&format!("d/f{i}"), format!("file {i}"));
    }
    a.write("d/sub/g", "g");
    a.synced();
    let start = head(&a);
    b.synced();
    fs::remove_dir_all(a.path("d")).unwrap();
    a.sync_env(&[("CAIRN_FP_TUNE", "sync.ops_a_commit=2")])
        .expect_ok();
    assert!(head(&a) >= start + 3, "the delete took several commits");

    let list = listed(&b);
    assert_eq!(list.len(), 1, "{list:#?}");
    let d = only(&list, "d");
    assert_eq!(d["files"], 7);
    b.cairn(&["undelete", &id(&d)]).expect_ok();
    for i in 0..6 {
        assert_eq!(b.read(&format!("d/f{i}")), format!("file {i}").as_bytes());
    }
    assert_eq!(b.read("d/sub/g"), b"g");
    a.synced();
    converge(&[&a, &b]);
}

#[test]
fn an_undelete_onto_a_name_taken_since_keeps_both() {
    let (_sb, _s, a, b) = pair("deleted-taken");
    // Not all of the tree, which a sync would take for a disk not mounted.
    a.write("keep", "k");
    a.write("n.txt", "the old one");
    a.synced();
    fs::remove_file(a.path("n.txt")).unwrap();
    a.synced();
    a.write("n.txt", "a new one");
    a.synced();

    let list = listed(&a);
    let n = only(&list, "n.txt");
    assert_eq!(n["kind"], "file");
    assert_eq!(n["bytes"], 11);
    let run = a.cairn(&["undelete", &id(&n)]).expect_ok();
    assert!(
        run.stdout
            .contains("brought back n.txt as n (conflict restored seq "),
        "{}",
        run.text()
    );
    assert_eq!(a.read("n.txt"), b"a new one", "what is there stays");
    let names = Picture::of(&a.root).names();
    let back: Vec<_> = names
        .iter()
        .filter(|n| n.starts_with("n (conflict restored seq "))
        .collect();
    assert_eq!(back.len(), 1, "{names:?}");
    assert_eq!(a.read(back[0]), b"the old one");
    b.synced();
    assert_eq!(b.read(back[0]), b"the old one");
    assert_eq!(b.read("n.txt"), b"a new one");
    converge(&[&a, &b]);
}

/// Deleted by two devices at two times, a file and then its folder are two entries; the file brings
/// the folder back holding only itself, and what the folder's own delete took is then listed in it.
#[test]
fn a_file_whose_folder_is_deleted_too_brings_the_folder_back_with_only_itself() {
    let (_sb, _s, a, b) = pair("deleted-parent");
    // Not all of the tree, which a sync would take for a disk not mounted.
    a.write("keep", "k");
    a.write("d/f", "f");
    a.write("d/g", "g");
    a.synced();
    b.synced();
    fs::remove_file(b.path("d/f")).unwrap();
    b.synced();
    a.synced();
    fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();

    let list = listed(&a);
    assert_eq!(list.len(), 2, "{list:#?}");
    assert_eq!(only(&list, "d")["files"], 1);
    assert_eq!(only(&list, "d")["device_name"], "a");
    let f = only(&list, "d/f");
    assert_eq!(f["device_name"], "b");
    assert_eq!(f["folder_gone"], true);
    assert_eq!(list[0]["path"], "d", "newest first");

    a.cairn(&["undelete", &id(&f)]).expect_ok();
    assert_eq!(a.read("d/f"), b"f");
    assert!(
        !a.exists("d/g"),
        "the folder came back holding only what was asked for"
    );
    let list = listed(&a);
    let g = only(&list, "d/g");
    assert_eq!(g["folder_gone"], false);
    a.cairn(&["undelete", &id(&g)]).expect_ok();
    assert_eq!(a.read("d/g"), b"g");
    b.synced();
    converge(&[&a, &b]);
}

#[test]
fn an_undelete_killed_before_its_commit_brings_nothing_and_after_it_everything() {
    let (sb, server, a, b) = pair("deleted-killed");
    // Not all of the tree, which a sync would take for a disk not mounted.
    a.write("keep", "k");
    a.write("d/x", "x");
    a.write("d/y", "y");
    a.synced();
    b.synced();
    fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    b.synced();
    let d = only(&listed(&a), "d");
    let at = head(&a);

    let run = a.cairn_env(
        &["undelete", &id(&d)],
        &[("CAIRN_FP_CRASH", "undelete.planned")],
    );
    assert!(run.was_killed(), "{}", run.text());
    assert_eq!(head(&a), at, "nothing went to the server");
    assert_eq!(listed(&a).len(), 1);

    let run = a.cairn_env(
        &["undelete", &id(&d)],
        &[("CAIRN_FP_CRASH", "undelete.committed")],
    );
    assert!(run.was_killed(), "{}", run.text());
    assert_eq!(head(&a), at + 1, "all of it in one commit");
    assert!(listed(&a).is_empty());
    assert!(!a.exists("d"), "killed before its sync");
    a.synced();
    assert_eq!(a.read("d/x"), b"x");
    // Killed again while the other device takes it in: its next sync finishes it.
    let run = b.sync_env(&[("CAIRN_FP_CRASH", "apply.create.renamed")]);
    assert!(run.was_killed(), "{}", run.text());
    b.synced();
    assert_eq!(b.read("d/y"), b"y");
    converge(&[&a, &b]);

    // The commit's answer lost on the way back: tried again, it is the same commit, once.
    fs::remove_dir_all(a.path("d")).unwrap();
    a.synced();
    let d = only(&listed(&a), "d");
    let at = head(&a);
    let spec = format!("client.commit:{ENOBUFS}:1:lost");
    let run = a.cairn_env(
        &["undelete", "--no-sync", &id(&d)],
        &[("CAIRN_FP_NET", &spec)],
    );
    assert!(run.ok(), "{}", run.text());
    assert_eq!(head(&a), at + 1, "{}", run.text());
    a.synced();
    b.synced();
    assert_eq!(b.read("d/x"), b"x");
    converge(&[&a, &b]);
    drop((sb, server));
}

#[test]
fn an_id_that_names_nothing_deleted_is_refused_and_changes_nothing() {
    let (_sb, _s, a, _b) = pair("deleted-bad-id");
    // Not all of the tree, which a sync would take for a disk not mounted.
    a.write("keep", "k");
    a.write("f", "f");
    a.synced();
    let at = head(&a);
    let run = a.cairn(&["undelete", "not-an-id"]);
    assert!(
        !run.ok() && run.stderr.contains("not an ID"),
        "{}",
        run.text()
    );
    let run = a.cairn(&["undelete", "0123456789abcdef0123456789abcde"]);
    assert!(
        !run.ok() && run.stderr.contains("not an ID"),
        "{}",
        run.text()
    );
    let run = a.cairn(&["undelete", "0123456789abcdef0123456789abcdef"]);
    assert!(
        !run.ok() && run.stderr.contains("is not in this root's history"),
        "{}",
        run.text()
    );
    assert_eq!(head(&a), at);
}
