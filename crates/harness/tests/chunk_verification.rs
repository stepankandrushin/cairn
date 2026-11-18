//! The Unix verification cache must not hide changed, removed, or damaged chunks.
//! Other platforms always rehash; these tests exercise Unix inode and ctime invalidation.
#![cfg(unix)]
mod common;
use cairn_protocol::{wire, Hash};
use common::*;
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn token(server: &Server) -> String {
    std::fs::read_to_string(&server.token_file)
        .unwrap()
        .trim()
        .into()
}
fn missing(server: &Server, hashes: &[Hash]) -> Vec<Hash> {
    serde_json::from_value(
        post(
            server,
            "/v1/ns/cloud/chunks/missing",
            Auth::Bearer(&token(server)),
            json!({"hashes":hashes}),
        )
        .ok()
        .json()["hashes"]
            .clone(),
    )
    .unwrap()
}
fn upload(server: &Server, bytes: &[u8]) -> Hash {
    let hash = Hash::of(bytes);
    let mut frame = Vec::new();
    wire::write_frame(&mut frame, &hash, bytes);
    request(
        server,
        "POST",
        "/v1/ns/cloud/chunks/upload",
        Auth::Bearer(&token(server)),
        &[],
        &frame,
    )
    .ok();
    hash
}
fn path(server: &Server, hash: Hash) -> PathBuf {
    std::fs::read_dir(server.data.join("ns"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("chunks")
        .join(&hash.to_string()[..2])
        .join(hash.to_string())
}
fn verifications(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| *l == "server.chunk.verify")
        .count()
}

#[test]
fn missing_reuses_verification_but_rechecks_damage_even_with_mtime_put_back() {
    let sb = Sandbox::on_disk("verified-damage");
    let log = sb.path("steps");
    let server = Server::start_with(
        sb.path("server"),
        vec![("CAIRN_FP_LOG".into(), log.to_str().unwrap().into())],
    );
    let bytes = token::make(66, 2_000_000);
    let hash = upload(&server, &bytes);
    std::thread::sleep(Duration::from_millis(2100));
    assert!(missing(&server, &[hash]).is_empty());
    let count = verifications(&log);
    for _ in 0..12 {
        assert!(missing(&server, &[hash]).is_empty());
    }
    assert_eq!(verifications(&log), count, "no reread on twelve retries");
    let file = path(&server, hash);
    let (sec, nsec) = mtime_of(&file);
    let mut bad = bytes.clone();
    bad[1000] ^= 1;
    std::fs::write(&file, &bad).unwrap();
    set_mtime(&file, sec, nsec);
    assert_eq!(missing(&server, &[hash]), vec![hash]);
    upload(&server, &bytes);
    assert!(missing(&server, &[hash]).is_empty());
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    // Replacement with the same length and mtime, then truncation, then disappearance.
    let other = sb.path("replacement");
    std::fs::write(&other, &bad).unwrap();
    set_mtime(&other, sec, nsec);
    std::fs::rename(&other, &file).unwrap();
    assert_eq!(missing(&server, &[hash]), vec![hash]);
    upload(&server, &bytes);
    assert!(missing(&server, &[hash]).is_empty());
    std::fs::write(&file, b"short").unwrap();
    assert_eq!(missing(&server, &[hash]), vec![hash]);
    upload(&server, &bytes);
    assert!(missing(&server, &[hash]).is_empty());
    std::fs::remove_file(&file).unwrap();
    assert_eq!(missing(&server, &[hash]), vec![hash]);
    upload(&server, &bytes);
    assert!(missing(&server, &[hash]).is_empty());
}

#[test]
fn verification_expires_and_its_memory_is_bounded() {
    for tune in ["chunk.verified_secs=0", "chunk.verified_count=1"] {
        let sb = Sandbox::on_disk("verified-bound");
        let log = sb.path("steps");
        let server = Server::start_with(
            sb.path("server"),
            vec![
                ("CAIRN_FP_LOG".into(), log.to_str().unwrap().into()),
                ("CAIRN_FP_TUNE".into(), tune.into()),
            ],
        );
        let a = upload(&server, b"a");
        let b = upload(&server, b"b");
        std::thread::sleep(Duration::from_millis(2100));
        assert!(missing(&server, &[a, b]).is_empty());
        let count = verifications(&log);
        assert!(missing(&server, &[a, b]).is_empty());
        assert_eq!(verifications(&log), count + 2, "{tune}");
    }
}

#[test]
fn a_replacement_during_verification_does_not_cache_unverified_bytes() {
    let sb = Sandbox::on_disk("verified-race");
    let gate = sb.path("gate");
    std::fs::create_dir(&gate).unwrap();
    let server = Server::start_with(
        sb.path("server"),
        vec![(
            "CAIRN_FP_PAUSE".into(),
            format!("server.chunk.verify#1:{}", gate.display()),
        )],
    );
    let hash = upload(&server, b"good content");
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| missing(&server, &[hash]));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !gate.join("reached").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let replacement = sb.path("replacement");
        std::fs::write(&replacement, b"evil content").unwrap();
        std::fs::rename(&replacement, path(&server, hash)).unwrap();
        std::fs::write(gate.join("go"), b"").unwrap();
        assert_eq!(worker.join().unwrap(), vec![hash]);
    });
    assert_eq!(missing(&server, &[hash]), vec![hash]);
    upload(&server, b"good content");
    assert!(missing(&server, &[hash]).is_empty());
}
