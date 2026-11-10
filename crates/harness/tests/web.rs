//! The web interface's endpoints, as a browser or any client uses them: what is uploaded there arrives on
//! every device byte for byte with nothing a device stored lost, nothing is replaced or deleted that the
//! request did not see, and a share or public link reaches what it names and nothing else.

mod common;
use base64::Engine;
use common::*;
use serde_json::{json, Value};
use std::io::{Read, Write};

const ROOT: &str = "00000000000000000000000000000000";

#[derive(Clone, Copy)]
enum Auth<'a> {
    None,
    Bearer(&'a str),
    /// The session cookie, and whether the web's header comes with it.
    Cookie(&'a str, bool),
}

struct Answer {
    code: u16,
    head: String,
    body: Vec<u8>,
}

impl Answer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    #[track_caller]
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {} {}\n{}", self.code, self.head, self.text()))
    }

    fn header(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_lowercase());
        self.head
            .lines()
            .find_map(|l| l.strip_prefix(&want).map(|v| v.trim().to_string()))
    }

    #[track_caller]
    fn ok(self) -> Self {
        assert!(
            (200..300).contains(&self.code),
            "{} {}\n{}",
            self.code,
            self.head,
            self.text()
        );
        self
    }
}

/// One HTTP/1.1 request, read to the end, a chunked body put together.
fn request(
    server: &Server,
    method: &str,
    path: &str,
    auth: Auth,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Answer {
    let addr = server.url.strip_prefix("http://").unwrap();
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    match auth {
        Auth::None => {}
        Auth::Bearer(t) => head.push_str(&format!("Authorization: Bearer {t}\r\n")),
        Auth::Cookie(t, header) => {
            head.push_str(&format!("Cookie: other=1; cairn={t}\r\n"));
            if header {
                head.push_str("X-Cairn-Web: 1\r\n");
            }
        }
    }
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes()).unwrap();
    conn.write_all(body).unwrap();
    let mut raw = Vec::new();
    let _ = conn.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("an answer has a head");
    let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
    let mut body = raw[split + 4..].to_vec();
    let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    if head.contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut rest = &body[..];
        loop {
            let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") else {
                panic!("a chunked body ended early: the server cut it off");
            };
            let len = usize::from_str_radix(std::str::from_utf8(&rest[..eol]).unwrap().trim(), 16)
                .unwrap();
            rest = &rest[eol + 2..];
            if len == 0 {
                break;
            }
            assert!(
                rest.len() >= len + 2,
                "a chunked body ended early: the server cut it off"
            );
            out.extend_from_slice(&rest[..len]);
            rest = &rest[len + 2..];
        }
        body = out;
    }
    Answer { code, head, body }
}

/// A POST whose answer is not waited for.
fn fire(server: &Server, path: &str, token: &str) {
    let addr = server.url.strip_prefix("http://").unwrap();
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let _ = write!(conn, "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let mut sink = Vec::new();
    let _ = conn.read_to_end(&mut sink);
}

fn get(server: &Server, path: &str, auth: Auth) -> Answer {
    request(server, "GET", path, auth, &[], b"")
}

fn post(server: &Server, path: &str, auth: Auth, body: Value) -> Answer {
    request(
        server,
        "POST",
        path,
        auth,
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    )
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

fn token(file: &std::path::Path) -> String {
    std::fs::read_to_string(file).unwrap().trim().to_string()
}

/// The entry named `name` in a listing.
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

fn dir(server: &Server, base: &str, object: &str, auth: Auth) -> Value {
    get(server, &format!("{base}/dir/{object}"), auth)
        .ok()
        .json()
}

/// An upload in parts of `part` bytes, committed, and waited for: its last status.
fn upload(
    server: &Server,
    base: &str,
    auth: Auth,
    (parent, name): (&str, &str),
    data: &[u8],
    replace: Option<&str>,
    part: usize,
) -> Value {
    let mut start = json!({"parent": parent, "name": b64(name), "size": data.len(), "mtime": {"sec": 1_700_000_000i64, "nsec": 0}});
    if let Some(r) = replace {
        start["replace"] = json!(r);
    }
    let began = post(server, &format!("{base}/uploads"), auth, start);
    assert_eq!(began.code, 201, "{}", began.text());
    let id = began.json()["id"].as_str().unwrap().to_string();
    let mut at = 0;
    while at < data.len() || data.is_empty() {
        let end = (at + part).min(data.len());
        let sent = request(
            server,
            "PUT",
            &format!("{base}/uploads/{id}?offset={at}"),
            auth,
            &[],
            &data[at..end],
        )
        .ok();
        assert_eq!(sent.json()["received"], end as u64);
        at = end;
        if data.is_empty() {
            break;
        }
    }
    let committed = request(
        server,
        "POST",
        &format!("{base}/uploads/{id}/commit"),
        auth,
        &[],
        b"",
    );
    assert_eq!(committed.code, 202, "{}", committed.text());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let status = get(server, &format!("{base}/uploads/{id}"), auth)
            .ok()
            .json();
        if status["state"] != "storing" {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an upload still storing after a minute"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Deterministic noise, so that the chunker finds cut points.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

#[test]
fn what_the_web_uploads_arrives_byte_for_byte_and_cut_as_a_client_cuts_it() {
    let sb = Sandbox::fast("web-upload");
    let server = Server::start(sb.path("data"));
    let t = token(&server.token_file);
    let auth = Auth::Bearer(&t);
    let base = "/v1/ns/default";

    let data = noise(5 * 1024 * 1024 + 123, 3);
    let made = post(
        &server,
        &format!("{base}/mkdir"),
        auth,
        json!({"parent": ROOT, "name": b64("Фото")}),
    )
    .ok()
    .json();
    let folder = made["object"].as_str().unwrap().to_string();
    let done = upload(
        &server,
        base,
        auth,
        (&folder, "big.bin"),
        &data,
        None,
        1 << 20,
    );
    assert_eq!(done["state"], "done", "{done}");
    let empty = upload(&server, base, auth, (ROOT, "empty"), b"", None, 1 << 20);
    assert_eq!(empty["state"], "done", "{empty}");

    let a = Device::new(&sb, &server, "a");
    a.synced();
    assert_eq!(a.read("Фото/big.bin"), data);
    assert_eq!(a.read("empty"), b"");
    #[cfg(unix)]
    assert_eq!(mode_of(&a.path("Фото/big.bin")) & 0o7777, 0o644);
    assert_eq!(mtime_of(&a.path("Фото/big.bin")).0, 1_700_000_000);

    // The same manifest a client makes of the same bytes, so dedup holds and no device sends it back.
    let f = std::fs::File::open(a.path("Фото/big.bin")).unwrap();
    let theirs = cairn_engine::chunker::manifest_of(&f, data.len() as u64).unwrap();
    assert_eq!(theirs, cairn_server::web_manifest(&data));
    assert!(theirs.chunks().count() > 1);
    let head = get(&server, &format!("{base}/info"), auth).ok().json()["head"].clone();
    a.synced();
    assert_eq!(
        get(&server, &format!("{base}/info"), auth).ok().json()["head"],
        head
    );

    // Read back whole and by range.
    let file = entry(&dir(&server, base, &folder, auth), "big.bin");
    let id = file["object"].as_str().unwrap();
    assert_eq!(file["size"], data.len() as u64);
    let whole = get(&server, &format!("{base}/content/{id}?download=1"), auth).ok();
    assert_eq!(whole.body, data);
    assert!(whole
        .header("content-disposition")
        .unwrap()
        .starts_with("attachment"));
    assert!(whole
        .header("content-security-policy")
        .unwrap()
        .starts_with("sandbox"));
    let part = request(
        &server,
        "GET",
        &format!("{base}/content/{id}"),
        auth,
        &[("Range", "bytes=3000000-3000009")],
        b"",
    );
    assert_eq!(part.code, 206, "{}", part.text());
    assert_eq!(part.body, &data[3_000_000..3_000_010]);
    assert_eq!(
        part.header("content-range").unwrap(),
        format!("bytes 3000000-3000009/{}", data.len())
    );
    let past = request(
        &server,
        "GET",
        &format!("{base}/content/{id}"),
        auth,
        &[("Range", "bytes=99999999-")],
        b"",
    );
    assert_eq!(past.code, 416);

    // A part that does not follow on is answered with where to go on from, and changes nothing.
    let began = post(
        &server,
        &format!("{base}/uploads"),
        auth,
        json!({"parent": ROOT, "name": b64("x"), "size": 4}),
    )
    .ok()
    .json();
    let up = began["id"].as_str().unwrap();
    let wrong = request(
        &server,
        "PUT",
        &format!("{base}/uploads/{up}?offset=2"),
        auth,
        &[],
        b"ab",
    );
    assert_eq!(wrong.code, 409);
    assert_eq!(wrong.json()["received"], 0);
    let early = request(
        &server,
        "POST",
        &format!("{base}/uploads/{up}/commit"),
        auth,
        &[],
        b"",
    );
    assert_eq!(early.code, 409, "{}", early.text());
    let over = request(
        &server,
        "PUT",
        &format!("{base}/uploads/{up}?offset=0"),
        auth,
        &[],
        b"abcde",
    );
    assert_eq!(over.code, 400);
    assert_eq!(
        request(
            &server,
            "DELETE",
            &format!("{base}/uploads/{up}"),
            auth,
            &[],
            b""
        )
        .code,
        204
    );
    assert_eq!(
        get(&server, &format!("{base}/uploads/{up}"), auth).code,
        404
    );
    server_fsck(server);
}

fn server_fsck(mut server: Server) {
    server.fsck();
}

/// Invariant 2: a file replaced from a browser keeps its mode and extended attributes, which a browser
/// never sees.
#[cfg(unix)]
#[test]
fn a_file_replaced_from_the_web_keeps_what_the_browser_never_saw() {
    let sb = Sandbox::fast("web-keeps");
    let server = Server::start(sb.path("data"));
    let t = token(&server.token_file);
    let auth = Auth::Bearer(&t);
    let base = "/v1/ns/default";
    let a = Device::new(&sb, &server, "a");
    a.write("run.sh", "echo one\n");
    chmod(&a.path("run.sh"), 0o750);
    set_xattr(&a.path("run.sh"), "user.note", b"kept");
    a.synced();

    let file = entry(&dir(&server, base, ROOT, auth), "run.sh");
    assert_eq!(file["mode"], 0o750);
    let done = upload(
        &server,
        base,
        auth,
        (ROOT, "run.sh"),
        b"echo two\n",
        file["revision"].as_str(),
        1 << 20,
    );
    assert_eq!(done["state"], "done", "{done}");
    a.synced();
    assert_eq!(a.read("run.sh"), b"echo two\n");
    assert_eq!(mode_of(&a.path("run.sh")) & 0o7777, 0o750);
    let mut buf = [0u8; 16];
    let n = rustix::fs::getxattr(a.path("run.sh"), "user.note", &mut buf[..]).unwrap();
    assert_eq!(&buf[..n], b"kept");
}

/// Invariant 3: an upload never replaces what its browser did not see, and a name taken is not taken
/// over.
#[test]
fn a_web_upload_never_replaces_what_it_did_not_see() {
    let sb = Sandbox::fast("web-unseen");
    let server = Server::start(sb.path("data"));
    let t = token(&server.token_file);
    let auth = Auth::Bearer(&t);
    let base = "/v1/ns/default";
    let a = Device::new(&sb, &server, "a");
    a.write("doc.txt", "first");
    a.synced();
    let seen = entry(&dir(&server, base, ROOT, auth), "doc.txt")["revision"]
        .as_str()
        .unwrap()
        .to_string();

    // Without `replace`, a taken name is refused at once.
    let taken = post(
        &server,
        &format!("{base}/uploads"),
        auth,
        json!({"parent": ROOT, "name": b64("doc.txt"), "size": 1}),
    );
    assert_eq!(taken.code, 409, "{}", taken.text());

    // The device edits it while the browser's upload is on its way: the upload is refused at its commit.
    let began = post(
        &server,
        &format!("{base}/uploads"),
        auth,
        json!({"parent": ROOT, "name": b64("doc.txt"), "size": 3, "replace": seen}),
    )
    .ok()
    .json();
    let id = began["id"].as_str().unwrap();
    request(
        &server,
        "PUT",
        &format!("{base}/uploads/{id}?offset=0"),
        auth,
        &[],
        b"web",
    )
    .ok();
    a.write("doc.txt", "the device's edit");
    a.synced();
    request(
        &server,
        "POST",
        &format!("{base}/uploads/{id}/commit"),
        auth,
        &[],
        b"",
    );
    let status = loop {
        let s = get(&server, &format!("{base}/uploads/{id}"), auth)
            .ok()
            .json();
        if s["state"] != "storing" {
            break s;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(status["state"], "failed", "{status}");
    assert!(
        status["error"]
            .as_str()
            .unwrap()
            .contains("nothing was replaced"),
        "{status}"
    );
    a.synced();
    assert_eq!(a.read("doc.txt"), b"the device's edit");

    // A delete from a listing older than an edit is refused too, and the edit stays.
    let now = entry(&dir(&server, base, ROOT, auth), "doc.txt");
    let link = json!({"parent": ROOT, "name": b64("doc.txt")});
    let stale = post(
        &server,
        &format!("{base}/delete"),
        auth,
        json!({"object": now["object"], "link": link, "base": seen}),
    );
    assert_eq!(stale.code, 409, "{}", stale.text());
    a.synced();
    assert_eq!(a.read("doc.txt"), b"the device's edit");
}

/// A web upload killed after its chunks are stored and before its commit leaves nothing committed and a
/// clean store; the next server starts with no upload left over, and the same upload then goes through.
#[test]
fn a_web_upload_killed_before_its_commit_leaves_nothing() {
    let sb = Sandbox::fast("web-kill");
    let mut server = Server::start_with(
        sb.path("data"),
        vec![("CAIRN_FP_CRASH".into(), "server.web.upload.stored".into())],
    );
    let t = token(&server.token_file);
    let auth = Auth::Bearer(&t);
    let base = "/v1/ns/default";
    let data = noise(2 * 1024 * 1024, 9);
    let began = post(
        &server,
        &format!("{base}/uploads"),
        auth,
        json!({"parent": ROOT, "name": b64("k.bin"), "size": data.len()}),
    )
    .ok()
    .json();
    let id = began["id"].as_str().unwrap();
    request(
        &server,
        "PUT",
        &format!("{base}/uploads/{id}?offset=0"),
        auth,
        &[],
        &data,
    )
    .ok();
    // The answer may or may not come before the kill.
    fire(&server, &format!("{base}/uploads/{id}/commit"), &t);
    assert!(
        !server.alive_after_a_kill(),
        "the failpoint did not kill the server"
    );
    server.set_env(Vec::new());
    server.restart();
    assert_eq!(
        get(&server, &format!("{base}/info"), auth).ok().json()["head"],
        0
    );
    assert_eq!(
        get(&server, &format!("{base}/uploads/{id}"), auth).code,
        404
    );
    assert_eq!(
        std::fs::read_dir(server.data.join("web/uploads"))
            .unwrap()
            .count(),
        0
    );
    let report = server.fsck();
    assert!(report.contains("clean"), "{report}");
    let done = upload(&server, base, auth, (ROOT, "k.bin"), &data, None, 1 << 20);
    assert_eq!(done["state"], "done", "{done}");
    let a = Device::new(&sb, &server, "a");
    a.synced();
    assert_eq!(a.read("k.bin"), data);
}

/// A full disk while a part arrives answers 507 and keeps nothing of that part; the upload goes on from
/// where it was.
#[test]
fn a_full_disk_during_a_web_upload_is_said_and_loses_nothing() {
    let sb = Sandbox::fast("web-enospc");
    let mut server = Server::start_with(
        sb.path("data"),
        vec![("CAIRN_FP_ENOSPC".into(), "server.web.upload.part".into())],
    );
    let t = token(&server.token_file);
    let auth = Auth::Bearer(&t);
    let base = "/v1/ns/default";
    let began = post(
        &server,
        &format!("{base}/uploads"),
        auth,
        json!({"parent": ROOT, "name": b64("f"), "size": 6}),
    )
    .ok()
    .json();
    let id = began["id"].as_str().unwrap().to_string();
    let full = request(
        &server,
        "PUT",
        &format!("{base}/uploads/{id}?offset=0"),
        auth,
        &[],
        b"abc",
    );
    assert_eq!(full.code, 507, "{}", full.text());
    assert_eq!(
        get(&server, &format!("{base}/uploads/{id}"), auth)
            .ok()
            .json()["received"],
        0
    );

    // The disk has room again: the same server, the failpoint off for its next start.
    server.set_env(Vec::new());
    server.restart();
    let done = upload(&server, base, auth, (ROOT, "f"), b"abcdef", None, 3);
    assert_eq!(done["state"], "done", "{done}");
    let a = Device::new(&sb, &server, "a");
    a.synced();
    assert_eq!(a.read("f"), b"abcdef");

    // A full disk while the chunks are stored fails the upload, says so, and commits nothing.
    server.set_env(vec![(
        "CAIRN_FP_ENOSPC".into(),
        "server.chunk.before_write".into(),
    )]);
    server.restart();
    let failed = upload(
        &server,
        base,
        auth,
        (ROOT, "g"),
        b"new bytes",
        None,
        1 << 20,
    );
    assert_eq!(failed["state"], "failed", "{failed}");
    assert!(
        failed["error"].as_str().unwrap().contains("disk is full"),
        "{failed}"
    );
    server.set_env(Vec::new());
    server.restart();
    assert!(dir(&server, base, ROOT, auth)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["display"] != "g"));
}

/// A folder deleted from the web goes whole in one commit, is listed as deleted once, with what it held,
/// and comes back as itself; a restored revision is a new one (invariant 7).
#[test]
fn deleted_from_the_web_comes_back_and_a_restore_is_a_new_revision() {
    let sb = Sandbox::fast("web-delete");
    let server = Server::start(sb.path("data"));
    let t = token(&server.token_file);
    let auth = Auth::Bearer(&t);
    let base = "/v1/ns/default";
    let a = Device::new(&sb, &server, "a");
    a.write("d/one.txt", "1");
    a.write("d/sub/two.txt", "22");
    a.write("keep.txt", "v1");
    a.synced();
    a.write("keep.txt", "v2");
    a.synced();

    let d = entry(&dir(&server, base, ROOT, auth), "d");
    let link = json!({"parent": ROOT, "name": b64("d")});
    let gone = post(
        &server,
        &format!("{base}/delete"),
        auth,
        json!({"object": d["object"], "link": link, "base": d["revision"]}),
    )
    .ok()
    .json();
    assert_eq!(gone["objects"], 4);
    a.synced();
    assert!(!a.exists("d"));

    let listed = get(&server, &format!("{base}/deleted"), auth).ok().json();
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{listed}");
    assert_eq!(items[0]["display"], "d");
    assert_eq!(items[0]["files"], 2);
    assert_eq!(items[0]["size"], 3);
    assert_eq!(items[0]["path"], "");
    assert_eq!(items[0]["device_name"], "web");
    assert_eq!(listed["complete"], true);

    // Something made at its name since: it comes back beside it.
    a.write("d/new.txt", "n");
    a.synced();
    let back = post(
        &server,
        &format!("{base}/undelete"),
        auth,
        json!({"objects": [d["object"]]}),
    )
    .ok()
    .json();
    assert_eq!(back["items"][0]["renamed"], true, "{back}");
    let path = back["items"][0]["path"].as_str().unwrap().to_string();
    assert!(path.starts_with("d (conflict restored seq "), "{path}");
    a.synced();
    assert_eq!(a.read(&format!("{path}/one.txt")), b"1");
    assert_eq!(a.read(&format!("{path}/sub/two.txt")), b"22");
    assert_eq!(a.read("d/new.txt"), b"n");
    assert!(
        get(&server, &format!("{base}/deleted"), auth).ok().json()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // History lists both revisions; restoring the first makes a third with its bytes.
    let keep = entry(&dir(&server, base, ROOT, auth), "keep.txt");
    let id = keep["object"].as_str().unwrap();
    let history = get(&server, &format!("{base}/history/{id}"), auth)
        .ok()
        .json();
    let revs = history["revisions"].as_array().unwrap();
    assert_eq!(revs.len(), 2, "{history}");
    let first = revs[1]["revision"].as_str().unwrap();
    assert_eq!(
        get(
            &server,
            &format!("{base}/content/{id}?revision={first}"),
            auth
        )
        .ok()
        .body,
        b"v1"
    );
    let restored = post(
        &server,
        &format!("{base}/restore"),
        auth,
        json!({"object": id, "revision": first}),
    )
    .ok()
    .json();
    assert_ne!(restored["revision"], first);
    a.synced();
    assert_eq!(a.read("keep.txt"), b"v1");
    assert_eq!(
        get(&server, &format!("{base}/history/{id}"), auth)
            .ok()
            .json()["revisions"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // A rename, a new folder, and a move into it.
    let made = post(
        &server,
        &format!("{base}/mkdir"),
        auth,
        json!({"parent": ROOT, "name": b64("box")}),
    )
    .ok()
    .json();
    post(&server, &format!("{base}/move"), auth, json!({"object": id, "from": {"parent": ROOT, "name": b64("keep.txt")}, "to": {"parent": made["object"], "name": b64("kept.txt")}})).ok();
    a.synced();
    assert_eq!(a.read("box/kept.txt"), b"v1");
    assert!(!a.exists("keep.txt"));
    // A folder into itself, and `.cairn` in the root, are refused.
    let into = post(
        &server,
        &format!("{base}/move"),
        auth,
        json!({"object": made["object"], "from": {"parent": ROOT, "name": b64("box")}, "to": {"parent": made["object"], "name": b64("box")}}),
    );
    assert_eq!(into.code, 409, "{}", into.text());
    let state = post(
        &server,
        &format!("{base}/mkdir"),
        auth,
        json!({"parent": ROOT, "name": b64(".cairn")}),
    );
    assert_eq!(state.code, 409, "{}", state.text());
}

/// A share with a user and a public link each reach the object they name and what is in it, nothing
/// above or beside it, and nothing of the sync protocol's; a link only reads, and stops once it expires,
/// is removed, its object is deleted, or its maker no longer manages the folder.
#[test]
fn a_share_or_link_reaches_what_it_names_and_nothing_else() {
    let sb = Sandbox::fast("web-shares");
    let mut server = Server::start(sb.path("data"));
    let alice_file = server.user("alice");
    let bob_file = server.user("bob");
    server
        .admin(&["folder", "add", "home", "--manager", "alice"])
        .expect_ok();
    let (alice, bob) = (token(&alice_file), token(&bob_file));
    let (al, bo) = (Auth::Bearer(&alice), Auth::Bearer(&bob));
    let a = Device::as_user(&sb, &server, "a", &alice_file, "home");
    a.write("private/secret.txt", "alice's");
    a.write("out/shared/file.txt", "for bob");
    a.write("out/shared/in/deep.txt", "deep");
    a.write("out/beside.txt", "beside");
    a.synced();
    let base = "/v1/ns/home";
    let out = entry(&dir(&server, base, ROOT, al), "out");
    let shared = entry(
        &dir(&server, base, out["object"].as_str().unwrap(), al),
        "shared",
    );
    let shared_id = shared["object"].as_str().unwrap().to_string();
    let private = entry(&dir(&server, base, ROOT, al), "private");
    let secret = entry(
        &dir(&server, base, private["object"].as_str().unwrap(), al),
        "secret.txt",
    );
    let secret_id = secret["object"].as_str().unwrap().to_string();

    // Only a manager shares; bob has no folder of alice's.
    assert_eq!(
        post(
            &server,
            &format!("{base}/shares"),
            bo,
            json!({"object": shared_id, "user": "bob"})
        )
        .code,
        404
    );
    let share = post(
        &server,
        &format!("{base}/shares"),
        al,
        json!({"object": shared_id, "user": "bob"}),
    );
    assert_eq!(share.code, 201, "{}", share.text());
    let share = share.json();
    assert_eq!(share["path"], "out/shared");
    assert_eq!(share["access"], "read");
    assert!(share.get("link").is_none());
    assert_eq!(
        post(
            &server,
            &format!("{base}/shares"),
            al,
            json!({"object": shared_id, "user": "nobody"})
        )
        .code,
        404
    );
    assert_eq!(
        post(
            &server,
            &format!("{base}/shares"),
            al,
            json!({"object": shared_id, "user": "bob", "access": "manage"})
        )
        .code,
        400
    );

    // What bob sees: the share, from its own root.
    let mine = get(&server, "/v1/shared", bo).ok().json();
    assert_eq!(mine["shares"].as_array().unwrap().len(), 1, "{mine}");
    assert_eq!(mine["shares"][0]["item"]["display"], "shared");
    let sbase = format!("/v1/shared/{}", share["id"].as_str().unwrap());
    let listing = dir(&server, &sbase, "root", bo);
    assert_eq!(listing["path"].as_array().unwrap().len(), 1, "{listing}");
    assert_eq!(listing["path"][0]["display"], "shared");
    let file = entry(&listing, "file.txt");
    let inner = entry(&listing, "in");
    assert_eq!(
        get(
            &server,
            &format!("{sbase}/content/{}", file["object"].as_str().unwrap()),
            bo
        )
        .ok()
        .body,
        b"for bob"
    );
    let deep = dir(&server, &sbase, inner["object"].as_str().unwrap(), bo);
    assert_eq!(deep["path"].as_array().unwrap().len(), 2);
    // Nothing above or beside it, by any route, and nothing of the folder itself.
    for path in [
        format!("{sbase}/content/{secret_id}"),
        format!("{sbase}/item/{secret_id}"),
        format!("{sbase}/dir/{}", out["object"].as_str().unwrap()),
        format!("{sbase}/dir/{ROOT}"),
        format!("{sbase}/history/{secret_id}"),
        format!("{base}/dir/root"),
        format!("{base}/info"),
        format!("{base}/changes"),
        "/v1/shared/00000000000000000000000000000000/dir/root".to_string(),
    ] {
        assert_eq!(get(&server, &path, bo).code, 404, "{path}");
    }
    // It reads only, and alice's share is not someone else's: carol does not see it.
    let mk = post(
        &server,
        &format!("{sbase}/mkdir"),
        bo,
        json!({"parent": shared_id, "name": b64("x")}),
    );
    assert_eq!(mk.code, 403, "{}", mk.text());
    let carol_file = server.user("carol");
    let carol = token(&carol_file);
    assert_eq!(
        get(&server, &format!("{sbase}/dir/root"), Auth::Bearer(&carol)).code,
        404
    );

    // A share to write: bob makes a folder in it, but cannot take the share's own root away, nor move
    // anything out of it.
    let w = post(
        &server,
        &format!("{base}/shares"),
        al,
        json!({"object": shared_id, "user": "bob", "access": "write"}),
    )
    .ok()
    .json();
    let wbase = format!("/v1/shared/{}", w["id"].as_str().unwrap());
    post(
        &server,
        &format!("{wbase}/mkdir"),
        bo,
        json!({"parent": shared_id, "name": b64("from bob")}),
    )
    .ok();
    let root_gone = post(
        &server,
        &format!("{wbase}/delete"),
        bo,
        json!({"object": shared_id, "link": {"parent": out["object"], "name": b64("shared")}}),
    );
    assert_eq!(root_gone.code, 404, "{}", root_gone.text());
    let moved_out = post(
        &server,
        &format!("{wbase}/move"),
        bo,
        json!({"object": file["object"], "from": {"parent": shared_id, "name": b64("file.txt")}, "to": {"parent": out["object"], "name": b64("file.txt")}}),
    );
    assert_eq!(moved_out.code, 404, "{}", moved_out.text());
    a.synced();
    assert!(a.path("out/shared/from bob").is_dir());
    assert_eq!(a.read("out/shared/file.txt"), b"for bob");

    // A public link, without any token.
    let link = post(
        &server,
        &format!("{base}/shares"),
        al,
        json!({"object": shared_id}),
    )
    .ok()
    .json();
    let token_part = link["link"].as_str().unwrap().to_string();
    assert!(token_part.len() >= 43, "{link}");
    let lbase = format!("/v1/links/{token_part}");
    let info = get(&server, &lbase, Auth::None).ok().json();
    assert_eq!(info["item"]["display"], "shared");
    let listing = dir(&server, &lbase, "root", Auth::None);
    assert_eq!(listing["access"], "read");
    let file_id = entry(&listing, "file.txt")["object"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        get(&server, &format!("{lbase}/content/{file_id}"), Auth::None)
            .ok()
            .body,
        b"for bob"
    );
    for path in [
        format!("{lbase}/content/{secret_id}"),
        format!("{lbase}/history/{file_id}"),
        format!("{lbase}/dir/{}", out["object"].as_str().unwrap()),
    ] {
        assert_eq!(get(&server, &path, Auth::None).code, 404, "{path}");
    }
    assert_eq!(
        post(
            &server,
            &format!("{lbase}/mkdir"),
            Auth::None,
            json!({"parent": shared_id, "name": b64("y")})
        )
        .code,
        403
    );
    assert_eq!(
        post(
            &server,
            &format!("{base}/shares"),
            al,
            json!({"object": shared_id, "access": "write"})
        )
        .code,
        400
    );
    assert_eq!(
        get(&server, &format!("/s/{token_part}"), Auth::None).code,
        200
    );

    // The folder as a zip, which any unzip reads: names, bytes and CRCs.
    let zip = get(&server, &format!("{lbase}/zip/root"), Auth::None).ok();
    let files = read_zip(&zip.body);
    assert_eq!(
        files.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        [
            "shared/",
            "shared/file.txt",
            "shared/from bob/",
            "shared/in/",
            "shared/in/deep.txt"
        ]
    );
    assert_eq!(files[1].1, b"for bob");
    assert_eq!(files[4].1, b"deep");

    // A link gives what is there now: not a revision before, which history would name.
    let first = get(&server, &format!("{base}/history/{file_id}"), al)
        .ok()
        .json()["revisions"][0]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    a.write("out/shared/file.txt", "for bob, again");
    a.synced();
    assert_eq!(
        get(
            &server,
            &format!("{lbase}/content/{file_id}?revision={first}"),
            Auth::None
        )
        .code,
        404
    );
    assert_eq!(
        get(
            &server,
            &format!("{base}/content/{file_id}?revision={first}"),
            al
        )
        .ok()
        .body,
        b"for bob"
    );
    assert_eq!(
        get(&server, &format!("{lbase}/content/{file_id}"), Auth::None)
            .ok()
            .body,
        b"for bob, again"
    );

    // Opened at the server's own address, unless it was told one for links: a door that passes only them.
    assert!(link.get("url").is_none(), "{link}");
    server.set_args(&["--link-url", "https://links.example:8443/"]);
    server.restart();
    let listed = get(&server, &format!("{base}/shares?object={shared_id}"), al)
        .ok()
        .json();
    let ours = listed["shares"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == link["id"])
        .unwrap()
        .clone();
    assert_eq!(
        ours["url"],
        format!("https://links.example:8443/s/{token_part}")
    );
    assert!(listed["shares"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["user"].is_null() || s.get("url").is_none()));
    server.set_args(&[]);
    server.restart();

    // Listed for alice, and gone once removed.
    let all = get(&server, &format!("{base}/shares"), al).ok().json();
    assert_eq!(all["shares"].as_array().unwrap().len(), 3);
    assert_eq!(get(&server, &format!("{base}/shares"), bo).code, 404);
    let link_id = link["id"].as_str().unwrap();
    assert_eq!(
        request(
            &server,
            "DELETE",
            &format!("{base}/shares/{link_id}"),
            al,
            &[],
            b""
        )
        .code,
        204
    );
    assert_eq!(get(&server, &lbase, Auth::None).code, 404);

    // A link that expires, and one whose object is deleted, then brought back.
    let soon = post(
        &server,
        &format!("{base}/shares"),
        al,
        json!({"object": file_id, "expires": now_secs() + 2}),
    )
    .ok()
    .json();
    let soon_base = format!("/v1/links/{}", soon["link"].as_str().unwrap());
    assert_eq!(
        get(&server, &soon_base, Auth::None).ok().json()["item"]["kind"],
        "file"
    );
    assert_eq!(
        get(&server, &format!("{soon_base}/content/root"), Auth::None)
            .ok()
            .body,
        b"for bob, again"
    );
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(get(&server, &soon_base, Auth::None).code, 404);
    let later = post(
        &server,
        &format!("{base}/shares"),
        al,
        json!({"object": file_id}),
    )
    .ok()
    .json();
    let later_base = format!("/v1/links/{}", later["link"].as_str().unwrap());
    std::fs::remove_file(a.path("out/shared/file.txt")).unwrap();
    a.synced();
    assert_eq!(get(&server, &later_base, Auth::None).code, 404);
    post(
        &server,
        &format!("{base}/undelete"),
        al,
        json!({"objects": [file_id]}),
    )
    .ok();
    assert_eq!(
        get(&server, &format!("{later_base}/content/root"), Auth::None)
            .ok()
            .body,
        b"for bob, again"
    );

    // Its maker no longer managing the folder ends every share it made.
    server
        .admin(&["grant", "home", "bob", "manage"])
        .expect_ok();
    server
        .admin(&["grant", "home", "alice", "write"])
        .expect_ok();
    assert_eq!(get(&server, &later_base, Auth::None).code, 404);
    assert!(
        get(&server, "/v1/shared", Auth::Bearer(&bob)).ok().json()["shares"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    c ^ 0xFFFF_FFFF
}

/// A zip read as a streaming unzip reads it, each local header's sizes and CRC checked against its bytes,
/// then its central directory's count against what was read.
fn read_zip(z: &[u8]) -> Vec<(String, Vec<u8>)> {
    let u16le = |at: usize| u16::from_le_bytes(z[at..at + 2].try_into().unwrap()) as usize;
    let u32le = |at: usize| u32::from_le_bytes(z[at..at + 4].try_into().unwrap());
    let mut out = Vec::new();
    let mut at = 0;
    while u32le(at) == 0x0403_4b50 {
        let crc = u32le(at + 14);
        let size = u32le(at + 18) as usize;
        assert_eq!(
            u32le(at + 22) as usize,
            size,
            "stored: compressed and plain sizes agree"
        );
        assert_eq!(u16le(at + 8), 0, "stored");
        assert_eq!(
            u16le(at + 6) & 0x0008,
            0,
            "no data descriptor: the header holds the sizes"
        );
        let (nlen, elen) = (u16le(at + 26), u16le(at + 28));
        let name = String::from_utf8(z[at + 30..at + 30 + nlen].to_vec()).unwrap();
        let data = z[at + 30 + nlen + elen..at + 30 + nlen + elen + size].to_vec();
        assert_eq!(crc32(&data), crc, "{name}");
        out.push((name, data));
        at += 30 + nlen + elen + size;
    }
    let end = z.len() - 22;
    assert_eq!(u32le(end), 0x0605_4b50, "the end record is last");
    assert_eq!(u16le(end + 10), out.len());
    assert_eq!(
        u32le(end + 16) as usize,
        at,
        "the central directory follows the last entry"
    );
    out
}

/// A browser signs in with its token as a cookie, which reads; changing anything needs the web's header
/// too, so another site's page that makes the browser send a request changes nothing.
#[test]
fn the_session_cookie_reads_and_changes_nothing_without_the_webs_header() {
    let sb = Sandbox::fast("web-session");
    let server = Server::start(sb.path("data"));
    let t = token(&server.token_file);
    let wrong = post(
        &server,
        "/v1/session",
        Auth::None,
        json!({"token": "not-it"}),
    );
    assert_eq!(wrong.code, 401);
    assert!(wrong.header("set-cookie").is_none());
    let signed = post(&server, "/v1/session", Auth::None, json!({"token": t})).ok();
    let cookie = signed.header("set-cookie").unwrap();
    for part in ["httponly", "secure", "samesite=strict", "path=/"] {
        assert!(cookie.contains(part), "{cookie}");
    }
    assert_eq!(
        signed.header("referrer-policy").as_deref(),
        Some("no-referrer")
    );

    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie(&t, false)).ok().json()["user"],
        "admin"
    );
    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie("0123", false)).code,
        401
    );
    let forged = post(
        &server,
        "/v1/ns/default/mkdir",
        Auth::Cookie(&t, false),
        json!({"parent": ROOT, "name": b64("x")}),
    );
    assert_eq!(forged.code, 403, "{}", forged.text());
    let commit = post(
        &server,
        "/v1/ns/default/commit",
        Auth::Cookie(&t, false),
        json!({}),
    );
    assert_eq!(commit.code, 403);
    assert_eq!(
        get(&server, "/v1/ns/default/info", Auth::Cookie(&t, false))
            .ok()
            .json()["head"],
        0
    );
    post(
        &server,
        "/v1/ns/default/mkdir",
        Auth::Cookie(&t, true),
        json!({"parent": ROOT, "name": b64("x")}),
    )
    .ok();
    assert_eq!(
        get(&server, "/v1/ns/default/info", Auth::Cookie(&t, false))
            .ok()
            .json()["head"],
        1
    );

    let out = request(&server, "DELETE", "/v1/session", Auth::None, &[], b"");
    assert_eq!(out.code, 204);
    assert!(out.header("set-cookie").unwrap().contains("max-age=0"));

    // The page and its files answer without a token, and say what may run.
    let page = get(&server, "/", Auth::None).ok();
    assert!(page
        .header("content-security-policy")
        .unwrap()
        .contains("script-src 'self'"));
    for file in ["/web/app.js", "/web/app.css", "/web/icon.svg"] {
        assert_eq!(get(&server, file, Auth::None).code, 200, "{file}");
    }
    assert_ne!(get(&server, "/web/../meta.sqlite", Auth::None).code, 200);
}
