//! A server's database of schema 3, every body as JSON in its commit and again in `revisions`, moved to
//! schema 4 when a server opens it: every answer the same, byte for byte, a kill at any point of the
//! move or of the compaction after it leaving one schema or the other whole, and the devices going on
//! as before. A real full disk under it is in real_disk_full.rs. Then on to schema 5, whose two tables
//! more a kill or a full disk leave out whole.

mod common;
use common::*;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const FILES: usize = 700;

fn db_of(server: &Server) -> PathBuf {
    server.data.join("meta.sqlite")
}

fn schema(db: &Path) -> i64 {
    rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

/// A GET as the server's first user, the body of a 200.
fn get(server: &Server, path: &str) -> String {
    let token = std::fs::read_to_string(&server.token_file).unwrap();
    let addr = server.url.strip_prefix("http://").unwrap();
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    write!(
        conn,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        token.trim()
    )
    .unwrap();
    let mut raw = Vec::new();
    let _ = conn.read_to_end(&mut raw);
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{path}: {head}");
    let mut body = raw[split + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut rest = &body[..];
        loop {
            let eol = rest.windows(2).position(|w| w == b"\r\n").unwrap();
            let len = usize::from_str_radix(std::str::from_utf8(&rest[..eol]).unwrap().trim(), 16)
                .unwrap();
            rest = &rest[eol + 2..];
            if len == 0 {
                break;
            }
            out.extend_from_slice(&rest[..len]);
            rest = &rest[len + 2..];
        }
        body = out;
    }
    String::from_utf8(body).unwrap()
}

/// Every answer about the tree and its history: the journal whole and in pages, each folder's
/// listing, each object's item and history, conflicts and what is deleted.
fn answers(server: &Server) -> Vec<(String, String)> {
    let ns = "/v1/ns/cloud";
    let mut paths = vec![
        format!("{ns}/changes?after=0"),
        format!("{ns}/changes?after=0&limit=2"),
        format!("{ns}/changes?after=3&limit=4"),
        format!("{ns}/changes?after=0&ops=false"),
        format!("{ns}/conflicts"),
        format!("{ns}/deleted"),
    ];
    let mut dirs = vec!["root".to_string()];
    while let Some(d) = dirs.pop() {
        let listing = get(server, &format!("{ns}/dir/{d}"));
        paths.push(format!("{ns}/dir/{d}"));
        let v: serde_json::Value = serde_json::from_str(&listing).unwrap();
        for e in v["entries"].as_array().unwrap() {
            let id = e["object"].as_str().unwrap().to_string();
            paths.push(format!("{ns}/item/{id}"));
            paths.push(format!("{ns}/history/{id}"));
            if e["kind"] == "dir" {
                dirs.push(id);
            }
        }
    }
    paths
        .into_iter()
        .map(|p| (get(server, &p), p))
        .map(|(a, p)| (p, a))
        .collect()
}

/// Two devices on a tree of every kind of entry, with conflict copies, a note of each device's mode
/// for one file, and deletes; then the server stopped and its database moved back to schema 3.
fn on_schema_3(label: &str) -> (Sandbox, Server, Device, Device, Vec<(String, String)>) {
    let sb = Sandbox::on_disk(label);
    let mut server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..FILES {
        a.write(&format!("d{}/f{i}", i % 9), format!("file {i}"));
    }
    a.write("both", "first");
    a.write("gone", "deleted later");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("d0/f0", a.path("link")).unwrap();
        std::fs::hard_link(a.path("d1/f1"), a.path("hard")).unwrap();
        set_xattr(&a.path("d2/f2"), "user.note", b"\x00\xffbinary");
    }
    a.synced();
    b.synced();
    a.write("both", "edited on a");
    b.write("both", "edited on b");
    #[cfg(unix)]
    {
        chmod(&a.path("d3/f3"), 0o600);
        chmod(&b.path("d3/f3"), 0o640);
    }
    std::fs::remove_file(a.path("gone")).unwrap();
    converge(&[&a, &b]);
    let want = answers(&server);
    assert!(
        want.iter().any(|(_, a)| a.contains(r#""op":"note""#)),
        "no note in the journal"
    );
    assert!(want.len() > FILES, "{} answers", want.len());
    server.kill();
    let data = server.data.to_str().unwrap();
    run_server(&["downgrade", "--data", data], &[]).expect_ok();
    assert_eq!(schema(&db_of(&server)), 4);
    run_server(&["downgrade", "--data", data], &[]).expect_ok();
    assert_eq!(schema(&db_of(&server)), 3);
    (sb, server, a, b, want)
}

/// Answers the same as before, each by each.
#[track_caller]
fn same_answers(got: &[(String, String)], want: &[(String, String)], context: &str) {
    assert_eq!(got.len(), want.len(), "{context}");
    for ((p, g), (_, w)) in got.iter().zip(want) {
        assert!(g == w, "{context}: {p} answers otherwise\n{g}\nwas\n{w}");
    }
}

/// SIGKILL at points all the way through the move, with a page cache small enough that it spills
/// into the WAL before the commit, and a full disk at its commit: each leaves schema 3 as it was,
/// table for table, until the commit, after which it is schema 4. A kill in the compaction that
/// follows leaves schema 4 as the move left it. Then the server answers everything as it did, fsck
/// finds it clean, and the devices go on and agree.
#[test]
fn a_kill_anywhere_in_the_move_to_schema_4_leaves_one_schema_or_the_other_whole() {
    let (_sb, mut server, a, b, want) = on_schema_3("schema-move-killed");
    let db = db_of(&server);
    let data = server.data.to_str().unwrap().to_string();
    let three = state_everything(&db);
    let spill = (
        "CAIRN_FP_TUNE",
        "server.migrate_cache_kib=64,server.compact_floor=0",
    );
    for point in [
        "server.migrate.due",
        "server.migrate.begun",
        "server.migrate.revisions#1",
        "server.migrate.revisions#2",
        "server.migrate.journal#1",
        "server.migrate.journal#2",
        "server.migrate.moved",
        "server.migrate.commit",
    ] {
        let run = run_server(
            &["fsck", "--data", &data],
            &[spill, ("CAIRN_FP_CRASH", point)],
        );
        assert!(run.was_killed(), "{point}: never reached: {}", run.text());
        if point == "server.migrate.commit" {
            assert!(wal_len(&db) > 0, "nothing of the move was in the WAL");
        }
        assert_eq!(schema(&db), 3, "{point}");
        same_state(&state_everything(&db), &three, point);
    }
    let run = run_server(
        &["fsck", "--data", &data],
        &[spill, ("CAIRN_FP_ENOSPC", "server.migrate.write")],
    );
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr.contains("ran out of room") && run.stderr.contains("stays at schema 3"),
        "{}",
        run.text()
    );
    assert_eq!(schema(&db), 3);
    same_state(&state_everything(&db), &three, "a full disk at the commit");
    assert_eq!(wal_len(&db), 0, "what the failed move wrote is let go");

    let run = run_server(
        &["fsck", "--data", &data],
        &[spill, ("CAIRN_FP_CRASH", "server.migrate.committed")],
    );
    assert!(run.was_killed(), "never committed: {}", run.text());
    assert_eq!(schema(&db), 4);
    let four = state_everything(&db);
    let (count, free) = state_pages(&db);
    assert!(
        free * 2 > count,
        "{free} of {count} pages free after the move"
    );
    let run = run_server(
        &["fsck", "--data", &data],
        &[spill, ("CAIRN_FP_CRASH", "server.compact.vacuuming#3")],
    );
    assert!(run.was_killed(), "never compacted: {}", run.text());
    same_state(&state_everything(&db), &four, "a kill in the compaction");
    assert_eq!(state_pages(&db), (count, free));

    // The compaction, whole this time, and fsck after it.
    let run = run_server(&["fsck", "--data", &data], &[spill]).expect_ok();
    assert!(
        run.stderr.contains("compacted the database"),
        "{}",
        run.text()
    );
    assert!(run.stdout.contains("clean"), "{}", run.text());
    // And moved on to schema 5, which only adds to it.
    assert_eq!(schema(&db), 5);
    same_state(&without_5(state_everything(&db)), &four, "compacted");
    assert_eq!(state_pages(&db).1, 0);

    server.restart();
    same_answers(&answers(&server), &want, "after the move");
    a.write("d4/after", "after the move");
    b.write("d5/f5", "edited on b after the move");
    converge(&[&a, &b]);
    assert_eq!(b.read("d4/after"), b"after the move");
    assert_eq!(a.read("d5/f5"), b"edited on b after the move");
    assert!(server.fsck().contains("clean"));
}

/// The move as a server makes it when it starts, saying what it did; and moved back to schema 3
/// and on to 4 again, every row the same, so nothing was lost either way.
#[test]
fn a_server_moves_a_database_of_schema_3_when_it_starts_and_answers_the_same() {
    let (_sb, mut server, a, b, want) = on_schema_3("schema-move");
    let db = db_of(&server);
    let data = server.data.to_str().unwrap().to_string();
    let three = state_everything(&db);
    server.restart();
    assert_eq!(schema(&db), 5);
    same_answers(&answers(&server), &want, "moved by the server");
    let four = state_everything(&db);
    server.kill();
    let run = run_server(&["downgrade", "--data", &data], &[]).expect_ok();
    assert!(run.stdout.contains("back to schema 4"), "{}", run.text());
    let run = run_server(&["downgrade", "--data", &data], &[]).expect_ok();
    assert!(run.stdout.contains("back to schema 3"), "{}", run.text());
    assert!(!run.stdout.contains("not as long"), "{}", run.text());
    same_state(&state_everything(&db), &three, "moved back again");
    // A server holds the lock: no move under it.
    server.restart();
    let run = run_server(&["downgrade", "--data", &data], &[]);
    assert!(
        !run.ok() && run.stderr.contains("another cairn-server"),
        "{}",
        run.text()
    );
    assert_eq!(schema(&db), 5);
    same_state(&state_everything(&db), &four, "moved again by the server");
    a.write("d6/new", "new");
    converge(&[&a, &b]);
    assert_eq!(b.read("d6/new"), b"new");
}

/// A state with what schema 5 adds taken out: what is left must be schema 4's as it was.
fn without_5(mut state: Vec<(String, Vec<String>)>) -> Vec<(String, Vec<String>)> {
    let new = ["former_names", "folder_names"];
    state.retain(|(t, _)| !new.contains(&t.as_str()));
    for (t, rows) in &mut state {
        if t == "sqlite_master" {
            rows.retain(|r| !new.iter().any(|n| r.contains(n)));
        }
    }
    state
}

/// The move from schema 4 to 5, two tables more: a kill at either point of it, or a full disk at its
/// commit, leaves schema 4 as it was; then it is 5 with everything of 4 the same, the server answers
/// as it did, and the devices go on.
#[test]
fn a_kill_or_a_full_disk_in_the_move_to_schema_5_leaves_schema_4_whole() {
    let sb = Sandbox::on_disk("schema-5-move");
    let mut server = Server::start(sb.path("server"));
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..50 {
        a.write(&format!("d{}/f{i}", i % 5), format!("file {i}"));
    }
    converge(&[&a, &b]);
    let want = answers(&server);
    server.kill();
    let db = db_of(&server);
    let data = server.data.to_str().unwrap().to_string();
    let run = run_server(&["downgrade", "--data", &data], &[]).expect_ok();
    assert!(run.stdout.contains("back to schema 4"), "{}", run.text());
    assert_eq!(schema(&db), 4);
    let four = state_everything(&db);
    for point in ["server.migrate5.begun", "server.migrate5.commit"] {
        let run = run_server(&["fsck", "--data", &data], &[("CAIRN_FP_CRASH", point)]);
        assert!(run.was_killed(), "{point}: never reached: {}", run.text());
        assert_eq!(schema(&db), 4, "{point}");
        same_state(&state_everything(&db), &four, point);
    }
    let run = run_server(
        &["fsck", "--data", &data],
        &[("CAIRN_FP_ENOSPC", "server.migrate5.write")],
    );
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr.contains("ran out of room") && run.stderr.contains("stays at schema 4"),
        "{}",
        run.text()
    );
    assert_eq!(schema(&db), 4);
    same_state(&state_everything(&db), &four, "a full disk at the commit");

    let run = run_server(&["fsck", "--data", &data], &[]).expect_ok();
    assert!(run.stdout.contains("clean"), "{}", run.text());
    assert_eq!(schema(&db), 5);
    same_state(&without_5(state_everything(&db)), &four, "moved to 5");
    server.restart();
    same_answers(&answers(&server), &want, "after the move to 5");
    a.write("after", "after the move");
    converge(&[&a, &b]);
    assert_eq!(b.read("after"), b"after the move");
}
