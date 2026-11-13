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

/// The session a sign-in's answer sets, as a browser would send it back.
#[track_caller]
fn session_of(signed: &Answer) -> String {
    let cookie = signed
        .header("set-cookie")
        .expect("a sign-in sets the cookie");
    let value = cookie
        .strip_prefix("cairn=")
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert!(!value.is_empty(), "{cookie}");
    value.to_string()
}

/// The ticket in a sign-in address, `<server>/#/signin/<ticket>/f/<folder>/root`.
#[track_caller]
fn ticket_in(url: &str, server: &Server) -> String {
    let rest = url
        .strip_prefix(&format!("{}/#/signin/", server.url))
        .unwrap_or_else(|| panic!("{url} is no sign-in address of {}", server.url));
    let (ticket, route) = rest.split_once('/').unwrap();
    assert_eq!(route, "f/cloud/root", "{url}");
    ticket.to_string()
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
    let base = "/v1/ns/cloud";

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
    let base = "/v1/ns/cloud";
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
    let base = "/v1/ns/cloud";
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
    let base = "/v1/ns/cloud";
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
    let base = "/v1/ns/cloud";
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
    let base = "/v1/ns/cloud";
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

/// A browser signs in with its token to a session cookie, which reads; changing anything needs the web's
/// header too, so another site's page that makes the browser send a request changes nothing.
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
    let s = session_of(&signed);
    assert!(
        !cookie.contains(&t),
        "the token itself is never in the browser: {cookie}"
    );
    for part in ["httponly", "secure", "samesite=strict", "path=/"] {
        assert!(cookie.contains(part), "{cookie}");
    }
    assert_eq!(
        signed.header("referrer-policy").as_deref(),
        Some("no-referrer")
    );

    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie(&s, false)).ok().json()["user"],
        "admin"
    );
    for not_one in ["0123", &t] {
        assert_eq!(
            get(&server, "/v1/me", Auth::Cookie(not_one, false)).code,
            401
        );
    }
    let forged = post(
        &server,
        "/v1/ns/cloud/mkdir",
        Auth::Cookie(&s, false),
        json!({"parent": ROOT, "name": b64("x")}),
    );
    assert_eq!(forged.code, 403, "{}", forged.text());
    let commit = post(
        &server,
        "/v1/ns/cloud/commit",
        Auth::Cookie(&s, false),
        json!({}),
    );
    assert_eq!(commit.code, 403);
    assert_eq!(
        get(&server, "/v1/ns/cloud/info", Auth::Cookie(&s, false))
            .ok()
            .json()["head"],
        0
    );
    post(
        &server,
        "/v1/ns/cloud/mkdir",
        Auth::Cookie(&s, true),
        json!({"parent": ROOT, "name": b64("x")}),
    )
    .ok();
    assert_eq!(
        get(&server, "/v1/ns/cloud/info", Auth::Cookie(&s, false))
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

/// A program with a token, the tray or `cairn-sync web`, signs a browser in by a ticket: once, and only by the
/// server that made it. The session names the token and holds it nowhere, outlives a restart, and ends
/// when the token is removed. A device's daemon gives the same address, as the tray asks for it.
#[test]
fn a_ticket_signs_a_browser_in_once_and_its_session_ends_with_its_token() {
    let sb = Sandbox::fast("web-ticket");
    let mut server = Server::start(sb.path("data"));
    let admin = token(&server.token_file);
    let other = sb.path("laptop.token");
    server
        .admin(&[
            "token",
            "add",
            "admin",
            "--label",
            "laptop",
            "--token-file",
            other.to_str().unwrap(),
        ])
        .expect_ok();
    let laptop = token(&other);

    let none = post(&server, "/v1/session/ticket", Auth::None, json!({}));
    assert_eq!(none.code, 401, "{}", none.text());
    let asked = post(
        &server,
        "/v1/session/ticket",
        Auth::Bearer(&laptop),
        json!({}),
    )
    .ok()
    .json();
    assert_eq!(asked["expires_in"], 120, "{asked}");
    let ticket = asked["ticket"].as_str().unwrap().to_string();
    let signed = post(
        &server,
        "/v1/session",
        Auth::None,
        json!({ "ticket": ticket }),
    )
    .ok();
    let session = session_of(&signed);
    for secret in [&ticket, &laptop, &admin] {
        assert!(!session.contains(secret.as_str()), "{session}");
    }
    let again = post(
        &server,
        "/v1/session",
        Auth::None,
        json!({ "ticket": ticket }),
    );
    assert_eq!(again.code, 401, "a ticket signs in once: {}", again.text());
    assert!(
        again.text().contains("used or has expired"),
        "{}",
        again.text()
    );
    assert!(again.header("set-cookie").is_none());
    for wrong in [
        json!({}),
        json!({ "ticket": "x", "token": admin }),
        json!({ "ticket": "nope" }),
    ] {
        assert_eq!(
            post(&server, "/v1/session", Auth::None, wrong.clone()).code,
            401,
            "{wrong}"
        );
    }
    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie(&session, false))
            .ok()
            .json()["user"],
        "admin"
    );
    // Its end, or its token, changed by hand is no session.
    let (hash, rest) = session.split_once('.').unwrap();
    let (expires, mac) = rest.split_once('.').unwrap();
    let longer = format!("{hash}.{}.{mac}", expires.parse::<u64>().unwrap() + 86_400);
    let admins = format!(
        "{}.{expires}.{mac}",
        hex::encode(cairn_protocol::Hash::of(admin.as_bytes()).0)
    );
    for forged in [longer, admins] {
        assert_eq!(
            get(&server, "/v1/me", Auth::Cookie(&forged, false)).code,
            401
        );
    }

    // The key it was signed with is kept, readable by the server alone, so a restart signs nobody out.
    let key = server.data.join("web/session.key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    server.restart();
    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie(&session, false))
            .ok()
            .json()["user"],
        "admin"
    );
    // A ticket waits only in memory: one asked for before a restart is gone after it.
    let before = post(
        &server,
        "/v1/session/ticket",
        Auth::Bearer(&laptop),
        json!({}),
    )
    .ok()
    .json()["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    server.restart();
    assert_eq!(
        post(
            &server,
            "/v1/session",
            Auth::None,
            json!({ "ticket": before })
        )
        .code,
        401
    );

    // The token removed, every browser it signed in is signed out; the other token's are not.
    let by_admin = post(
        &server,
        "/v1/session/ticket",
        Auth::Bearer(&admin),
        json!({}),
    )
    .ok()
    .json()["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    let admin_session = session_of(
        &post(
            &server,
            "/v1/session",
            Auth::None,
            json!({ "ticket": by_admin }),
        )
        .ok(),
    );
    let list = server.admin(&["token", "list"]).expect_ok();
    let id = list
        .stdout
        .lines()
        .find(|l| l.contains("laptop"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_else(|| panic!("{}", list.text()))
        .to_string();
    server.admin(&["token", "remove", &id]).expect_ok();
    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie(&session, false)).code,
        401
    );
    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie(&admin_session, false))
            .ok()
            .json()["user"],
        "admin"
    );

    // `cairn-sync web` gives the address it would open, at this root's folder.
    let d = Device::new(&sb, &server, "d");
    let printed = d.cairn(&["web", "--print"]).expect_ok();
    let ticket = ticket_in(printed.stdout.trim(), &server);
    post(
        &server,
        "/v1/session",
        Auth::None,
        json!({ "ticket": ticket }),
    )
    .ok();

    // The daemon gives one too, as the tray asks it for its Open in browser.
    let log = sb.path("daemon.log");
    let out = std::fs::File::create(&log).unwrap();
    let mut daemon = std::process::Command::new(CAIRN)
        .arg("--root")
        .arg(&d.root)
        .arg("daemon")
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    let reply = loop {
        let run = d.cairn(&["ask", "sign_in_link"]);
        if run.ok() {
            break serde_json::from_str::<Value>(run.stdout.trim()).unwrap();
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{}\n{}",
            run.text(),
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert_eq!(reply["reply"], "sign_in_link", "{reply}");
    let ticket = ticket_in(reply["url"].as_str().unwrap(), &server);
    let signed = post(
        &server,
        "/v1/session",
        Auth::None,
        json!({ "ticket": ticket }),
    )
    .ok();
    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie(&session_of(&signed), false))
            .ok()
            .json()["user"],
        "admin"
    );
}

/// Each user's own name for a folder: theirs alone, offered by a device only while they have none, set
/// or taken away in the web; `cairn-sync init` without --folder joins the only folder there is and asks
/// which of several; and a folder renamed answers by its former name, to someone without access as for
/// a folder that does not exist.
#[test]
fn each_user_names_a_folder_for_themselves_and_init_asks_which_of_several() {
    let sb = Sandbox::fast("web-names");
    let mut server = Server::start(sb.path("data"));
    let admin = token(&server.token_file);
    let bob_file = server.user("bob");
    let bob = token(&bob_file);
    server.admin(&["grant", "cloud", "bob", "read"]).expect_ok();
    let me = |t: &str| get(&server, "/v1/me", Auth::Bearer(t)).ok().json();
    let name = |t: &str, body: Value| {
        request(
            &server,
            "PUT",
            "/v1/ns/cloud/name",
            Auth::Bearer(t),
            &[("Content-Type", "application/json")],
            body.to_string().as_bytes(),
        )
    };
    assert_eq!(
        me(&admin)["folders"],
        json!([{"folder": "cloud", "access": "manage"}])
    );

    // A device's offer is taken while there is none, and only then; a choice always.
    let a = Device::new(&sb, &server, "Мои файлы");
    assert_eq!(me(&admin)["folders"][0]["name"], "Мои файлы");
    let got = name(&admin, json!({"name": "other", "if_none": true}))
        .ok()
        .json();
    assert_eq!(got["name"], "Мои файлы");
    let got = name(&admin, json!({"name": "Family photos"})).ok().json();
    assert_eq!(got["name"], "Family photos");
    for bad in ["", " padded", "a\nb"] {
        let r = name(&admin, json!({ "name": bad }));
        assert_eq!(r.code, 400, "{bad:?}: {}", r.text());
    }
    // Theirs alone: bob, who may only read, has his own, and sees none of the admin's.
    assert_eq!(
        me(&bob)["folders"],
        json!([{"folder": "cloud", "access": "read"}])
    );
    name(&bob, json!({"name": "Bob's"})).ok();
    assert_eq!(me(&bob)["folders"][0]["name"], "Bob's");
    assert_eq!(me(&admin)["folders"][0]["name"], "Family photos");
    let r = request(
        &server,
        "DELETE",
        "/v1/ns/cloud/name",
        Auth::Bearer(&admin),
        &[],
        b"",
    );
    assert_eq!(r.code, 204, "{}", r.text());
    assert_eq!(
        me(&admin)["folders"],
        json!([{"folder": "cloud", "access": "manage"}])
    );
    // No access, no name: as for a folder that does not exist.
    let r = request(
        &server,
        "PUT",
        "/v1/ns/nowhere/name",
        Auth::Bearer(&admin),
        &[("Content-Type", "application/json")],
        br#"{"name":"x"}"#,
    );
    assert_eq!(r.code, 404, "{}", r.text());

    // Several folders: init asks which, naming each with the user's own name for it.
    server
        .admin(&["folder", "add", "work", "--manager", "admin"])
        .expect_ok();
    let root = sb.path("several");
    std::fs::create_dir_all(&root).unwrap();
    let run = run(
        &[
            "init",
            "--server",
            &server.url,
            "--token-file",
            server.token_file.to_str().unwrap(),
            root.to_str().unwrap(),
        ],
        &[],
    );
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.stderr
            .contains("access to 2 folders, so say which with --folder: cloud (Мои файлы), work")
            || run
                .stderr
                .contains("access to 2 folders, so say which with --folder: cloud, work"),
        "{}",
        run.text()
    );
    assert!(!root.join(".cairn").exists(), "nothing made of it");

    // Renamed: the former name reaches it for whoever has access, and for nobody else.
    server.kill();
    server
        .admin(&["folder", "rename", "work", "office"])
        .expect_ok();
    server.restart();
    let info = get(&server, "/v1/ns/work/info", Auth::Bearer(&admin))
        .ok()
        .json();
    assert_eq!(info["folder"], "office");
    let r = get(&server, "/v1/ns/work/info", Auth::Bearer(&bob));
    assert_eq!(r.code, 404);
    assert_eq!(r.text(), "no folder \"work\" for user \"bob\"");
    drop(a);
}

/// A request whose answer is not waited for, as one a server is killed in the middle of.
fn fire_json(server: &Server, path: &str, token: &str, body: Value) {
    let addr = server.url.strip_prefix("http://").unwrap();
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let body = body.to_string();
    let _ = write!(
        conn,
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut sink = Vec::new();
    let _ = conn.read_to_end(&mut sink);
}

/// Until the paused process says it reached its failpoint.
#[track_caller]
fn wait_reached(gate: &std::path::Path, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !gate.join("reached").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: never reached"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// A user adds a folder of their own, which they manage, all at once or not at all, through a kill or
/// a full disk, and learns of a name that is taken nothing but that it is; and a manager renames a
/// folder while the server serves, with a commit and a web upload begun by its former name in flight
/// across it: both land in it, devices follow it and lose nothing, what is deleted is listed by the
/// name now, the chunks never move, and the server started again has the names it had in memory.
#[test]
fn a_user_adds_a_folder_and_a_manager_renames_one_while_it_serves_losing_nothing() {
    let sb = Sandbox::fast("web-folders");
    let mut server = Server::start(sb.path("data"));
    let admin = token(&server.token_file);
    let bob = token(&server.user("bob"));
    let me = |server: &Server, t: &str| get(server, "/v1/me", Auth::Bearer(t)).ok().json();
    let add =
        |server: &Server, t: &str, body: Value| post(server, "/v1/folders", Auth::Bearer(t), body);

    // Bob's own folder: his to manage, by his own name for it, and nobody else's to see.
    let made = add(
        &server,
        &bob,
        json!({"folder": "bobs", "name": "Bob's things"}),
    );
    assert_eq!(made.code, 201, "{}", made.text());
    assert_eq!(
        made.json(),
        json!({"folder": "bobs", "access": "manage", "name": "Bob's things"})
    );
    assert_eq!(
        me(&server, &bob)["folders"],
        json!([{"folder": "bobs", "access": "manage", "name": "Bob's things"}])
    );
    assert_eq!(
        me(&server, &admin)["folders"],
        json!([{"folder": "cloud", "access": "manage"}])
    );
    assert_eq!(
        get(&server, "/v1/ns/bobs/info", Auth::Bearer(&admin)).code,
        404
    );
    assert_eq!(
        add(&server, &bob, json!({"folder": "plain"})).code,
        201,
        "no name of his own"
    );
    // Taken: said, and nothing of whose it is or what it holds.
    let taken = add(&server, &bob, json!({"folder": "cloud"}));
    assert_eq!(taken.code, 409, "{}", taken.text());
    assert_eq!(
        taken.text(),
        "the name \"cloud\" is taken by another folder, now or before: pick another"
    );
    for bad in [
        json!({"folder": ""}),
        json!({"folder": "two words"}),
        json!({"folder": ".hidden"}),
        json!({"folder": "-x"}),
        json!({"folder": "x".repeat(65)}),
        json!({"folder": "ok", "name": " padded"}),
        json!({"folder": "ok", "name": ""}),
    ] {
        let r = add(&server, &bob, bad.clone());
        assert_eq!(r.code, 400, "{bad}: {}", r.text());
    }
    let folders = |server: &Server| server.admin(&["folder", "list"]).expect_ok().stdout;
    let listed = folders(&server);
    assert_eq!(listed.lines().count(), 3, "{listed}");

    // Killed before its commit, or a full disk at it: nothing made, and the name free.
    server.set_env(vec![(
        "CAIRN_FP_CRASH".into(),
        "server.folder_add.commit".into(),
    )]);
    server.restart();
    fire_json(&server, "/v1/folders", &bob, json!({"folder": "crashed"}));
    assert!(
        !server.alive_after_a_kill(),
        "the failpoint did not kill the server"
    );
    server.set_env(vec![(
        "CAIRN_FP_ENOSPC".into(),
        "server.folder_add.write".into(),
    )]);
    server.restart();
    let full = add(&server, &bob, json!({"folder": "full", "name": "Full"}));
    assert_eq!(full.code, 507, "{}", full.text());
    server.set_env(Vec::new());
    server.restart();
    assert_eq!(folders(&server), listed);
    assert_eq!(me(&server, &bob)["folders"].as_array().unwrap().len(), 2);
    for name in ["crashed", "full"] {
        assert_eq!(
            add(&server, &bob, json!({ "folder": name })).code,
            201,
            "{name}"
        );
    }

    // The first folder on two devices, which bob may read.
    server.admin(&["grant", "cloud", "bob", "read"]).expect_ok();
    let a = Device::new(&sb, &server, "a");
    let b = Device::new(&sb, &server, "b");
    for i in 0..20 {
        a.write(&format!("d{}/f{i}", i % 3), format!("file {i}"));
    }
    converge(&[&a, &b]);
    let rename = |server: &Server, t: &str, ns: &str, to: &str| {
        post(
            server,
            &format!("/v1/ns/{ns}/rename"),
            Auth::Bearer(t),
            json!({ "folder": to }),
        )
    };
    let r = rename(&server, &bob, "cloud", "mine");
    assert_eq!(r.code, 403, "{}", r.text());
    let r = rename(&server, &admin, "cloud", "bobs");
    assert_eq!(r.code, 409, "a folder the admin cannot reach: {}", r.text());
    assert!(!r.text().contains("Bob"), "{}", r.text());
    assert_eq!(rename(&server, &admin, "cloud", "no good").code, 400);
    assert_eq!(rename(&server, &admin, "bobs", "x").code, 404);
    // Beside a server that serves, the administrator's command is refused, as it would not know.
    let run = server.admin(&["folder", "rename", "cloud", "photos"]);
    assert!(
        !run.ok() && run.stderr.contains("stop it first"),
        "{}",
        run.text()
    );

    // A commit whose name and access were found by "cloud", paused there while it becomes "photos".
    let gate = sb.path("gate-commit");
    std::fs::create_dir(&gate).unwrap();
    server.set_env(vec![(
        "CAIRN_FP_PAUSE".into(),
        format!("server.commit.received#1:{}", gate.display()),
    )]);
    server.restart();
    a.write("during/commit.txt", "committed across the rename");
    std::fs::remove_file(a.path("d0/f0")).unwrap();
    let syncing = {
        let a = a.clone();
        std::thread::spawn(move || a.sync())
    };
    wait_reached(&gate, "the commit");
    let r = rename(&server, &admin, "cloud", "photos").ok();
    assert_eq!(r.json(), json!({"folder": "photos"}));
    std::fs::write(gate.join("go"), b"").unwrap();
    syncing.join().unwrap().expect_ok();
    assert_eq!(
        get(&server, "/v1/ns/cloud/info", Auth::Bearer(&admin))
            .ok()
            .json()["folder"],
        "photos"
    );
    assert_eq!(
        get(&server, "/v1/ns/cloud/info", Auth::Bearer(&bob))
            .ok()
            .json()["folder"],
        "photos",
        "a reader learns the name now too"
    );
    let listed = folders(&server);
    assert!(
        listed.contains("photos  at ") && listed.contains("formerly cloud"),
        "{listed}"
    );
    // What was deleted by the commit begun as "cloud" is listed by either name.
    for ns in ["photos", "cloud"] {
        let gone = get(
            &server,
            &format!("/v1/ns/{ns}/deleted"),
            Auth::Bearer(&admin),
        )
        .ok()
        .json();
        let items = gone["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["display"] == "f0"), "{ns}: {gone}");
    }
    let run = a.synced();
    assert!(
        run.stdout
            .contains("is called \"photos\" now, and this root follows it"),
        "{}",
        run.text()
    );
    b.synced();
    assert_eq!(b.read("during/commit.txt"), b"committed across the rename");
    assert!(!b.exists("d0/f0"));

    // A web upload begun as "photos", its chunks stored, paused before its commit while it becomes
    // "pics": it lands, and its status is read by either name.
    let gate = sb.path("gate-upload");
    std::fs::create_dir(&gate).unwrap();
    server.set_env(vec![(
        "CAIRN_FP_PAUSE".into(),
        format!("server.web.upload.stored#1:{}", gate.display()),
    )]);
    server.restart();
    let auth = Auth::Bearer(&admin);
    let data = noise(3 * 1024 * 1024, 17);
    let began = post(
        &server,
        "/v1/ns/photos/uploads",
        auth,
        json!({"parent": ROOT, "name": b64("up.bin"), "size": data.len()}),
    );
    assert_eq!(began.code, 201, "{}", began.text());
    let id = began.json()["id"].as_str().unwrap().to_string();
    request(
        &server,
        "PUT",
        &format!("/v1/ns/photos/uploads/{id}?offset=0"),
        auth,
        &[],
        &data,
    )
    .ok();
    let committed = request(
        &server,
        "POST",
        &format!("/v1/ns/photos/uploads/{id}/commit"),
        auth,
        &[],
        b"",
    );
    assert_eq!(committed.code, 202, "{}", committed.text());
    wait_reached(&gate, "the upload");
    rename(&server, &admin, "photos", "pics").ok();
    let storing = get(&server, &format!("/v1/ns/pics/uploads/{id}"), auth)
        .ok()
        .json();
    assert_eq!(storing["state"], "storing", "{storing}");
    std::fs::write(gate.join("go"), b"").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let done = loop {
        let status = get(&server, &format!("/v1/ns/photos/uploads/{id}"), auth)
            .ok()
            .json();
        if status["state"] != "storing" {
            break status;
        }
        assert!(std::time::Instant::now() < deadline, "still storing");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(done["state"], "done", "{done}");
    server.set_env(Vec::new());

    // Back at its first name, its own former name its to take again; the names are no one else's.
    assert_eq!(
        rename(&server, &admin, "pics", "cloud").ok().json(),
        json!({"folder": "cloud"})
    );
    for was in ["photos", "pics"] {
        assert_eq!(
            get(&server, &format!("/v1/ns/{was}/info"), Auth::Bearer(&admin))
                .ok()
                .json()["folder"],
            "cloud",
            "{was}"
        );
        assert_eq!(
            add(&server, &bob, json!({ "folder": was })).code,
            409,
            "{was}"
        );
    }
    let in_memory = folders(&server);
    server.restart();
    assert_eq!(folders(&server), in_memory, "as the database has them");
    assert!(
        in_memory.contains("cloud  at ") && in_memory.contains("formerly photos, pics"),
        "{in_memory}"
    );
    assert_eq!(
        get(&server, "/v1/ns/pics/info", Auth::Bearer(&admin))
            .ok()
            .json()["folder"],
        "cloud"
    );
    let ns_dir = |name: &str| server.data.join("ns").join(hex::encode(name));
    assert!(ns_dir("cloud").exists());
    for never in ["photos", "pics"] {
        assert!(!ns_dir(never).exists(), "{never}: a directory of its own");
    }
    let report = server.fsck();
    assert!(report.contains("clean"), "{report}");

    // Every device follows it and has all of it; a new one joins it by its name now.
    let p = converge(&[&a, &b]);
    for d in [&a, &b] {
        assert_eq!(d.read("up.bin"), data);
        assert_eq!(d.read("during/commit.txt"), b"committed across the rename");
        for i in 1..20 {
            assert_eq!(
                d.read(&format!("d{}/f{i}", i % 3)),
                format!("file {i}").as_bytes()
            );
        }
    }
    let c = Device::new(&sb, &server, "c");
    c.synced();
    let diff = Picture::of(&c.root).diff(
        &p,
        Ignore {
            owner: true,
            ..Ignore::default()
        },
    );
    assert!(diff.is_empty(), "{diff:?}");
}
