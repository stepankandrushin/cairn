//! Retention and collection (docs/format.md, "Collection") under load, killed at every step, paused
//! between a commit's check and its commit, starved of disk and beside a backup: the exit criterion of
//! milestone 4, that a collection under load never removes a chunk anything kept names, nor one a
//! commit is acknowledged with; and what retention gave up answers cleanly, never as damage.
//!
//! The tests take the collection's delay and upload grace to nothing (`server.gc.delay`,
//! `server.gc.grace`) and push its clock on by days a run (`server.gc.clock_step`), so that what a policy
//! gives up in one run is collected in the next, a second later.

mod common;
use base64::Engine;
use cairn_protocol::file::{query_string, FileQuery, Stale, Taken};
use cairn_protocol::{DeviceId, Hash, ObjectId, RevisionId};
use common::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DAY: u64 = 86_400;
const ROOT: &str = "00000000000000000000000000000000";

fn token(server: &Server) -> String {
    std::fs::read_to_string(&server.token_file)
        .unwrap()
        .trim()
        .to_string()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The collections' settings for a test: no delay, no upload grace, and the clock `ahead` seconds on
/// each run.
fn fast(ahead: u64) -> String {
    format!("server.gc.delay=0,server.gc.grace=0,server.gc.clock_step={ahead}")
}

/// `cairn-server gc` with the server stopped.
fn gc(data: &Path, tune: &str) -> Run {
    run_server(
        &["gc", "--data", data.to_str().unwrap()],
        &[("CAIRN_FP_TUNE", tune)],
    )
}

fn gc_status(data: &Path) -> String {
    run_server(&["gc", "--data", data.to_str().unwrap(), "--status"], &[])
        .expect_ok()
        .stdout
}

#[track_caller]
fn policy(server: &Server, text: &str) -> Run {
    server
        .admin(&["folder", "retention", "cloud", "--keep", text])
        .expect_ok()
}

/// `fsck` of a data directory with no server on it: its report.
#[track_caller]
fn fsck(data: &Path) -> String {
    let run = run_server(&["fsck", "--data", data.to_str().unwrap()], &[]);
    assert!(run.ok(), "fsck found damage:\n{}", run.text());
    run.stdout
}

/// Every chunk file of every namespace.
fn chunk_files(data: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let Ok(spaces) = std::fs::read_dir(data.join("ns")) else {
        return out;
    };
    for ns in spaces.flatten() {
        let Ok(fans) = std::fs::read_dir(ns.path().join("chunks")) else {
            continue;
        };
        for fan in fans.flatten() {
            for c in std::fs::read_dir(fan.path()).unwrap().flatten() {
                out.insert(c.path());
            }
        }
    }
    out
}

fn chunk_path(data: &Path, hash: &Hash) -> PathBuf {
    let h = hash.to_string();
    data.join("ns")
        .join(hex::encode("cloud"))
        .join("chunks")
        .join(&h[..2])
        .join(h)
}

/// A picture the server makes a thumbnail of.
fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 3) as u8, (y * 5) as u8, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// Every file under a folder.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(list) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in list.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            } else {
                out.push(e.path());
            }
        }
    }
    out
}

/// Bytes no other test or run makes.
fn noise(rng: &mut StdRng, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    rng.fill(&mut out[..]);
    out
}

/// One chunk batch frame each, uploaded and named by nothing: an orphan.
fn upload_orphans(server: &Server, t: &str, chunks: &[Vec<u8>]) -> Vec<Hash> {
    let mut frames = Vec::new();
    let mut hashes = Vec::new();
    for c in chunks {
        let h = Hash::of(c);
        frames.extend_from_slice(&h.0);
        frames.extend_from_slice(&(c.len() as u32).to_be_bytes());
        frames.extend_from_slice(c);
        hashes.push(h);
    }
    request(
        server,
        "POST",
        "/v1/ns/cloud/chunks/upload",
        Auth::Bearer(t),
        &[],
        &frames,
    )
    .ok();
    hashes
}

/// A file made by `PUT /file` at `path`, or with `base` a change of one: the answer.
fn put_file(
    server: &Server,
    t: &str,
    path: &str,
    data: &[u8],
    base: Option<(ObjectId, RevisionId)>,
) -> Answer {
    let (object, base) = match base {
        Some((o, r)) => (o, Some(r)),
        None => (ObjectId::random(), None),
    };
    let q = FileQuery {
        parent: None,
        path: if base.is_some() {
            String::new()
        } else {
            path.into()
        },
        object,
        revision: RevisionId::random(),
        device: DeviceId([7; 16]),
        device_name: "phone".into(),
        mtime: "1700000000".into(),
        btime: None,
        taken: Taken::Rename,
        base,
        stale: Stale::Copy,
    };
    request(
        server,
        "PUT",
        &format!("/v1/ns/cloud/file?{}", query_string(&q)),
        Auth::Bearer(t),
        &[],
        data,
    )
}

/// A web upload of a new file into the root, waited for: its last status, or the refusal that began it.
fn web_upload(server: &Server, t: &str, name: &str, data: &[u8]) -> Result<Value, String> {
    let base = "/v1/ns/cloud";
    let auth = Auth::Bearer(t);
    let began = post(
        server,
        &format!("{base}/uploads"),
        auth,
        json!({"parent": ROOT, "name": b64(name.as_bytes()), "size": data.len()}),
    );
    if began.code != 201 {
        return Err(format!("{} {}", began.code, began.text()));
    }
    let id = began.json()["id"].as_str().unwrap().to_string();
    let sent = request(
        server,
        "PUT",
        &format!("{base}/uploads/{id}?offset=0"),
        auth,
        &[],
        data,
    );
    if sent.code != 200 {
        return Err(format!("{} {}", sent.code, sent.text()));
    }
    let committed = request(
        server,
        "POST",
        &format!("{base}/uploads/{id}/commit"),
        auth,
        &[],
        b"",
    );
    if committed.code != 202 {
        return Err(format!("{} {}", committed.code, committed.text()));
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = get(server, &format!("{base}/uploads/{id}"), auth).json();
        if status["state"] != "storing" {
            return Ok(status);
        }
        assert!(
            Instant::now() < deadline,
            "an upload still storing after a minute"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn dir(server: &Server, t: &str, object: &str) -> Value {
    get(
        server,
        &format!("/v1/ns/cloud/dir/{object}"),
        Auth::Bearer(t),
    )
    .ok()
    .json()
}

#[track_caller]
fn entry(listing: &Value, name: &str) -> Value {
    listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["display"] == name)
        .unwrap_or_else(|| panic!("no {name} in {listing}"))
        .clone()
}

fn history(server: &Server, t: &str, object: &str) -> Vec<Value> {
    get(
        server,
        &format!("/v1/ns/cloud/history/{object}"),
        Auth::Bearer(t),
    )
    .ok()
    .json()["revisions"]
        .as_array()
        .unwrap()
        .clone()
}

fn content(server: &Server, t: &str, object: &str, revision: &str) -> Answer {
    get(
        server,
        &format!("/v1/ns/cloud/content/{object}?revision={revision}"),
        Auth::Bearer(t),
    )
}

fn pruned(server: &Server, t: &str, revisions: &[&str]) -> BTreeSet<String> {
    post(
        server,
        "/v1/ns/cloud/pruned",
        Auth::Bearer(t),
        json!({ "revisions": revisions }),
    )
    .ok()
    .json()["pruned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["revision"].as_str().unwrap().to_string())
        .collect()
}

/// A folder with history: `doc.txt` changed once, `gone.txt` made and deleted, `photo.jpg` sent by a
/// phone, and a symbolic link. Each file's bytes its own.
struct Story {
    sb: Sandbox,
    server: Server,
    t: String,
    a: Device,
    doc: String,
    doc_v1: String,
    doc_v2: String,
    v1: Vec<u8>,
    v2: Vec<u8>,
    gone: String,
}

fn story(label: &str) -> Story {
    let mut rng = StdRng::seed_from_u64(rand::random());
    let sb = Sandbox::on_disk(label);
    let server = Server::start(sb.path("server"));
    let t = token(&server);
    let a = Device::new(&sb, &server, "a");
    let (v1, v2) = (noise(&mut rng, 700_000), noise(&mut rng, 900));
    a.write("doc.txt", &v1);
    a.write("gone.txt", noise(&mut rng, 3000));
    #[cfg(unix)]
    std::os::unix::fs::symlink("doc.txt", a.path("link")).unwrap();
    a.synced();
    a.write("doc.txt", &v2);
    remove_file(&a.path("gone.txt"));
    a.synced();
    put_file(&server, &t, "photo.jpg", &noise(&mut rng, 5000), None).ok();
    let root = dir(&server, &t, "root");
    let doc = entry(&root, "doc.txt")["object"]
        .as_str()
        .unwrap()
        .to_string();
    let revs = history(&server, &t, &doc);
    assert_eq!(revs.len(), 2);
    let deleted = get(&server, "/v1/ns/cloud/deleted", Auth::Bearer(&t))
        .ok()
        .json();
    let gone = deleted["items"][0]["object"].as_str().unwrap().to_string();
    Story {
        doc_v2: revs[0]["revision"].as_str().unwrap().to_string(),
        doc_v1: revs[1]["revision"].as_str().unwrap().to_string(),
        doc,
        v1,
        v2,
        gone,
        sb,
        server,
        t,
        a,
    }
}

/// A folder's policy gives up the old revision of a file and a deleted file's after its trash period,
/// keeping their content a collection's delay; reads of them answer as given up, never as damage; and a
/// collection after that takes their chunks and nothing anything kept names.
#[test]
fn what_retention_gives_up_answers_cleanly_and_its_content_goes_a_collection_later() {
    let mut st = story("gc-story");
    let data = st.server.data.clone();
    // A picture whose thumbnail is made, then deleted: the thumbnail goes with its content.
    let t = st.t.clone();
    let pic = put_file(&st.server, &t, "pic.png", &png(64, 48), None)
        .ok()
        .json();
    let pic_object = pic["object"].as_str().unwrap().to_string();
    get(
        &st.server,
        &format!("/v1/ns/cloud/thumb/{pic_object}?size=128"),
        Auth::Bearer(&t),
    )
    .ok();
    let thumbs = |data: &Path| {
        walk_files(&data.join("thumbs"))
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .count()
    };
    assert_eq!(thumbs(&data), 1);
    let pic_link = entry(&dir(&st.server, &t, "root"), "pic.png");
    post(
        &st.server,
        "/v1/ns/cloud/delete",
        Auth::Bearer(&t),
        json!({"object": pic_object, "link": {"parent": ROOT, "name": pic_link["name"]}}),
    )
    .ok();
    // Beside a running server, a run is refused, and a dry run says what one would do and does none
    // of it.
    policy(&st.server, "trash 1d");
    let beside = gc(&data, &fast(3 * DAY));
    assert!(
        !beside.ok() && beside.stderr.contains("only --dry-run and --status"),
        "{}",
        beside.text()
    );
    let dry = run_server(
        &["gc", "--data", data.to_str().unwrap(), "--dry-run"],
        &[("CAIRN_FP_TUNE", &fast(3 * DAY))],
    )
    .expect_ok();
    assert!(
        dry.stdout.contains("would give up 3 revision(s)"),
        "{}",
        dry.text()
    );
    assert!(
        !gc_status(&data).contains("run "),
        "a dry run recorded a run"
    );
    st.server.kill();
    let before = chunk_files(&data);

    // Three days on: the old revision, and the file deleted past its trash period, given up; nothing
    // removed yet, as what was given up keeps its content until a later run's cutoff passes it.
    let run = gc(&data, &fast(3 * DAY)).expect_ok();
    assert!(
        run.stdout.contains("gave up 3 revision(s)"),
        "{}",
        run.text()
    );
    assert_eq!(chunk_files(&data), before);
    let report = fsck(&data);
    assert!(
        report.contains("3 revisions given up by retention (0 of them without all their content)"),
        "{report}"
    );

    // What was given up answers so.
    st.server.set_env(vec![(
        "CAIRN_FP_TUNE".into(),
        format!("server.clock_ahead={}", 3 * DAY),
    )]);
    st.server.restart();
    let revs = history(&st.server, &t, &st.doc);
    assert!(revs[0].get("pruned").is_none(), "{revs:?}");
    assert!(revs[1]["pruned"].as_i64().is_some(), "{revs:?}");
    let old = content(&st.server, &t, &st.doc, &st.doc_v1);
    assert_eq!(old.code, 410, "{}", old.text());
    assert!(
        old.text()
            .contains("removed by this folder's retention on "),
        "{}",
        old.text()
    );
    let restore = post(
        &st.server,
        "/v1/ns/cloud/restore",
        Auth::Bearer(&t),
        json!({"object": st.doc, "revision": st.doc_v1}),
    );
    assert_eq!(restore.code, 410, "{}", restore.text());
    assert_eq!(
        content(&st.server, &t, &st.doc, &st.doc_v2).ok().body,
        st.v2
    );
    assert_eq!(
        pruned(&st.server, &t, &[&st.doc_v1, &st.doc_v2]),
        [st.doc_v1.clone()].into()
    );
    let retention = get(&st.server, "/v1/ns/cloud/retention", Auth::Bearer(&t))
        .ok()
        .json();
    assert_eq!(retention["policy"], "trash 1d", "{retention}");
    assert_eq!(retention["trash"], DAY, "{retention}");
    // Past its trash period, a deleted file is listed no more, and does not come back: by the web, nor
    // by a link of a device's own.
    let deleted = get(&st.server, "/v1/ns/cloud/deleted", Auth::Bearer(&t))
        .ok()
        .json();
    assert!(deleted["items"].as_array().unwrap().is_empty(), "{deleted}");
    let history_line = st.a.cairn(&["history", "doc.txt"]).expect_ok();
    assert!(
        history_line
            .stdout
            .contains("removed by the folder's retention on "),
        "{}",
        history_line.text()
    );
    let back = post(
        &st.server,
        "/v1/ns/cloud/undelete",
        Auth::Bearer(&t),
        json!({"objects": [st.gone]}),
    );
    assert_eq!(back.code, 409, "{}", back.text());
    assert!(
        back.text().contains("past this folder's trash period"),
        "{}",
        back.text()
    );
    let link = post(
        &st.server,
        "/v1/ns/cloud/commit",
        Auth::Bearer(&t),
        json!({
            "op_id": hex::encode(rand::random::<[u8; 16]>()),
            "device": hex::encode(rand::random::<[u8; 16]>()),
            "device_name": "t",
            "ops": [{"op": "link", "object": st.gone, "link": {"parent": ROOT, "name": b64(b"gone.txt")}}]
        }),
    )
    .ok()
    .json();
    assert_eq!(link["result"], "rejected", "{link}");
    assert_eq!(
        link["reject"]["reason"]["reject"], "no_such_object",
        "{link}"
    );

    // A run four days on takes their chunks, and nothing anything kept names.
    st.server.kill();
    let run = gc(&data, &fast(4 * DAY)).expect_ok();
    assert!(run.stdout.contains("removed "), "{}", run.text());
    let after = chunk_files(&data);
    assert!(
        after.len() < before.len(),
        "{} of {}",
        after.len(),
        before.len()
    );
    assert!(after.is_subset(&before));
    let report = fsck(&data);
    assert!(
        report.contains("3 revisions given up by retention (3 of them without all their content)"),
        "{report}"
    );
    assert_eq!(thumbs(&data), 0, "a thumbnail outlived its content");
    let status = gc_status(&data);
    assert!(
        status.contains("cloud: trash 1d, set by administrator"),
        "{status}"
    );
    assert!(status.contains(": done, "), "{status}");

    // Every device still has all it had, and a new one gets every head.
    st.server.restart();
    assert_eq!(
        content(&st.server, &t, &st.doc, &st.doc_v2).ok().body,
        st.v2
    );
    let b = Device::new(&st.sb, &st.server, "b");
    let picture = converge(&[&st.a, &b]);
    assert_eq!(b.read("doc.txt"), st.v2);
    assert!(b.exists("photo.jpg"));
    assert!(!b.exists("gone.txt"));
    let _ = (picture, &st.v1);
}

/// A policy set again, or taken away, gives back what was given up and no collection took yet, whole;
/// what one took stays given up.
#[test]
fn a_policy_set_again_brings_back_what_no_collection_took() {
    let mut st = story("gc-undo");
    let data = st.server.data.clone();
    st.server.kill();
    policy(&st.server, "trash 1d");
    gc(&data, &fast(3 * DAY)).expect_ok();
    let none = st
        .server
        .admin(&["folder", "retention", "cloud", "--none"])
        .expect_ok();
    assert!(
        none.stdout.contains(
            "2 revision(s) given up before, whose content was still there, are history again"
        ),
        "{}",
        none.text()
    );
    st.server.restart();
    let t = st.t.clone();
    assert!(history(&st.server, &t, &st.doc)
        .iter()
        .all(|r| r.get("pruned").is_none()));
    assert_eq!(
        content(&st.server, &t, &st.doc, &st.doc_v1).ok().body,
        st.v1
    );
    // With no policy, a run gives up and removes nothing, orphans included.
    st.server.kill();
    let orphans_before = chunk_files(&data);
    let run = gc(&data, &fast(30 * DAY)).expect_ok();
    assert!(run.stdout.contains("no policy"), "{}", run.text());
    assert_eq!(chunk_files(&data), orphans_before);

    // Given up and collected: a policy set again brings back nothing of that.
    policy(&st.server, "trash 1d");
    gc(&data, &fast(3 * DAY)).expect_ok();
    gc(&data, &fast(4 * DAY)).expect_ok();
    let again = policy(&st.server, "all 1y; trash 1y");
    assert!(!again.stdout.contains("history again"), "{}", again.text());
    fsck(&data);
    st.server.restart();
    assert_eq!(
        content(&st.server, &t, &st.doc, &st.doc_v1).code,
        410,
        "collected, it stays given up"
    );
}

/// Anyone with access reads a folder's policy; only its managers set it, to a policy that only ever
/// gives up more as time goes on.
#[test]
fn a_folders_policy_is_read_by_anyone_with_access_and_set_by_its_managers() {
    let sb = Sandbox::on_disk("gc-api");
    let server = Server::start(sb.path("server"));
    let admin = token(&server);
    let bob_file = server.user("bob");
    let bob = std::fs::read_to_string(&bob_file)
        .unwrap()
        .trim()
        .to_string();
    server
        .admin(&["grant", "cloud", "bob", "write"])
        .expect_ok();
    let put = |t: &str, body: Value| {
        request(
            &server,
            "PUT",
            "/v1/ns/cloud/retention",
            Auth::Bearer(t),
            &[("Content-Type", "application/json")],
            body.to_string().as_bytes(),
        )
    };
    let read = get(&server, "/v1/ns/cloud/retention", Auth::Bearer(&bob))
        .ok()
        .json();
    assert_eq!(read["policy"], Value::Null, "{read}");
    let no = put(&bob, json!({"policy": "trash 30d"}));
    assert_eq!(no.code, 403, "{}", no.text());
    let bad = put(&admin, json!({"policy": "daily 30d, hourly 1d"}));
    assert_eq!(bad.code, 400, "{}", bad.text());
    let set = put(
        &admin,
        json!({"policy": "all 2d, hourly 1d, daily 30d, weekly forever"}),
    )
    .ok()
    .json();
    assert_eq!(
        set["policy"], "all 2d, hourly 1d, daily 30d, weekly forever; trash 30d",
        "{set}"
    );
    assert_eq!(set["set_by"], "admin", "{set}");
    let read = get(&server, "/v1/ns/cloud/retention", Auth::Bearer(&bob))
        .ok()
        .json();
    assert_eq!(read["trash"], 30 * DAY, "{read}");
    let shown = server.admin(&["folder", "retention", "cloud"]).expect_ok();
    assert!(
        shown
            .stdout
            .contains("weekly forever; trash 30d, set by admin"),
        "{}",
        shown.text()
    );
    let off = put(&admin, json!({"policy": null})).ok().json();
    assert_eq!(off["policy"], Value::Null, "{off}");
    // A server's flags hold a delay of at least a day.
    let refused = run_server(
        &[
            "gc",
            "--data",
            server.data.to_str().unwrap(),
            "--dry-run",
            "--gc-delay",
            "12h",
        ],
        &[],
    );
    assert!(
        !refused.ok() && refused.stderr.contains("under a day"),
        "{}",
        refused.text()
    );
}

/// What one round of the load did, to check when it is over.
#[derive(Default)]
struct Ledger {
    /// Each revision a commit was acknowledged with, by a phone's upload, the web's or a restore, and
    /// the bytes it holds.
    acked: Vec<(ObjectId, RevisionId, Vec<u8>)>,
    /// What must not happen: a refusal for want of a chunk where none may be, or the server's error.
    wrong: Vec<String>,
    /// Device syncs that went through, and those refused once and sent again.
    syncs: u64,
    refused_once: u64,
    restores: u64,
    gone: u64,
}

/// One device's step: a new file, a change, a change back to bytes it had before, or a delete, each
/// in its own folder; then a sync.
fn device_steps(d: &Device, seed: u64, deadline: Instant, grace: bool, ledger: &Mutex<Ledger>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut had: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
    let mut n = 0;
    while Instant::now() < deadline {
        let names: Vec<String> = had.keys().cloned().collect();
        let size = if rng.random_range(0..10) == 0 {
            rng.random_range(600_000..1_500_000)
        } else {
            rng.random_range(10..4000)
        };
        match rng.random_range(0..6) {
            0 | 1 if !names.is_empty() => {
                let p = &names[rng.random_range(0..names.len())];
                let bytes = noise(&mut rng, size);
                d.write(p, &bytes);
                had.get_mut(p).unwrap().push(bytes);
            }
            2 if !names.is_empty() => {
                // Back to what it held before: chunks the collection may be about to take.
                let p = &names[rng.random_range(0..names.len())];
                let versions = &had[p];
                let old = versions[rng.random_range(0..versions.len())].clone();
                d.write(p, &old);
                had.get_mut(p).unwrap().push(old);
            }
            3 if !names.is_empty() => {
                let p = names[rng.random_range(0..names.len())].clone();
                remove_file(&d.path(&p));
                had.remove(&p);
            }
            _ => {
                n += 1;
                let p = format!("{}/f{n}", d.name);
                let bytes = noise(&mut rng, size);
                d.write(&p, &bytes);
                had.insert(p, vec![bytes]);
            }
        }
        let run = d.cairn(&["sync", "--allow-mass-delete"]);
        let missing = run.stdout.contains("MissingChunk") || run.stderr.contains("MissingChunk");
        let mut l = ledger.lock().unwrap();
        l.syncs += 1;
        if missing {
            l.refused_once += 1;
        }
        if grace && (missing || !run.ok()) {
            l.wrong.push(format!("device {}: {}", d.name, run.text()));
        }
        if run.text().contains("answered 500") {
            l.wrong.push(format!("device {}: {}", d.name, run.text()));
        }
    }
}

/// A phone's uploads, new files and changes of them.
fn phone_steps(server: &Server, t: &str, seed: u64, deadline: Instant, ledger: &Mutex<Ledger>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut mine: Vec<(ObjectId, RevisionId)> = Vec::new();
    let mut n = 0;
    while Instant::now() < deadline {
        let size = if rng.random_range(0..20) == 0 {
            rng.random_range(600_000..1_200_000)
        } else {
            rng.random_range(10..9000)
        };
        let bytes = noise(&mut rng, size);
        let base = (!mine.is_empty() && rng.random_range(0..3) == 0)
            .then(|| mine[rng.random_range(0..mine.len())]);
        n += 1;
        let answer = put_file(server, t, &format!("p/{n}.bin"), &bytes, base);
        let mut l = ledger.lock().unwrap();
        if !(200..300).contains(&answer.code) {
            l.wrong
                .push(format!("PUT /file: {} {}", answer.code, answer.text()));
            continue;
        }
        let a = answer.json();
        let object: ObjectId = a["object"].as_str().unwrap().parse().unwrap();
        let revision: RevisionId = a["revision"].as_str().unwrap().parse().unwrap();
        l.acked.push((object, revision, bytes));
        match base {
            Some((o, _)) if o == object => {
                let i = mine.iter().position(|(m, _)| *m == o).unwrap();
                mine[i] = (object, revision);
            }
            _ => mine.push((object, revision)),
        }
    }
}

/// The web's uploads, restores of older revisions, deletes and bringing back.
fn web_steps(
    server: &Server,
    t: &str,
    seed: u64,
    deadline: Instant,
    grace: bool,
    ledger: &Mutex<Ledger>,
) {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut n = 0;
    while Instant::now() < deadline {
        match rng.random_range(0..6) {
            0 => {
                n += 1;
                let size = if rng.random_range(0..10) == 0 {
                    rng.random_range(600_000..900_000)
                } else {
                    rng.random_range(10..9000)
                };
                let bytes = noise(&mut rng, size);
                let name = format!("w{n}.bin");
                match web_upload(server, t, &name, &bytes) {
                    Ok(status) if status["state"] == "done" => {
                        let d = &status["done"];
                        let object = d["object"].as_str().unwrap().parse().unwrap();
                        let revision = d["revision"].as_str().unwrap().parse().unwrap();
                        ledger.lock().unwrap().acked.push((object, revision, bytes));
                    }
                    other => ledger
                        .lock()
                        .unwrap()
                        .wrong
                        .push(format!("web upload: {other:?}")),
                }
            }
            1 | 2 => {
                // An older revision of something acknowledged more than once, back as a new one.
                let pick = {
                    let l = ledger.lock().unwrap();
                    let mut count: BTreeMap<ObjectId, usize> = BTreeMap::new();
                    for (o, _, _) in &l.acked {
                        *count.entry(*o).or_default() += 1;
                    }
                    let many: Vec<ObjectId> = count
                        .into_iter()
                        .filter(|(_, n)| *n > 1)
                        .map(|(o, _)| o)
                        .collect();
                    if many.is_empty() {
                        drop(l);
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    let o = many[rng.random_range(0..many.len())];
                    let all: Vec<(RevisionId, Vec<u8>)> = l
                        .acked
                        .iter()
                        .filter(|(x, _, _)| *x == o)
                        .map(|(_, r, b)| (*r, b.clone()))
                        .collect();
                    (o, all)
                };
                let (object, revisions) = pick;
                let (revision, bytes) = revisions[rng.random_range(0..revisions.len())].clone();
                let r = post(
                    server,
                    "/v1/ns/cloud/restore",
                    Auth::Bearer(t),
                    json!({"object": object.to_string(), "revision": revision.to_string()}),
                );
                let mut l = ledger.lock().unwrap();
                match r.code {
                    200 => {
                        let new: RevisionId =
                            r.json()["revision"].as_str().unwrap().parse().unwrap();
                        l.acked.push((object, new, bytes));
                        l.restores += 1;
                    }
                    410 => l.gone += 1,
                    // The head it has now, or deleted since.
                    400 | 404 => {}
                    409 if !grace || !r.text().contains("not on the server") => {}
                    _ => l.wrong.push(format!("restore: {} {}", r.code, r.text())),
                }
            }
            3 => {
                let listing = dir(server, t, "root");
                let files: Vec<Value> = listing["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|e| e["display"].as_str().unwrap().starts_with('w'))
                    .cloned()
                    .collect();
                if files.is_empty() {
                    continue;
                }
                let f = &files[rng.random_range(0..files.len())];
                let r = post(
                    server,
                    "/v1/ns/cloud/delete",
                    Auth::Bearer(t),
                    json!({"object": f["object"], "link": {"parent": ROOT, "name": f["name"]}}),
                );
                if r.code >= 500 {
                    ledger
                        .lock()
                        .unwrap()
                        .wrong
                        .push(format!("delete: {} {}", r.code, r.text()));
                }
            }
            _ => {
                let list = get(server, "/v1/ns/cloud/deleted?days=3650", Auth::Bearer(t)).json();
                let items = list["items"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    continue;
                }
                let it = &items[rng.random_range(0..items.len())];
                let r = post(
                    server,
                    "/v1/ns/cloud/undelete",
                    Auth::Bearer(t),
                    json!({"objects": [it["object"]]}),
                );
                if r.code >= 500 {
                    ledger
                        .lock()
                        .unwrap()
                        .wrong
                        .push(format!("undelete: {} {}", r.code, r.text()));
                }
            }
        }
    }
}

const POLICIES: &[&str] = &[
    "trash 1d",
    "all 1d; trash 1d",
    "hourly 2d, daily 30d; trash 1d",
    "all 3d, weekly forever; trash 2d",
];

/// Devices syncing, a phone's uploads, the web's uploads, restores, deletes and bringing back, while a
/// collection runs every second with no delay, the clock two days on each time, and with `grace` the
/// upload grace a test's version of the default, else none. Then every acknowledged revision is either
/// downloaded byte for byte or answered as given up, fsck is clean, and the devices agree. With no
/// grace a device's commit may be refused once for a chunk taken between its check and its commit, and
/// sent again; with it, nothing is ever refused.
fn load_round(seed: u64, grace: bool) {
    let mut rng = StdRng::seed_from_u64(seed);
    let sb = Sandbox::on_disk("gc-load");
    let mut server = Server::start(sb.path("server"));
    let t = token(&server);
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    let policy_text = POLICIES[seed as usize % POLICIES.len()];
    policy(&server, policy_text);
    // Old orphans, as an upload killed long ago leaves: what the grace lets a collection take.
    let orphans: Vec<Vec<u8>> = (0..20).map(|_| noise(&mut rng, 300)).collect();
    let orphans = upload_orphans(&server, &t, &orphans);
    let long_ago = (std::time::SystemTime::now() - Duration::from_secs(3 * DAY))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    for h in &orphans {
        set_mtime(&chunk_path(&server.data, h), long_ago, 0);
    }
    let tune = if grace {
        format!(
            "server.gc.first=0,server.gc.every=1,server.gc.delay=0,server.gc.grace={},server.gc.clock_step={}",
            DAY,
            2 * DAY
        )
    } else {
        format!("server.gc.first=0,server.gc.every=1,{}", fast(2 * DAY))
    };
    server.set_env(vec![("CAIRN_FP_TUNE".into(), tune)]);
    server.restart();

    let ledger = Mutex::new(Ledger::default());
    let deadline = Instant::now() + Duration::from_secs(7);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let l = &ledger;
        let (server, t, a, b) = (&server, &t, &a, &b);
        s.spawn(move || device_steps(a, seed ^ 0xa, deadline, grace, l));
        s.spawn(move || device_steps(b, seed ^ 0xb, deadline, grace, l));
        s.spawn(move || phone_steps(server, t, seed ^ 0xc, deadline, l));
        s.spawn(move || web_steps(server, t, seed ^ 0xd, deadline, grace, l));
    });
    stop.store(true, Ordering::SeqCst);
    let mut ledger = ledger.into_inner().unwrap();
    assert!(
        ledger.wrong.is_empty(),
        "seed {seed}, {policy_text}: {:#?}",
        ledger.wrong
    );

    // Killed wherever its collection is, and checked.
    server.kill();
    let report = fsck(&server.data);
    let status = gc_status(&server.data);
    let removed: u64 = {
        let db = rusqlite::Connection::open_with_flags(
            server.data.join("meta.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.query_row("SELECT coalesce(sum(chunks), 0) FROM gc_runs", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap() as u64
    };
    assert!(
        status.contains(": done, "),
        "seed {seed}: no collection ran\n{status}"
    );
    if grace {
        assert!(
            orphans
                .iter()
                .all(|h| !chunk_path(&server.data, h).exists()),
            "seed {seed}: the old orphans are still there\n{status}"
        );
    } else {
        assert!(
            removed > 0,
            "seed {seed}: no collection removed anything\n{status}"
        );
    }

    // Every acknowledged revision is there byte for byte, or answered as given up; none is lost.
    server.set_env(Vec::new());
    server.restart();
    let mut given_up = 0;
    for (object, revision, bytes) in &ledger.acked {
        let got = content(&server, &t, &object.to_string(), &revision.to_string());
        match got.code {
            200 => assert!(
                got.body == *bytes,
                "seed {seed}: revision {revision} of {object} does not hold what it was acknowledged with"
            ),
            410 => {
                given_up += 1;
                assert!(
                    pruned(&server, &t, &[&revision.to_string()]).contains(&revision.to_string()),
                    "seed {seed}: {revision} answered as given up, and is not"
                );
            }
            404 => {
                let item = get(&server, &format!("/v1/ns/cloud/item/{object}"), Auth::Bearer(&t));
                assert_eq!(
                    item.code, 404,
                    "seed {seed}: a revision {revision} of {object}, which is there, is not found"
                );
            }
            other => panic!("seed {seed}: {other} for {revision} of {object}: {}", got.text()),
        }
    }
    let picture = converge(&[&a, &b]);
    let _ = picture;
    let after = server.fsck();
    ledger.gone += given_up;
    eprintln!(
        "seed {seed}, {policy_text}: {} syncs ({} refused once for a chunk taken), {} acknowledged, {} restores, {} given up when read, {removed} chunks removed\n{report}{after}",
        ledger.syncs,
        ledger.refused_once,
        ledger.acked.len(),
        ledger.restores,
        ledger.gone
    );
}

/// This part's seeds: a quarter of `CAIRN_SEEDS`, at least one each; or `CAIRN_SEED` alone.
fn seeds(shard: Shard, f: impl Fn(u64)) {
    if let Ok(seed) = std::env::var("CAIRN_SEED") {
        let seed: u64 = seed.parse().expect("CAIRN_SEED is a number");
        if shard.has(seed as usize) {
            f(seed);
        }
        return;
    }
    let all: usize = std::env::var("CAIRN_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(shard.of, |n: usize| (n / 8).max(shard.of));
    let failed = each_item(shard, all, |seed| f(seed as u64));
    assert!(
        failed.is_empty(),
        "{} of this part's seeds failed; first: {:?}",
        failed.len(),
        failed.first()
    );
}

fn gc_under_load_never_removes_a_reachable_chunk(shard: Shard) {
    seeds(shard, |seed| load_round(seed, false));
}

shards!(gc_under_load_never_removes_a_reachable_chunk: s01 s02 s03 s04);

fn with_the_upload_grace_nothing_under_gc_is_ever_refused(shard: Shard) {
    seeds(shard, |seed| load_round(seed + 1000, true));
}

shards!(with_the_upload_grace_nothing_under_gc_is_ever_refused: s01 s02);

/// A server whose next collection pauses at `point`, and runs once.
fn paused_at(sb: &Sandbox, server: &mut Server, point: &str) -> PathBuf {
    let pause = sb.path(&format!("pause-{}", point.replace('.', "-")));
    std::fs::create_dir_all(&pause).unwrap();
    server.set_env(vec![
        (
            "CAIRN_FP_TUNE".into(),
            format!("server.gc.first=0,server.gc.every=3600,{}", fast(DAY)),
        ),
        (
            "CAIRN_FP_PAUSE".into(),
            format!("{point}:{}", pause.display()),
        ),
    ]);
    server.restart();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !pause.join("reached").exists() {
        assert!(
            Instant::now() < deadline,
            "the collection never reached {point}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pause
}

/// A manifest of one chunk, both uploaded, named by nothing yet: what a commit after them names.
fn manifest_of(server: &Server, t: &str, data: &[u8]) -> (Hash, Hash) {
    let chunk = Hash::of(data);
    let m = cairn_protocol::Manifest::new(
        data.len() as u64,
        vec![cairn_protocol::Segment {
            offset: 0,
            chunks: vec![cairn_protocol::ChunkRef {
                hash: chunk,
                len: data.len() as u32,
            }],
        }],
    );
    let bytes = m.encode();
    let mh = Hash::of(&bytes);
    upload_orphans(server, t, &[data.to_vec(), bytes]);
    (mh, chunk)
}

/// A commit of a new file at the root with this manifest: the answer.
fn commit_file(server: &Server, t: &str, name: &str, manifest: Hash, size: usize) -> Value {
    post(
        server,
        "/v1/ns/cloud/commit",
        Auth::Bearer(t),
        json!({
            "op_id": hex::encode(rand::random::<[u8; 16]>()),
            "device": hex::encode(rand::random::<[u8; 16]>()),
            "device_name": "t",
            "ops": [{
                "op": "create",
                "object": hex::encode(rand::random::<[u8; 16]>()),
                "revision": hex::encode(rand::random::<[u8; 16]>()),
                "body": {
                    "content": {"type": "file", "manifest": manifest.to_string(), "size": size},
                    "metadata": {"mode": 420, "mtime": {"sec": 1_700_000_000i64, "nsec": 0}, "time_precision_ns": 1, "records": [], "streams": []}
                },
                "link": {"parent": ROOT, "name": b64(name.as_bytes())}
            }]
        }),
    )
    .ok()
    .json()
}

fn wait_for_a_done_run(data: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !gc_status(data).contains(": done, ") {
        assert!(Instant::now() < deadline, "the collection did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A commit checked after a collection read what is kept, while it marks, keeps its chunks: they are
/// vouched for before they are looked at, and the collection takes none of what is vouched for.
#[test]
fn a_commit_made_while_a_collection_marks_keeps_its_chunks() {
    let sb = Sandbox::on_disk("gc-pinned");
    let mut server = Server::start(sb.path("server"));
    let t = token(&server);
    policy(&server, "all 100y");
    let data = b"named once the collection has read what it keeps".to_vec();
    let (m, c) = manifest_of(&server, &t, &data);
    let other = upload_orphans(&server, &t, &[b"an orphan for good".to_vec()]);
    let pause = paused_at(&sb, &mut server, "server.gc.pinned");
    let made = commit_file(&server, &t, "late.txt", m, data.len());
    assert_eq!(made["result"], "committed", "{made}");
    std::fs::write(pause.join("go"), b"").unwrap();
    wait_for_a_done_run(&server.data);
    assert!(chunk_path(&server.data, &m).exists() && chunk_path(&server.data, &c).exists());
    assert!(
        !chunk_path(&server.data, &other[0]).exists(),
        "an orphan the run could take"
    );
    let late = entry(&dir(&server, &t, "root"), "late.txt");
    assert_eq!(
        get(
            &server,
            &format!("/v1/ns/cloud/content/{}", late["object"].as_str().unwrap()),
            Auth::Bearer(&t)
        )
        .ok()
        .body,
        data
    );
    server.fsck();
}

/// A commit that comes while a batch of removals holds the gate waits for it, and is refused for the
/// chunk the batch took, never acknowledged without it; sent again with the chunk, it goes in.
#[test]
fn a_commit_during_a_batch_waits_and_is_refused_rather_than_acknowledged_without_its_chunk() {
    let sb = Sandbox::on_disk("gc-batch");
    let mut server = Server::start(sb.path("server"));
    let t = token(&server);
    policy(&server, "all 100y");
    let data = b"taken by the batch the commit waits for".to_vec();
    let (m, _) = manifest_of(&server, &t, &data);
    let pause = paused_at(&sb, &mut server, "server.gc.batch_locked");
    let answer = std::thread::scope(|s| {
        let waiting = s.spawn(|| commit_file(&server, &t, "waited.txt", m, data.len()));
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !waiting.is_finished(),
            "a commit went on beside a batch of removals"
        );
        std::fs::write(pause.join("go"), b"").unwrap();
        waiting.join().unwrap()
    });
    assert_eq!(answer["result"], "rejected", "{answer}");
    assert_eq!(answer["reject"]["kind"], "missing_chunk", "{answer}");
    wait_for_a_done_run(&server.data);
    manifest_of(&server, &t, &data);
    let again = commit_file(&server, &t, "waited.txt", m, data.len());
    assert_eq!(again["result"], "committed", "{again}");
    server.fsck();
}

/// A phone's upload holds its chunks from before it looks for them until its commit returns: a whole
/// collection between its bytes and its commit takes none of them.
#[test]
fn a_one_request_upload_holds_its_chunks_through_a_whole_collection() {
    let sb = Sandbox::on_disk("gc-held");
    let mut server = Server::start(sb.path("server"));
    let t = token(&server);
    policy(&server, "all 100y");
    let mut rng = StdRng::seed_from_u64(rand::random());
    let bytes = noise(&mut rng, 1_500_000);
    let fresh = upload_orphans(&server, &t, &[b"taken meanwhile".to_vec()]);
    // Its commit a while after its bytes, and a collection in between.
    server.set_env(vec![
        (
            "CAIRN_FP_TUNE".into(),
            format!("server.gc.first=2,server.gc.every=3600,{}", fast(DAY)),
        ),
        ("CAIRN_FP_DELAY".into(), "server.file.stored:5000".into()),
    ]);
    server.restart();
    let started = Instant::now();
    let answer = put_file(&server, &t, "held.bin", &bytes, None);
    assert_eq!(answer.code, 201, "{}", answer.text());
    assert!(started.elapsed() >= Duration::from_secs(5));
    let status = gc_status(&server.data);
    assert!(
        status.contains(": done, "),
        "no collection ran meanwhile: {status}"
    );
    assert!(!chunk_path(&server.data, &fresh[0]).exists(), "{status}");
    let object = answer.json()["object"].as_str().unwrap().to_string();
    assert_eq!(
        get(
            &server,
            &format!("/v1/ns/cloud/content/{object}"),
            Auth::Bearer(&t)
        )
        .ok()
        .body,
        bytes
    );
    server.fsck();
}

/// A store with history to give up, deleted files past their trash period, and orphans: a template the
/// kill sweep copies for each kill.
fn kill_template(sb: &Sandbox) -> PathBuf {
    let mut rng = StdRng::seed_from_u64(7);
    let mut server = Server::start(sb.path("template"));
    let t = token(&server);
    let a = Device::new(sb, &server, "a");
    for v in 0..5 {
        a.write("doc.txt", noise(&mut rng, 2000 + v));
        a.write(&format!("old{v}.txt"), noise(&mut rng, 900));
        a.synced();
    }
    for v in 0..5 {
        remove_file(&a.path(&format!("old{v}.txt")));
    }
    a.synced();
    let orphans: Vec<Vec<u8>> = (0..7).map(|_| noise(&mut rng, 100)).collect();
    upload_orphans(&server, &t, &orphans);
    server.kill();
    policy(&server, "trash 2d");
    // The history given up a day on; the deleted files are given up and collected in the run killed.
    gc(&server.data, &fast(DAY)).expect_ok();
    server.data.clone()
}

/// What a store holds when a run is over: its chunks, and the revisions given up.
fn outcome(data: &Path) -> (BTreeSet<PathBuf>, String) {
    let chunks: BTreeSet<PathBuf> = chunk_files(data)
        .into_iter()
        .map(|p| p.strip_prefix(data).unwrap().to_path_buf())
        .collect();
    let report = fsck(data);
    let pruned = report
        .split(", ")
        .find(|p| p.contains("given up by retention"))
        .unwrap_or_default()
        .to_string();
    (chunks, pruned)
}

/// Killed at every step of a run, retention's and the collection's, and run again: every kept chunk
/// is there at each kill, fsck is clean, and the run after ends where one never killed does.
fn a_collection_killed_at_any_step_keeps_every_kept_chunk_and_starts_over(shard: Shard) {
    let sb = Sandbox::on_disk("gc-kill");
    let template = kill_template(&sb);
    // Every run at one time, three days on, so that each ends where the one never killed does.
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3 * DAY;
    let tune = format!(
        "{},server.gc.at={at},server.retention.batch_rows=2,server.gc.batch_chunks=2",
        fast(0)
    );
    // The run never killed, with every step it takes written down.
    let reference = sb.path("reference");
    copy_tree(&template, &reference);
    let log = sb.path("steps.log");
    let run = run_server(
        &["gc", "--data", reference.to_str().unwrap()],
        &[
            ("CAIRN_FP_TUNE", &tune),
            ("CAIRN_FP_LOG", log.to_str().unwrap()),
        ],
    )
    .expect_ok();
    assert!(run.stdout.contains("removed "), "{}", run.text());
    let want = outcome(&reference);
    let mut steps: BTreeMap<String, u64> = BTreeMap::new();
    for line in std::fs::read_to_string(&log).unwrap().lines() {
        if line.starts_with("server.gc.") || line.starts_with("server.retention.") {
            *steps.entry(line.to_string()).or_default() += 1;
        }
    }
    for point in [
        "server.retention.planned",
        "server.retention.batch",
        "server.retention.pruned",
        "server.gc.begin",
        "server.gc.begun",
        "server.gc.pinned",
        "server.gc.marked",
        "server.gc.deleting",
        "server.gc.batch",
        "server.gc.batch_locked",
        "server.gc.unlink",
        "server.gc.batched",
        "server.gc.done",
    ] {
        assert!(
            steps.contains_key(point),
            "the run never reached {point}: {steps:?}"
        );
    }
    let kills: Vec<String> = steps
        .iter()
        .flat_map(|(p, n)| (1..=(*n).min(3)).map(move |i| format!("{p}#{i}")))
        .collect();
    let failed = each_item(shard, kills.len(), |i| {
        let at = &kills[i];
        let data = sb.path(&format!("k{i}"));
        copy_tree(&template, &data);
        let killed = run_server(
            &["gc", "--data", data.to_str().unwrap()],
            &[("CAIRN_FP_TUNE", &tune), ("CAIRN_FP_CRASH", at)],
        );
        assert!(killed.was_killed(), "{at}: {}", killed.text());
        // Nothing kept is gone at the kill.
        fsck(&data);
        let again = gc(&data, &tune);
        assert!(again.ok(), "{at}: {}", again.text());
        // A run whose row was committed, and not yet ended, is marked so, and the next starts over.
        let unfinished = at.starts_with("server.gc.")
            && !at.starts_with("server.gc.begin#")
            && !at.starts_with("server.gc.done#");
        assert_eq!(
            again.stdout.contains("left unfinished marked so"),
            unfinished,
            "{at}: {}",
            again.text()
        );
        assert_eq!(outcome(&data), want, "{at}");
        let status = gc_status(&data);
        assert_eq!(
            status.contains(": aborted") || status.contains(": interrupted"),
            unfinished,
            "{at}: {status}"
        );
        std::fs::remove_dir_all(&data).unwrap();
    });
    assert!(failed.is_empty(), "{failed:#?}");
}

shards!(a_collection_killed_at_any_step_keeps_every_kept_chunk_and_starts_over: s01 s02 s03);

/// A full disk as a collection would record its start: it says so and removes nothing, and the
/// server that meets it serves on and collects once there is room.
#[test]
fn a_full_disk_as_a_collection_begins_removes_nothing_and_says_so() {
    let sb = Sandbox::on_disk("gc-full");
    let mut server = Server::start(sb.path("server"));
    let t = token(&server);
    let orphans = upload_orphans(&server, &t, &[b"one".to_vec(), b"two".to_vec()]);
    server.kill();
    policy(&server, "all 100y");
    let before = chunk_files(&server.data);
    let full = run_server(
        &["gc", "--data", server.data.to_str().unwrap()],
        &[
            ("CAIRN_FP_TUNE", &fast(DAY)),
            ("CAIRN_FP_ENOSPC", "server.gc.begin"),
        ],
    );
    assert!(!full.ok(), "{}", full.text());
    assert!(
        full.stdout.contains("nothing was removed")
            && full.stdout.contains("No space left on device"),
        "{}",
        full.text()
    );
    assert_eq!(chunk_files(&server.data), before);
    assert!(!gc_status(&server.data).contains("run "), "a run recorded");

    // A server meeting it keeps serving.
    server.set_env(vec![
        (
            "CAIRN_FP_TUNE".into(),
            format!("server.gc.first=0,server.gc.every=1,{}", fast(DAY)),
        ),
        ("CAIRN_FP_ENOSPC".into(), "server.gc.begin".into()),
    ]);
    server.restart();
    std::thread::sleep(Duration::from_millis(2500));
    assert!(server.alive());
    get(&server, "/v1/ns/cloud/info", Auth::Bearer(&t)).ok();
    assert_eq!(chunk_files(&server.data), before);
    server.kill();
    let run = gc(&server.data, &fast(DAY)).expect_ok();
    assert!(run.stdout.contains("removed 2 chunk(s)"), "{}", run.text());
    assert!(orphans
        .iter()
        .all(|h| !chunk_path(&server.data, h).exists()));
    fsck(&server.data);
}

/// A copy of a store as a backup checks it: its database as it was, and its chunks linked to the live
/// store's.
fn backup_copy(sb: &Sandbox, data: &Path) -> PathBuf {
    let copy = sb.path("backup");
    std::fs::create_dir_all(&copy).unwrap();
    let db = rusqlite::Connection::open(data.join("meta.sqlite")).unwrap();
    db.execute(
        "VACUUM INTO ?1",
        [copy.join("meta.sqlite").to_str().unwrap()],
    )
    .unwrap();
    let ns = hex::encode("cloud");
    std::fs::create_dir_all(copy.join("ns").join(&ns)).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        data.join("ns").join(&ns).join("chunks"),
        copy.join("ns").join(&ns).join("chunks"),
    )
    .unwrap();
    copy
}

/// A backup's database, taken before retention gave up what it names, checks clean against the live
/// chunks through the collection's delay, and its check reads the live store alone: the delay, not
/// luck, is what keeps it clean, as a collection past it shows.
#[cfg(unix)]
#[test]
fn a_backup_taken_before_a_prune_checks_clean_against_the_live_chunks() {
    let mut st = story("gc-backup");
    let data = st.server.data.clone();
    st.server.kill();
    let copy = backup_copy(&st.sb, &data);
    policy(&st.server, "trash 1d");
    // The default delay of seven days: given up three days on, not taken four days on.
    let keep = "server.gc.grace=0,server.gc.clock_step=";
    gc(&data, &format!("{keep}{}", 3 * DAY)).expect_ok();
    let run = gc(&data, &format!("{keep}{}", 4 * DAY)).expect_ok();
    assert!(run.stdout.contains("removed 0 chunk(s)"), "{}", run.text());
    let in_flight = data
        .join("ns")
        .join(hex::encode("cloud"))
        .join("incoming")
        .join("upload-in-flight");
    std::fs::write(&in_flight, b"x").unwrap();
    let report = fsck(&copy);
    assert!(report.contains("clean"), "{report}");
    assert!(
        in_flight.exists(),
        "the check of a backup emptied the live store's incoming"
    );
    // Past the delay, what the backup names is gone from the live store.
    gc(&data, &format!("{keep}{}", 11 * DAY)).expect_ok();
    let late = run_server(&["fsck", "--data", copy.to_str().unwrap()], &[]);
    assert!(
        !late.ok() && late.stdout.contains("missing"),
        "{}",
        late.text()
    );
    fsck(&data);
}

/// While a backup holds its lock, a collection removes nothing, says it waits, and goes on once it is
/// let go.
#[cfg(unix)]
#[test]
fn a_backup_holding_its_lock_holds_back_every_removal() {
    let sb = Sandbox::on_disk("gc-lock");
    let mut server = Server::start(sb.path("server"));
    let t = token(&server);
    let orphans = upload_orphans(
        &server,
        &t,
        &[b"held back".to_vec(), b"until the backup ends".to_vec()],
    );
    server.kill();
    policy(&server, "all 100y");
    let lock = std::fs::File::open(server.data.join("backup.lock"))
        .expect("the server makes the backup's lock");
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
    // Killed as it waits: nothing was removed, and the next run starts over.
    let killed = run_server(
        &["gc", "--data", server.data.to_str().unwrap()],
        &[
            ("CAIRN_FP_TUNE", &fast(DAY)),
            ("CAIRN_FP_CRASH", "server.gc.backup_wait"),
        ],
    );
    assert!(killed.was_killed(), "{}", killed.text());
    assert!(orphans.iter().all(|h| chunk_path(&server.data, h).exists()));
    let mut child = std::process::Command::new(SERVER)
        .args(["gc", "--data", server.data.to_str().unwrap()])
        .env("CAIRN_FP_TUNE", fast(DAY))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the collection did not wait for the backup"
    );
    assert!(orphans.iter().all(|h| chunk_path(&server.data, h).exists()));
    assert!(gc_status(&server.data).contains(": deleting, "));
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::Unlock).unwrap();
    let out = child.wait_with_output().unwrap();
    let run = Run::from(out);
    assert!(run.ok(), "{}", run.text());
    assert!(
        run.stderr.contains("a backup holds backup.lock"),
        "{}",
        run.text()
    );
    assert!(orphans
        .iter()
        .all(|h| !chunk_path(&server.data, h).exists()));
    fsck(&server.data);
}

/// A policy set, killed as it commits or starved of disk, changes nothing: neither the policy nor what
/// it would give back.
#[test]
fn a_policy_set_killed_or_on_a_full_disk_changes_nothing() {
    let mut st = story("gc-set");
    let data = st.server.data.clone();
    st.server.kill();
    policy(&st.server, "trash 1d");
    gc(&data, &fast(3 * DAY)).expect_ok();
    let set = |env: &[(&str, &str)]| {
        run_server(
            &[
                "folder",
                "retention",
                "cloud",
                "--none",
                "--data",
                data.to_str().unwrap(),
            ],
            env,
        )
    };
    let killed = set(&[("CAIRN_FP_CRASH", "server.retention.set")]);
    assert!(killed.was_killed(), "{}", killed.text());
    let full = set(&[("CAIRN_FP_ENOSPC", "server.retention.set")]);
    assert!(!full.ok(), "{}", full.text());
    let status = gc_status(&data);
    assert!(status.contains("cloud: trash 1d"), "{status}");
    assert!(status.contains("2 revision(s) given up"), "{status}");
    fsck(&data);
    set(&[]).expect_ok();
    assert!(gc_status(&data).contains("cloud: no policy"));
    st.server.restart();
    assert_eq!(
        content(&st.server, &st.t.clone(), &st.doc, &st.doc_v1)
            .ok()
            .body,
        st.v1
    );
}

/// A device's export and restore of a past state ask what retention gave up first, and refuse, naming
/// it, unless told to leave it out; its list of what was deleted leaves out what is past the trash
/// period, and does not bring it back.
#[test]
fn export_and_restore_refuse_what_retention_removed_unless_told_to_skip_it() {
    let mut st = story("gc-client");
    let t = st.t.clone();
    let head = |server: &Server| {
        get(server, "/v1/ns/cloud/info", Auth::Bearer(&t))
            .ok()
            .json()["head"]
            .as_u64()
            .unwrap()
    };
    // As of sequence 1: doc.txt's first bytes, gone.txt, the link.
    let at = "1".to_string();
    let listed = st.a.cairn(&["deleted"]).expect_ok();
    assert!(listed.stdout.contains("gone.txt"), "{}", listed.text());
    let data = st.server.data.clone();
    st.server.kill();
    policy(&st.server, "trash 1d");
    gc(&data, &fast(3 * DAY)).expect_ok();
    gc(&data, &fast(4 * DAY)).expect_ok();
    st.server.restart();
    let _ = head(&st.server);

    let dest = st.sb.path("export");
    let refused = st.a.cairn(&["export", "--at", &at, dest.to_str().unwrap()]);
    assert!(!refused.ok(), "{}", refused.text());
    assert!(
        refused.stderr.contains("retention removed 2 file(s)")
            && refused.stderr.contains("doc.txt")
            && refused.stderr.contains("--skip-pruned"),
        "{}",
        refused.text()
    );
    let dest = st.sb.path("export2");
    let partial = st.a.cairn(&[
        "export",
        "--at",
        &at,
        "--skip-pruned",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(partial.code, Some(3), "{}", partial.text());
    assert!(
        partial
            .stdout
            .contains("left out: doc.txt, removed by the folder's retention on "),
        "{}",
        partial.text()
    );
    assert!(!dest.join("doc.txt").exists() && !dest.join("gone.txt").exists());
    #[cfg(unix)]
    assert!(std::fs::symlink_metadata(dest.join("link")).is_ok());

    let refused = st.a.cairn(&["restore", ".", "--at", &at, "--no-sync"]);
    assert!(
        !refused.ok() && refused.stderr.contains("--skip-pruned"),
        "{}",
        refused.text()
    );
    let restored =
        st.a.cairn(&["restore", ".", "--at", &at, "--skip-pruned"])
            .expect_ok();
    assert!(
        restored.stdout.contains("left as it is: doc.txt"),
        "{}",
        restored.text()
    );
    assert_eq!(st.a.read("doc.txt"), st.v2);
    let revision = st.doc_v1.clone();
    let one =
        st.a.cairn(&["restore", "doc.txt", "--revision", &revision, "--no-sync"]);
    assert!(
        !one.ok() && one.stderr.contains("removed by the folder's retention on"),
        "{}",
        one.text()
    );

    // Past its trash period, as the server's clock goes, a delete is listed no more nor brought back.
    let ahead = format!("client.clock_ahead={}", 4 * DAY);
    let env = [("CAIRN_FP_TUNE", ahead.as_str())];
    let listed = st.a.cairn_env(&["deleted"], &env).expect_ok();
    assert!(!listed.stdout.contains("gone.txt"), "{}", listed.text());
    let back =
        st.a.cairn_env(&["undelete", &st.gone.clone(), "--no-sync"], &env);
    assert!(
        !back.ok() && back.stderr.contains("past this folder's trash period"),
        "{}",
        back.text()
    );
    st.server.fsck();
}
