//! The phone apps: a file sent in one request (`PUT /file`) arrives on every device byte for byte, once
//! however often it is sent and wherever the server or the phone was killed, and nothing is committed of
//! one that did not arrive whole; a name taken by other bytes is numbered or refused, never replaced; a
//! path through a symbolic link is refused; a download killed midway leaves nothing at its destination;
//! and the change feed tells the Files app what changed.

mod common;
use base64::Engine;
use cairn_engine::client::{Client, RawAnswer, Retries};
use cairn_engine::root::Config;
use cairn_mobile::{new_device_id, Session};
use cairn_protocol::file::{self as f, FileQuery, Taken};
use cairn_protocol::{DeviceId, ObjectId, RevisionId};
use common::*;
use std::io::{Read, Write};
use std::path::Path;

const MOBILE: &str = env!("CARGO_BIN_EXE_h-mobile");

fn token(server: &Server) -> String {
    std::fs::read_to_string(&server.token_file)
        .unwrap()
        .trim()
        .to_string()
}

fn client(server: &Server) -> Client {
    Client::new(
        &Config {
            server: server.url.clone(),
            namespace: "cloud".into(),
            token: token(server),
            device: DeviceId::random(),
            device_name: "test".into(),
            policy: Default::default(),
            filenames: Default::default(),
            allow_http: false,
        },
        Retries::Short,
    )
    .unwrap()
}

fn query(path: &str, taken: Taken) -> FileQuery {
    FileQuery {
        parent: None,
        path: path.into(),
        object: ObjectId::random(),
        revision: RevisionId::random(),
        device: DeviceId([7; 16]),
        device_name: "Phone of the test".into(),
        mtime: "1691920214.250".into(),
        btime: Some("1691920214.250".into()),
        taken,
        base: None,
        stale: Default::default(),
    }
}

fn put(c: &Client, q: &FileQuery, bytes: &[u8]) -> RawAnswer {
    c.put_file(&f::query_string(q), &mut &bytes[..], bytes.len() as u64)
        .unwrap()
}

fn header<'a>(a: &'a RawAnswer, name: &str) -> Option<&'a str> {
    a.headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[track_caller]
fn created(a: &RawAnswer) -> (String, u64) {
    assert!(
        (200..300).contains(&a.status),
        "{} {:?} {}",
        a.status,
        a.headers,
        String::from_utf8_lossy(&a.body)
    );
    let name = base64::engine::general_purpose::STANDARD
        .decode(header(a, f::HEADER_NAME).unwrap())
        .unwrap();
    (
        String::from_utf8(name).unwrap(),
        header(a, f::HEADER_SEQ).unwrap().parse().unwrap(),
    )
}

fn bytes(n: usize, seed: u8) -> Vec<u8> {
    let mut x = u64::from(seed) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// The names in a device's folder, sorted.
fn names(d: &Device, dir: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d.path(dir))
        .map(|r| {
            r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn a_file_sent_in_one_request_arrives_whole_with_its_times_and_the_phone_named() {
    let sb = Sandbox::on_disk("mobile-put");
    let server = Server::start(sb.path("data"));
    let c = client(&server);
    // Larger than a chunk batch, so it is stored in batches as it arrives.
    let data = bytes(40 * 1024 * 1024 + 12345, 1);
    let q = query("phone/iPhone/23-08-13 16-50-14 0003.heic", Taken::Rename);
    let a = put(&c, &q, &data);
    assert_eq!(a.status, 201, "{}", String::from_utf8_lossy(&a.body));
    let (name, seq) = created(&a);
    assert_eq!(name, "23-08-13 16-50-14 0003.heic");
    assert_eq!(
        header(&a, f::HEADER_OBJECT),
        Some(q.object.to_string().as_str())
    );
    assert_eq!(header(&a, f::HEADER_EXISTED), None);

    let d = Device::new(&sb, &server, "laptop");
    d.synced();
    let rel = "phone/iPhone/23-08-13 16-50-14 0003.heic";
    assert!(d.read(rel) == data, "the bytes differ");
    assert_eq!(mtime_of(&d.path(rel)).0, 1_691_920_214);
    assert_eq!(mode_of(&d.path(rel)) & 0o777, other_mode(0o644) & 0o777);
    // One commit, with the folders, as the phone.
    let change = c.change_with_ops(seq).unwrap().unwrap();
    assert_eq!(change.device_name, "Phone of the test");
    assert_eq!(change.device, DeviceId([7; 16]));
    assert_eq!(change.ops.len(), 3, "phone, iPhone and the file");
}

#[test]
fn the_same_request_twice_is_one_file_and_the_same_answer() {
    let sb = Sandbox::on_disk("mobile-twice");
    let server = Server::start(sb.path("data"));
    let c = client(&server);
    let data = bytes(3 * 1024 * 1024, 2);
    let q = query("Photos/a.heic", Taken::Rename);
    let first = created(&put(&c, &q, &data));
    let head = c.info().unwrap().head;
    let again = put(&c, &q, &data);
    assert_eq!(again.status, 201);
    assert_eq!(created(&again), first);
    assert_eq!(c.info().unwrap().head, head, "a retry commits nothing");
    let d = Device::new(&sb, &server, "laptop");
    d.synced();
    assert_eq!(names(&d, "Photos"), ["a.heic"]);
}

#[test]
fn a_name_taken_by_the_same_bytes_is_that_file_and_by_other_bytes_numbered_or_refused() {
    let sb = Sandbox::on_disk("mobile-taken");
    let server = Server::start(sb.path("data"));
    let c = client(&server);
    let one = bytes(100_000, 3);
    let other = bytes(100_000, 4);
    let (_, first_seq) = created(&put(&c, &query("P/x.jpg", Taken::Fail), &one));

    // A phone that sends it again as a new request, as after a reinstall: that file, nothing committed.
    let head = c.info().unwrap().head;
    let same = put(&c, &query("P/x.jpg", Taken::Fail), &one);
    assert_eq!(same.status, 200);
    assert_eq!(header(&same, f::HEADER_EXISTED), Some("1"));
    assert_eq!(created(&same).1, first_seq);
    assert_eq!(c.info().unwrap().head, head);

    let refused = put(&c, &query("P/x.jpg", Taken::Fail), &other);
    assert_eq!(refused.status, 409);
    assert!(
        header(&refused, f::HEADER_ERROR).is_some_and(|e| f::from_header_text(e).contains("taken"))
    );
    assert_eq!(c.info().unwrap().head, head);

    let numbered = put(&c, &query("P/x.jpg", Taken::Rename), &other);
    assert_eq!(created(&numbered).0, "x (2).jpg");
    let d = Device::new(&sb, &server, "laptop");
    d.synced();
    assert_eq!(names(&d, "P"), ["x (2).jpg", "x.jpg"]);
    assert!(d.read("P/x.jpg") == one && d.read("P/x (2).jpg") == other);
}

#[cfg(unix)]
#[test]
fn a_path_through_a_symlink_or_a_file_is_refused() {
    let sb = Sandbox::on_disk("mobile-path");
    let server = Server::start(sb.path("data"));
    let d = Device::new(&sb, &server, "laptop");
    std::fs::create_dir_all(d.path("real")).unwrap();
    std::os::unix::fs::symlink("real", d.path("link")).unwrap();
    d.write("file", b"x");
    d.synced();
    let c = client(&server);
    let head = c.info().unwrap().head;
    for path in ["link/a.jpg", "link/deeper/a.jpg", "file/a.jpg"] {
        let a = put(&c, &query(path, Taken::Rename), b"photo");
        assert_eq!(a.status, 409, "{path}");
        assert!(header(&a, f::HEADER_ERROR).is_some(), "{path}");
    }
    assert_eq!(c.info().unwrap().head, head, "nothing was committed");
    for bad in ["", "a//b", "../a", "a/./b"] {
        assert_eq!(
            put(&c, &query(bad, Taken::Rename), b"x").status,
            400,
            "{bad:?}"
        );
    }
    d.synced();
    assert_eq!(names(&d, "real"), Vec::<String>::new());
}

#[test]
fn a_read_only_folder_refuses_and_says_why() {
    let sb = Sandbox::on_disk("mobile-read-only");
    let server = Server::start(sb.path("data"));
    let bob = server.user("bob");
    server.admin(&["grant", "cloud", "bob", "read"]).expect_ok();
    let c = Client::new(
        &Config {
            server: server.url.clone(),
            namespace: "cloud".into(),
            token: std::fs::read_to_string(bob).unwrap().trim().into(),
            device: DeviceId::random(),
            device_name: "bob's phone".into(),
            policy: Default::default(),
            filenames: Default::default(),
            allow_http: false,
        },
        Retries::Short,
    )
    .unwrap();
    let a = put(&c, &query("a.jpg", Taken::Rename), b"x");
    assert_eq!(a.status, 403);
    assert!(header(&a, f::HEADER_ERROR).is_some_and(|e| e.contains("read-only")));
}

/// A request whose body stops short of its length, and then its connection closed.
fn cut_short(server: &Server, q: &FileQuery, data: &[u8], sent: usize) {
    let addr = server.url.strip_prefix("http://").unwrap();
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let head = format!(
        "PUT /v1/ns/cloud/file?{} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
        f::query_string(q),
        token(server),
        data.len()
    );
    conn.write_all(head.as_bytes()).unwrap();
    conn.write_all(&data[..sent]).unwrap();
    conn.shutdown(std::net::Shutdown::Write).unwrap();
    let mut sink = Vec::new();
    let _ = conn.read_to_end(&mut sink);
}

#[test]
fn a_body_cut_short_commits_nothing_and_the_retry_arrives() {
    let sb = Sandbox::on_disk("mobile-cut");
    let server = Server::start(sb.path("data"));
    let c = client(&server);
    let data = bytes(20 * 1024 * 1024, 5);
    let q = query("V/clip.mov", Taken::Rename);
    let head = c.info().unwrap().head;
    cut_short(&server, &q, &data, 9 * 1024 * 1024);
    assert_eq!(c.info().unwrap().head, head, "half a file was committed");
    assert_eq!(created(&put(&c, &q, &data)).0, "clip.mov");
    let d = Device::new(&sb, &server, "laptop");
    d.synced();
    assert!(d.read("V/clip.mov") == data);
}

#[test]
fn a_full_disk_while_storing_commits_nothing_and_says_so() {
    let sb = Sandbox::on_disk("mobile-full");
    let mut server = Server::start(sb.path("data"));
    let c = client(&server);
    let head = c.info().unwrap().head;
    server.set_env(vec![(
        "CAIRN_FP_ENOSPC".into(),
        "server.chunk.before_write".into(),
    )]);
    server.restart();
    let q = query("Photos/b.heic", Taken::Rename);
    let data = bytes(5 * 1024 * 1024, 6);
    let a = put(&c, &q, &data);
    assert_eq!(a.status, 507, "{}", String::from_utf8_lossy(&a.body));
    assert!(header(&a, f::HEADER_ERROR).is_some_and(|e| e.contains("disk is full")));
    server.set_env(vec![]);
    server.restart();
    assert_eq!(c.info().unwrap().head, head);
    assert_eq!(created(&put(&c, &q, &data)).0, "b.heic");
}

#[test]
fn a_server_killed_at_each_step_leaves_one_whole_file_after_the_retry() {
    for point in [
        "server.chunk.before_write",
        "server.file.stored",
        "server.file.before_ack",
    ] {
        let sb = Sandbox::on_disk("mobile-kill");
        let mut server = Server::start(sb.path("data"));
        let c = client(&server);
        server.set_env(vec![("CAIRN_FP_CRASH".into(), point.into())]);
        server.restart();
        let data = bytes(6 * 1024 * 1024, 7);
        let q = query("Photos/c.heic", Taken::Rename);
        let lost = c.put_file(&f::query_string(&q), &mut &data[..], data.len() as u64);
        assert!(
            lost.as_ref().is_err() || lost.as_ref().is_ok_and(|a| a.status >= 500),
            "{point}: answered {:?}",
            lost.map(|a| a.status)
        );
        assert!(!server.alive_after_a_kill(), "{point} was not reached");
        server.set_env(vec![]);
        server.restart();
        let (name, _) = created(&put(&c, &q, &data));
        assert_eq!(name, "c.heic", "{point}");
        let d = Device::new(&sb, &server, "laptop");
        d.synced();
        assert_eq!(names(&d, "Photos"), ["c.heic"], "{point}");
        assert!(d.read("Photos/c.heic") == data, "{point}: the bytes differ");
        server.fsck();
    }
}

fn h_mobile(args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = std::process::Command::new(MOBILE);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn a_phone_killed_at_each_step_of_an_upload_sends_it_once() {
    for point in [
        "mobile.upload.planned",
        "mobile.upload.sending",
        "mobile.upload.answered",
    ] {
        let sb = Sandbox::on_disk("mobile-phone-kill");
        let server = Server::start(sb.path("data"));
        let db = sb.path("cairn.db");
        let photo = sb.path("IMG_0003.HEIC");
        let data = bytes(2 * 1024 * 1024 + 7, 8);
        std::fs::write(&photo, &data).unwrap();
        let args = [
            "upload",
            db.to_str().unwrap(),
            &server.url,
            server.token_file.to_str().unwrap(),
            "cloud",
            "phone/iPhone",
            photo.to_str().unwrap(),
            "asset-1",
            "IMG_0003.HEIC",
            "1691920214",
        ];
        let killed = h_mobile(&args, &[("CAIRN_FP_CRASH", point)]);
        assert!(killed.was_killed(), "{point}: {}", killed.text());
        let done = h_mobile(&args, &[]).expect_ok();
        assert!(
            done.text().contains("\"state\":\"Done\""),
            "{point}: {}",
            done.text()
        );
        // And once more: done stays done, and sends nothing.
        h_mobile(&args, &[]).expect_ok();
        let d = Device::new(&sb, &server, "laptop");
        d.synced();
        assert_eq!(
            names(&d, "phone/iPhone"),
            ["23-08-13 09-50-14 0003.heic"],
            "{point}"
        );
        assert!(d.read("phone/iPhone/23-08-13 09-50-14 0003.heic") == data);
    }
}

#[test]
fn a_download_killed_midway_leaves_nothing_at_its_destination() {
    let sb = Sandbox::on_disk("mobile-download");
    let server = Server::start(sb.path("data"));
    let d = Device::new(&sb, &server, "laptop");
    let data = bytes(9 * 1024 * 1024, 9);
    d.write("big.mov", &data);
    d.synced();
    let s = Session::new(
        server.url.clone(),
        token(&server),
        new_device_id(),
        "iPhone".into(),
        false,
    )
    .unwrap();
    let folder = s.folder("cloud".into()).unwrap();
    let entry = folder
        .list("root".into())
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.display == "big.mov")
        .unwrap();
    let dest = sb.path("big.mov");
    let args = [
        "download",
        &server.url,
        server.token_file.to_str().unwrap(),
        "cloud",
        &entry.object,
        &entry.revision,
        dest.to_str().unwrap(),
        &data.len().to_string(),
    ];
    let killed = h_mobile(&args, &[("CAIRN_FP_CRASH", "mobile.download.written")]);
    assert!(killed.was_killed(), "{}", killed.text());
    assert!(!dest.exists(), "a half-made file is at the destination");
    h_mobile(&args, &[]).expect_ok();
    assert!(std::fs::read(&dest).unwrap() == data);
    // A size that is not the file's is refused, and leaves nothing.
    let wrong = sb.path("wrong.mov");
    let err = folder
        .download(
            entry.object.clone(),
            entry.revision.clone(),
            wrong.to_string_lossy().into_owned(),
            123,
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("bytes"), "{err}");
    assert!(!wrong.exists());
}

#[cfg(unix)]
#[test]
fn the_files_app_is_told_what_changed_and_what_it_cannot_show() {
    let sb = Sandbox::on_disk("mobile-fp");
    let server = Server::start(sb.path("data"));
    let d = Device::new(&sb, &server, "laptop");
    d.write("keep.txt", b"one");
    d.write("dir/edit.txt", b"two");
    d.write("dir/gone.txt", b"three");
    d.synced();
    let s = Session::new(
        server.url.clone(),
        token(&server),
        new_device_id(),
        "iPhone".into(),
        false,
    )
    .unwrap();
    let folder = s.folder("cloud".into()).unwrap();
    let start = folder.head().unwrap();
    let root = folder.fp_list("root".into()).unwrap();
    let names_of = |items: &[cairn_mobile::fp::FpItem]| {
        let mut v: Vec<String> = items.iter().map(|i| i.filename.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(names_of(&root.items), ["dir", "keep.txt"]);
    let dir = root
        .items
        .iter()
        .find(|i| i.filename == "dir")
        .unwrap()
        .clone();
    assert!(dir.is_dir && dir.writable);
    let inner = folder.fp_list(dir.identifier.clone()).unwrap();
    let edit = inner
        .items
        .iter()
        .find(|i| i.filename == "edit.txt")
        .unwrap()
        .clone();
    let gone = inner
        .items
        .iter()
        .find(|i| i.filename == "gone.txt")
        .unwrap()
        .clone();
    assert_eq!(edit.parent, dir.identifier);

    d.write("dir/edit.txt", b"two, edited");
    std::fs::rename(d.path("keep.txt"), d.path("dir/moved.txt")).unwrap();
    std::fs::remove_file(d.path("dir/gone.txt")).unwrap();
    std::os::unix::fs::symlink("keep.txt", d.path("link")).unwrap();
    d.write("new.txt", b"four");
    d.synced();

    let mut updated = Vec::new();
    let mut deleted = Vec::new();
    let mut skipped = Vec::new();
    let mut at = start;
    loop {
        // One look-up at a time, so that the batches are cut.
        let c = folder.fp_changes(at, 1).unwrap();
        updated.extend(c.updated);
        deleted.extend(c.deleted);
        skipped.extend(c.skipped);
        assert!(c.next >= at);
        at = c.next;
        if !c.more {
            break;
        }
    }
    let last = |name: &str| {
        updated
            .iter()
            .rev()
            .find(|i| i.filename == name)
            .unwrap_or_else(|| panic!("{name} not reported: {updated:?}"))
            .clone()
    };
    let edited = last("edit.txt");
    assert_eq!(edited.identifier, edit.identifier);
    assert_ne!(edited.content_version, edit.content_version);
    let moved = last("moved.txt");
    assert_eq!(moved.parent, dir.identifier);
    assert_eq!(last("new.txt").parent, "root");
    assert!(deleted.contains(&gone.identifier), "{deleted:?}");
    assert!(
        skipped
            .iter()
            .any(|s| s.display == "link" && s.reason.contains("symbolic link")),
        "{skipped:?}"
    );
    assert_eq!(at, folder.head().unwrap());

    // Listed again, the same: the link skipped with its reason.
    let root = folder.fp_list("root".into()).unwrap();
    assert_eq!(names_of(&root.items), ["dir", "new.txt"]);
    assert_eq!(root.skipped.len(), 1);
    assert!(folder.fp_item(gone.identifier.clone()).unwrap().is_none());
    assert_eq!(
        folder
            .fp_item(moved.identifier.clone())
            .unwrap()
            .unwrap()
            .filename,
        "moved.txt"
    );
}

#[test]
fn a_folder_made_and_a_file_added_from_the_files_app_arrive() {
    let sb = Sandbox::on_disk("mobile-add");
    let server = Server::start(sb.path("data"));
    let s = Session::new(
        server.url.clone(),
        token(&server),
        new_device_id(),
        "iPhone".into(),
        false,
    )
    .unwrap();
    let folder = s.folder("cloud".into()).unwrap();
    let made = folder.mkdir("root".into(), "Scans".into()).unwrap();
    let local = sb.path("doc.pdf");
    std::fs::write(&local, b"%PDF-1.7 a scan").unwrap();
    let sent = folder
        .put_file(
            made.object.clone(),
            "doc.pdf".into(),
            local.to_string_lossy().into_owned(),
            1_700_000_000,
            0,
            false,
            None,
        )
        .unwrap();
    assert_eq!(sent.name, "doc.pdf");
    // The same name again, other bytes: refused, for the Files app to choose another.
    std::fs::write(&local, b"%PDF-1.7 another").unwrap();
    let err = folder
        .put_file(
            made.object.clone(),
            "doc.pdf".into(),
            local.to_string_lossy().into_owned(),
            1_700_000_000,
            0,
            false,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err, cairn_mobile::CairnError::Refused { status: 409, .. }),
        "{err:?}"
    );
    assert!(matches!(
        folder.mkdir("root".into(), "Scans".into()),
        Err(cairn_mobile::CairnError::Refused { .. })
    ));
    let d = Device::new(&sb, &server, "laptop");
    d.synced();
    assert_eq!(d.read("Scans/doc.pdf"), b"%PDF-1.7 a scan");
    assert_eq!(mtime_of(&d.path("Scans/doc.pdf")).0, 1_700_000_000);
    assert!(Path::new(&d.path("Scans")).is_dir());
}

const ROOT: &str = "00000000000000000000000000000000";

/// A file's object and head revision, as the web's listing of its folder has them.
fn listed(server: &Server, dir: &str, name: &str) -> (ObjectId, RevisionId) {
    let t = token(server);
    let listing = get(server, &format!("/v1/ns/cloud/dir/{dir}"), Auth::Bearer(&t))
        .ok()
        .json();
    let e = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["display"] == name)
        .unwrap_or_else(|| panic!("no {name} in {listing}"))
        .clone();
    (
        e["object"].as_str().unwrap().parse().unwrap(),
        e["revision"].as_str().unwrap().parse().unwrap(),
    )
}

/// A change of the file `object` was at `base` when the phone saw it.
fn change(object: ObjectId, base: RevisionId, stale: f::Stale) -> FileQuery {
    FileQuery {
        path: String::new(),
        object,
        base: Some(base),
        stale,
        ..query("unused", Taken::Fail)
    }
}

/// A file changed in one request, against the revision the phone saw, takes the new bytes and keeps
/// everything of it the phone never saw but its mtime; changed since by another, the bytes go beside it as
/// a conflict copy, or with `stale=fail` nowhere; the same request sent again is the same answer; and a file
/// deleted since comes back where it was, as itself or, changed before its delete, as a copy.
#[test]
fn a_change_in_one_request_lands_on_what_the_phone_saw_and_beside_anything_else() {
    let sb = Sandbox::on_disk("mobile-change");
    let mut server = Server::start(sb.path("data"));
    let c = client(&server);
    let a = Device::new(&sb, &server, "laptop");
    a.write("notes/f.txt", "as the laptop wrote it");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            a.path("notes/f.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    a.synced();
    let (notes, _) = listed(&server, ROOT, "notes");
    let (f_id, seen) = listed(&server, &notes.to_string(), "f.txt");

    let q = change(f_id, seen, f::Stale::Copy);
    let answer = put(&c, &q, b"as the phone changed it");
    assert_eq!(
        answer.status,
        201,
        "{}",
        String::from_utf8_lossy(&answer.body)
    );
    assert!(header(&answer, f::HEADER_CONFLICT).is_none());
    assert_eq!(created(&answer).0, "f.txt");
    assert_eq!(
        header(&answer, f::HEADER_OBJECT),
        Some(f_id.to_string().as_str())
    );
    let head = c.info().unwrap().head;
    // Again, as a phone whose answer was lost: the same answer, and nothing more committed.
    let again = put(&c, &q, b"as the phone changed it");
    assert_eq!(created(&again), created(&answer));
    assert_eq!(c.info().unwrap().head, head);
    a.synced();
    assert_eq!(a.read("notes/f.txt"), b"as the phone changed it");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(a.path("notes/f.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the mode the phone never saw is kept");
    }

    // The same bytes again, against what is there now: that file, nothing committed.
    let (_, now) = listed(&server, &notes.to_string(), "f.txt");
    let same = put(
        &c,
        &change(f_id, now, f::Stale::Copy),
        b"as the phone changed it",
    );
    assert_eq!(same.status, 200);
    assert_eq!(header(&same, f::HEADER_EXISTED), Some("1"));
    assert_eq!(c.info().unwrap().head, head);

    // Against what the phone saw before the laptop changed it: beside it, never in its place.
    a.write("notes/f.txt", "the laptop changed it again");
    a.synced();
    let refused = put(&c, &change(f_id, now, f::Stale::Fail), b"a stale change");
    assert_eq!(refused.status, 409);
    let stale = change(f_id, now, f::Stale::Copy);
    let copied = put(&c, &stale, b"a stale change");
    assert_eq!(
        copied.status,
        201,
        "{}",
        String::from_utf8_lossy(&copied.body)
    );
    assert_eq!(header(&copied, f::HEADER_CONFLICT), Some("1"));
    let (copy_name, _) = created(&copied);
    assert!(
        copy_name.starts_with("f (conflict Phone of the test ") && copy_name.ends_with(").txt"),
        "{copy_name}"
    );
    let again = put(&c, &stale, b"a stale change");
    assert_eq!(created(&again), created(&copied), "the copy's retry");
    assert_eq!(header(&again, f::HEADER_CONFLICT), Some("1"));
    a.synced();
    assert_eq!(a.read("notes/f.txt"), b"the laptop changed it again");
    assert_eq!(a.read(&format!("notes/{copy_name}")), b"a stale change");

    // Deleted since, unchanged before: back at its name, with the new bytes, as itself.
    let (_, last) = listed(&server, &notes.to_string(), "f.txt");
    std::fs::remove_file(a.path("notes/f.txt")).unwrap();
    a.synced();
    let back = put(
        &c,
        &change(f_id, last, f::Stale::Copy),
        b"brought back by the phone",
    );
    assert_eq!(created(&back).0, "f.txt");
    assert!(header(&back, f::HEADER_CONFLICT).is_none());
    assert_eq!(
        header(&back, f::HEADER_OBJECT),
        Some(f_id.to_string().as_str())
    );
    a.synced();
    assert_eq!(a.read("notes/f.txt"), b"brought back by the phone");

    // Deleted after a change the phone did not see: a copy where it was.
    let (_, seen) = listed(&server, &notes.to_string(), "f.txt");
    a.write("notes/f.txt", "changed, then deleted");
    a.synced();
    std::fs::remove_file(a.path("notes/f.txt")).unwrap();
    a.synced();
    let copied = put(
        &c,
        &change(f_id, seen, f::Stale::Copy),
        b"the phone's, after both",
    );
    assert_eq!(header(&copied, f::HEADER_CONFLICT), Some("1"));
    let (name, _) = created(&copied);
    a.synced();
    assert!(!a.path("notes/f.txt").exists());
    assert_eq!(a.read(&format!("notes/{name}")), b"the phone's, after both");
    server.fsck();
}

/// A change killed at each step on the server lands once, after the phone sends it again.
#[test]
fn a_change_killed_at_each_step_lands_once_after_the_retry() {
    for point in [
        "server.chunk.before_write",
        "server.file.stored",
        "server.file.before_ack",
    ] {
        let sb = Sandbox::on_disk("mobile-change-kill");
        let mut server = Server::start(sb.path("data"));
        let c = client(&server);
        let a = Device::new(&sb, &server, "laptop");
        a.write("f.bin", bytes(1024, 1));
        a.synced();
        let (f_id, seen) = listed(&server, ROOT, "f.bin");
        server.set_env(vec![("CAIRN_FP_CRASH".into(), point.into())]);
        server.restart();
        let data = bytes(3 * 1024 * 1024, 9);
        let q = change(f_id, seen, f::Stale::Copy);
        let lost = c.put_file(&f::query_string(&q), &mut &data[..], data.len() as u64);
        assert!(
            lost.as_ref().is_err() || lost.as_ref().is_ok_and(|a| a.status >= 500),
            "{point}: answered {:?}",
            lost.map(|a| a.status)
        );
        assert!(!server.alive_after_a_kill(), "{point} was not reached");
        server.set_env(vec![]);
        server.restart();
        let answer = put(&c, &q, &data);
        assert_eq!(created(&answer).0, "f.bin", "{point}");
        assert!(header(&answer, f::HEADER_CONFLICT).is_none(), "{point}");
        a.synced();
        assert_eq!(names(&a, "."), [".cairn", "f.bin"], "{point}");
        assert!(a.read("f.bin") == data, "{point}: the bytes differ");
        server.fsck();
    }
}

/// Begins a resumable upload on a socket of its own and sends `sent` bytes of `data`; returns the
/// upload's address from the 104, once it has come, with the socket still open.
fn begin_resumable(
    server: &Server,
    q: &FileQuery,
    data: &[u8],
    sent: usize,
    headers: &[(&str, &str)],
) -> (String, std::net::TcpStream) {
    let addr = server.url.strip_prefix("http://").unwrap();
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let mut head = format!(
        "PUT /v1/ns/cloud/file?{} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n",
        f::query_string(q),
        token(server),
        data.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes()).unwrap();
    conn.write_all(&data[..sent]).unwrap();
    conn.set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = conn.read(&mut buf).expect("a 104 within 20 s");
        assert!(
            n > 0,
            "closed before a 104: {}",
            String::from_utf8_lossy(&got)
        );
        got.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&got).to_lowercase();
        if let Some(end) = text.find("\r\n\r\n") {
            let interim = &text[..end];
            assert!(interim.starts_with("http/1.1 104 "), "{text}");
            let location = interim
                .lines()
                .find_map(|l| l.strip_prefix("location: "))
                .expect("the 104 says where")
                .to_string();
            return (location, conn);
        }
    }
}

fn upload_request(
    server: &Server,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Answer {
    let t = token(server);
    request(server, method, path, Auth::Bearer(&t), headers, body)
}

/// A resumable upload cut off midway goes on from where the server stands, in the draft's interop
/// versions the phones speak, and is one file and one commit; one given up leaves nothing; another
/// user's is not there.
#[test]
fn a_resumable_upload_cut_off_goes_on_from_where_it_stood_and_is_one_file() {
    let sb = Sandbox::on_disk("mobile-resume");
    let server = Server::start(sb.path("data"));
    let c = client(&server);
    let options = |version: Option<&str>| {
        let mut h = vec![];
        if let Some(v) = version {
            h.push(("Upload-Draft-Interop-Version", v));
        }
        upload_request(&server, "OPTIONS", "/v1/ns/cloud/file", &h, b"")
    };
    let asked = options(Some("6"));
    assert_eq!(asked.code, 200, "{}", asked.text());
    assert!(asked.header("upload-limit").is_some());
    assert_eq!(options(None).code, 501);
    assert_eq!(options(Some("99")).code, 501);

    // Version 6: cut off after a third, then the rest from where the server stands.
    let data = bytes(5 * 1024 * 1024 + 99, 4);
    let q = query("Photos/long.mov", Taken::Rename);
    let v6 = [
        ("Upload-Draft-Interop-Version", "6"),
        ("Upload-Complete", "?1"),
    ];
    let (at, conn) = begin_resumable(&server, &q, &data, data.len() / 3, &v6);
    assert_eq!(at, format!("/v1/ns/cloud/file/uploads/{}", q.revision));
    let head = c.info().unwrap().head;
    drop(conn);
    let mut stands = 0;
    for _ in 0..200 {
        let h = upload_request(
            &server,
            "HEAD",
            &at,
            &[("Upload-Draft-Interop-Version", "6")],
            b"",
        );
        assert_eq!(h.code, 204, "{}", h.text());
        assert_eq!(h.header("upload-complete").as_deref(), Some("?0"));
        stands = h.header("upload-offset").unwrap().parse().unwrap();
        if stands == data.len() / 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(stands > 0 && stands <= data.len() / 3, "{stands}");
    assert_eq!(
        c.info().unwrap().head,
        head,
        "nothing committed of half a file"
    );
    // Another user's upload is not there for them.
    let bob = std::fs::read_to_string(server.user("bob")).unwrap();
    let theirs = request(
        &server,
        "HEAD",
        &at,
        Auth::Bearer(bob.trim()),
        &[("Upload-Draft-Interop-Version", "6")],
        b"",
    );
    assert_eq!(theirs.code, 404);
    // From the wrong offset, refused, saying where it stands.
    let offset = (stands + 1).to_string();
    let wrong = upload_request(
        &server,
        "PATCH",
        &at,
        &[
            ("Upload-Draft-Interop-Version", "6"),
            ("Upload-Complete", "?1"),
            ("Upload-Offset", &offset),
            ("Content-Type", "application/partial-upload"),
        ],
        &data[stands + 1..],
    );
    assert_eq!(wrong.code, 409, "{}", wrong.text());
    assert_eq!(wrong.header("upload-offset"), Some(stands.to_string()));
    let offset = stands.to_string();
    let rest = upload_request(
        &server,
        "PATCH",
        &at,
        &[
            ("Upload-Draft-Interop-Version", "6"),
            ("Upload-Complete", "?1"),
            ("Upload-Offset", &offset),
            ("Content-Type", "application/partial-upload"),
        ],
        &data[stands..],
    );
    assert_eq!(rest.code, 201, "{}", rest.text());
    assert_eq!(rest.header("upload-offset"), Some(data.len().to_string()));
    assert_eq!(
        rest.header(f::HEADER_REVISION),
        Some(q.revision.to_string())
    );
    assert_eq!(c.info().unwrap().head, head + 1, "one commit");
    let h = upload_request(
        &server,
        "HEAD",
        &at,
        &[("Upload-Draft-Interop-Version", "6")],
        b"",
    );
    assert_eq!(h.header("upload-complete").as_deref(), Some("?1"));
    let again = upload_request(
        &server,
        "PATCH",
        &at,
        &[
            ("Upload-Draft-Interop-Version", "6"),
            ("Upload-Complete", "?1"),
            ("Upload-Offset", &data.len().to_string()),
        ],
        b"",
    );
    assert_eq!(again.code, 400, "{}", again.text());
    // The whole request sent again, as a client that lost every answer: the first answer.
    let (name, seq) = created(&put(&c, &q, &data));
    assert_eq!((name.as_str(), seq), ("long.mov", head + 1));

    // Version 3, in two parts by design: the first says more is to come.
    let small = bytes(300_000, 5);
    let q3 = query("Photos/two-parts.jpg", Taken::Rename);
    let addr = server.url.strip_prefix("http://").unwrap();
    let first = request(
        &server,
        "PUT",
        &format!("/v1/ns/cloud/file?{}", f::query_string(&q3)),
        Auth::Bearer(&token(&server)),
        &[
            ("Upload-Draft-Interop-Version", "3"),
            ("Upload-Incomplete", "?1"),
            ("Host", addr),
        ],
        &small[..100_000],
    );
    assert_eq!(first.code, 201, "{}", first.text());
    assert_eq!(first.header("upload-offset").as_deref(), Some("100000"));
    let at3 = first.header("location").unwrap();
    let second = upload_request(
        &server,
        "PATCH",
        &at3,
        &[
            ("Upload-Draft-Interop-Version", "3"),
            ("Upload-Incomplete", "?0"),
            ("Upload-Offset", "100000"),
        ],
        &small[100_000..],
    );
    assert_eq!(second.code, 201, "{}", second.text());

    // Given up: nothing of it stays.
    let q4 = query("Photos/given-up.mov", Taken::Rename);
    let (at4, conn) = begin_resumable(&server, &q4, &data, 1024 * 1024, &v6);
    drop(conn);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let gone = upload_request(&server, "DELETE", &at4, &[], b"");
    assert_eq!(gone.code, 204, "{}", gone.text());
    let h = upload_request(
        &server,
        "HEAD",
        &at4,
        &[("Upload-Draft-Interop-Version", "6")],
        b"",
    );
    assert_eq!(h.code, 404);
    assert!(
        std::fs::read_dir(server.data.join("web/resumable"))
            .unwrap()
            .next()
            .is_none(),
        "the given-up upload's bytes are removed, the committed ones' too"
    );

    let d = Device::new(&sb, &server, "laptop");
    d.synced();
    assert_eq!(names(&d, "Photos"), ["long.mov", "two-parts.jpg"]);
    assert!(d.read("Photos/long.mov") == data, "the bytes differ");
    assert!(d.read("Photos/two-parts.jpg") == small, "the bytes differ");
    let mut server = server;
    server.fsck();
}

/// A server killed with a resumable upload's bytes all in but not committed forgets the upload, which a
/// start empties; the phone's system sends it again whole, and it is one file.
#[test]
fn a_server_killed_before_a_resumable_upload_commits_leaves_one_file_after_it_is_sent_again() {
    let sb = Sandbox::on_disk("mobile-resume-kill");
    let mut server = Server::start(sb.path("data"));
    let c = client(&server);
    server.set_env(vec![(
        "CAIRN_FP_CRASH".into(),
        "server.resumable.stored".into(),
    )]);
    server.restart();
    let data = bytes(2 * 1024 * 1024, 6);
    let q = query("Photos/k.mov", Taken::Rename);
    // Sent whole, and its answer never comes: the server is gone after the 104.
    let (_, mut conn) = begin_resumable(
        &server,
        &q,
        &data,
        1,
        &[
            ("Upload-Draft-Interop-Version", "6"),
            ("Upload-Complete", "?1"),
        ],
    );
    let _ = conn.write_all(&data[1..]);
    let mut rest = Vec::new();
    let _ = conn.read_to_end(&mut rest);
    assert!(
        !String::from_utf8_lossy(&rest).contains("HTTP/1.1 2"),
        "answered: {}",
        String::from_utf8_lossy(&rest)
    );
    assert!(!server.alive_after_a_kill(), "never reached");
    server.set_env(vec![]);
    server.restart();
    let at = format!("/v1/ns/cloud/file/uploads/{}", q.revision);
    let h = upload_request(
        &server,
        "HEAD",
        &at,
        &[("Upload-Draft-Interop-Version", "6")],
        b"",
    );
    assert_eq!(h.code, 404, "a start forgets what waited");
    assert_eq!(created(&put(&c, &q, &data)).0, "k.mov");
    let d = Device::new(&sb, &server, "laptop");
    d.synced();
    assert_eq!(names(&d, "Photos"), ["k.mov"]);
    assert!(d.read("Photos/k.mov") == data);
    server.fsck();
}
