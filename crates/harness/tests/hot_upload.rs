//! A writer cannot hold the rest of a push back, including operations that share its chunks.
mod common;
use common::*;
use std::time::{Duration, Instant};

fn pair(label: &str) -> (Sandbox, Server, Device, Device) {
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    (sb, server, a, b)
}

#[cfg(unix)]
const HOT: &str = "hot\nfile";
#[cfg(not(unix))]
const HOT: &str = "hot-file";
fn shown_hot() -> String {
    HOT.replace('\n', "\\n")
}

fn while_writing(sb: &Sandbox, a: &Device, hot: &str, env: &[(&str, &str)]) -> Run {
    let gate = sb.path("upload-gate");
    std::fs::create_dir(&gate).unwrap();
    let pause = format!("sync.upload.begin#each:{}", gate.display());
    let device = a.clone();
    let extra: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let worker = std::thread::spawn(move || {
        let mut env: Vec<_> = extra
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        env.push(("CAIRN_FP_PAUSE", &pause));
        device.sync_env(&env)
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut saves = 0;
    while !worker.is_finished() {
        assert!(Instant::now() < deadline, "upload did not finish");
        if gate.join("reached").exists() {
            saves += 1;
            a.write(hot, format!("writer's version {saves}"));
            // Stable by the next scan, but different from this round's scanned bytes.
            std::thread::sleep(Duration::from_millis(300));
            std::fs::remove_file(gate.join("reached")).unwrap();
            std::fs::write(gate.join("go"), b"").unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(saves >= 10, "only {saves} uploads tried");
    let run = worker.join().unwrap();
    assert!(
        run.text()
            .contains("the tree kept changing for 12 rounds, last at"),
        "{}",
        run.text()
    );
    assert!(!run.ok(), "{}", run.text());
    run
}

#[test]
fn thousands_of_files_land_while_one_changes_in_every_upload_and_status_names_it() {
    let (sb, mut server, a, b) = pair("hot-thousands");
    for i in 0..2400 {
        a.write(&format!("files/{i:04}"), format!("stable file {i}"));
    }
    a.write(HOT, "scanned version");
    #[cfg(unix)]
    std::fs::hard_link(a.path(HOT), a.path("other-name")).unwrap();
    let run = while_writing(&sb, &a, HOT, &[]);
    assert!(run.text().contains(&shown_hot()), "{}", run.text());
    assert!(!run.text().contains("refused a commit"), "{}", run.text());
    b.synced();
    for i in 0..2400 {
        assert_eq!(
            b.read(&format!("files/{i:04}")),
            format!("stable file {i}").as_bytes()
        );
    }
    assert!(!b.exists(HOT));
    assert!(!b.exists("other-name"));
    let status = a.cairn(&["status"]);
    assert!(
        status.text().contains("changed during upload"),
        "{}",
        status.text()
    );
    assert!(status.text().contains(&shown_hot()), "{}", status.text());
    // A paused run also keeps the report: only an agreement clears it.
    std::fs::rename(a.path("files"), sb.path("files-aside")).unwrap();
    let paused = a.sync();
    assert!(paused.text().contains("paused:"), "{}", paused.text());
    assert!(
        paused.text().contains("changed during upload"),
        "{}",
        paused.text()
    );
    std::fs::rename(sb.path("files-aside"), a.path("files")).unwrap();
    a.synced();
    b.synced();
    assert_eq!(b.read(HOT), a.read(HOT));
    #[cfg(unix)]
    assert_eq!(inode(&b.path(HOT)), inode(&b.path("other-name")));
    assert!(!a
        .cairn(&["status"])
        .text()
        .contains("changed during upload"));
    assert!(server.fsck().contains("clean"));
}

#[test]
fn a_held_move_holds_a_replacement_directory_and_its_children_but_not_its_neighbours() {
    let (sb, mut server, a, b) = pair("hot-dependencies");
    a.write("old", "old bytes");
    a.write("arriving", "move me");
    a.synced();
    b.synced();
    std::fs::create_dir(a.path("new")).unwrap();
    std::fs::rename(a.path("old"), a.path("new/hot")).unwrap();
    a.write("new/hot", "new bytes");
    a.write("old/child", "new child");
    std::fs::rename(a.path("arriving"), a.path("old/arriving")).unwrap();
    a.write("neighbour", "must arrive now");
    let run = while_writing(&sb, &a, "new/hot", &[]);
    assert!(!run.text().contains("refused a commit"), "{}", run.text());
    b.synced();
    assert_eq!(b.read("old"), b"old bytes");
    assert_eq!(b.read("arriving"), b"move me");
    assert_eq!(b.read("neighbour"), b"must arrive now");
    a.synced();
    b.synced();
    assert_eq!(b.read("new/hot"), a.read("new/hot"));
    assert_eq!(b.read("old/child"), b"new child");
    assert_eq!(b.read("old/arriving"), b"move me");
    assert!(server.fsck().contains("clean"));
}

#[test]
fn a_held_swap_leaves_no_staging_name_on_the_server() {
    let (sb, mut server, a, b) = pair("hot-swap");
    a.write("left", "left bytes");
    a.write("right", "right bytes");
    a.synced();
    b.synced();
    std::fs::rename(a.path("left"), a.path("tmp")).unwrap();
    std::fs::rename(a.path("right"), a.path("left")).unwrap();
    std::fs::rename(a.path("tmp"), a.path("right")).unwrap();
    a.write("right", "changed swapped file");
    a.write("neighbour", "independent");
    let run = while_writing(&sb, &a, "right", &[]);
    assert!(!run.text().contains("refused a commit"), "{}", run.text());
    b.synced();
    assert_eq!(b.read("left"), b"left bytes");
    assert_eq!(b.read("right"), b"right bytes");
    assert_eq!(b.read("neighbour"), b"independent");
    assert_eq!(
        Picture::of(&b.root).names(),
        ["", "left", "neighbour", "right"]
    );
    a.synced();
    b.synced();
    assert_eq!(b.read("right"), a.read("right"));
    assert_eq!(b.read("left"), b"right bytes");
    assert!(server.fsck().contains("clean"));
}

#[test]
fn an_unchanged_twin_supplies_a_chunk_after_the_preferred_source_changes() {
    let (sb, mut server, a, b) = pair("hot-shared");
    a.write("one", "one before");
    a.write("two", "two before");
    a.synced();
    b.synced();
    let state = a.path(".cairn/state.sqlite");
    let first = objects_named(&state, b"one")[0].clone();
    let second = objects_named(&state, b"two")[0].clone();
    let (hot, twin) = if first < second {
        ("one", "two")
    } else {
        ("two", "one")
    };
    a.write(hot, "shared new content");
    a.write(twin, "shared new content");
    while_writing(&sb, &a, hot, &[]);
    b.synced();
    assert_eq!(b.read(twin), b"shared new content");
    a.synced();
    b.synced();
    assert_eq!(b.read(hot), a.read(hot));
    assert!(server.fsck().contains("clean"));
}

#[test]
fn a_deferred_creation_also_removes_links_in_later_commits() {
    let (sb, mut server, a, b) = pair("hot-many-commits");
    a.write("hot", "scanned");
    #[cfg(unix)]
    for i in 0..3 {
        std::fs::hard_link(a.path("hot"), a.path(&format!("link{i}"))).unwrap();
    }
    for i in 0..12 {
        a.write(&format!("cold{i}"), format!("cold {i}"));
    }
    let run = while_writing(&sb, &a, "hot", &[("CAIRN_FP_TUNE", "sync.ops_a_commit=1")]);
    assert!(!run.text().contains("refused a commit"), "{}", run.text());
    b.synced();
    for i in 0..12 {
        assert_eq!(b.read(&format!("cold{i}")), format!("cold {i}").as_bytes());
    }
    assert!(!b.exists("hot"));
    for i in 0..3 {
        assert!(!b.exists(&format!("link{i}")));
    }
    a.synced();
    b.synced();
    assert_eq!(a.read("hot"), b.read("hot"));
    #[cfg(unix)]
    for i in 0..3 {
        assert_eq!(inode(&b.path("hot")), inode(&b.path(&format!("link{i}"))));
    }
    assert!(server.fsck().contains("clean"));
}

// Mode conflicts are Unix metadata; Windows does not preserve these distinct permission bits.
#[cfg(unix)]
#[test]
fn notes_after_a_deferred_note_are_delivered_once_and_the_deferred_note_is_not_lost() {
    let (sb, mut server, a, b) = pair("hot-notes");
    for name in ["one", "two", "three"] {
        a.write(name, name);
        chmod(&a.path(name), 0o644);
    }
    a.synced();
    b.synced();
    let state = a.path(".cairn/state.sqlite");
    let hot = ["one", "two", "three"]
        .into_iter()
        .min_by_key(|n| objects_named(&state, n.as_bytes())[0].clone())
        .unwrap();
    for name in ["one", "two", "three"] {
        chmod(&b.path(name), 0o600);
        chmod(&a.path(name), 0o640);
    }
    b.synced();
    a.write(hot, "new local bytes");
    let run = while_writing(&sb, &a, hot, &[]);
    assert!(!run.text().contains("refused a commit"), "{}", run.text());
    let (notes, _) = notes_in_journal(&server.data);
    assert_eq!(notes.len(), 2, "{notes:?}");
    a.synced();
    b.synced();
    let (notes, _) = notes_in_journal(&server.data);
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert_eq!(
        notes
            .iter()
            .map(|(id, _)| id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    assert_eq!(b.read(hot), a.read(hot));
    assert!(server.fsck().contains("clean"));
}

#[test]
fn a_kill_after_queueing_or_committing_the_filtered_batch_keeps_the_hot_file_dirty() {
    for kill in ["sync.enqueued", "sync.committed"] {
        let (sb, mut server, a, b) = pair("hot-killed");
        a.write("hot", "scanned");
        a.write("cold", "must survive");
        let gate = sb.path("gate");
        std::fs::create_dir(&gate).unwrap();
        let pause = format!("sync.upload.begin:{}", gate.display());
        let device = a.clone();
        let worker = std::thread::spawn(move || {
            device.sync_env(&[("CAIRN_FP_PAUSE", &pause), ("CAIRN_FP_CRASH", kill)])
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        while !gate.join("reached").exists() {
            assert!(Instant::now() < deadline && !worker.is_finished());
            std::thread::sleep(Duration::from_millis(5));
        }
        a.write("hot", "latest write");
        std::fs::write(gate.join("go"), b"").unwrap();
        assert!(worker.join().unwrap().was_killed());
        let state = a.path(".cairn/state.sqlite");
        if !objects_named(&state, b"hot").is_empty() {
            assert!(row_named(&state, b"hot").synced.is_none());
        }
        a.synced();
        b.synced();
        assert_eq!(b.read("hot"), b"latest write");
        assert_eq!(b.read("cold"), b"must survive");
        assert!(!a
            .cairn(&["status"])
            .text()
            .contains("changed during upload"));
        assert!(server.fsck().contains("clean"));
    }
}
