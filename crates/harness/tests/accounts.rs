//! Users, their tokens, and what each may do in each folder: a folder nobody gave a user is not there
//! for that user, and a folder a user may only read takes nothing from that user's devices, nor loses
//! anything on them.

mod common;
use common::*;

/// The ID `token list` shows for the one token of `user`.
fn token_of(server: &Server, user: &str) -> String {
    let list = server.admin(&["token", "list"]).expect_ok();
    let ids: Vec<&str> = list
        .stdout
        .lines()
        .filter(|l| l.split_whitespace().nth(1) == Some(user))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(ids.len(), 1, "{}", list.text());
    ids[0].to_string()
}

fn skipped_read_only(run: &Run) -> Vec<String> {
    run.stdout
        .lines()
        .filter(|l| l.starts_with("skipped: ") && l.contains("read-only for you"))
        .map(str::to_string)
        .collect()
}

#[test]
fn each_user_reaches_only_the_folders_given_it() {
    let sb = Sandbox::on_disk("accounts-apart");
    let mut server = Server::start(sb.path("data"));
    let alice = server.user("alice");
    let bob = server.user("bob");
    server
        .admin(&["folder", "add", "alice", "--manager", "alice"])
        .expect_ok();
    server
        .admin(&["folder", "add", "bob", "--manager", "bob"])
        .expect_ok();

    let a = Device::as_user(&sb, &server, "a", &alice, "alice");
    a.write("secret.txt", "alice's");
    a.synced();

    // Bob's token opens neither alice's folder nor the first user's, and is answered as for a folder
    // that does not exist, so that nobody learns what is there by asking.
    for folder in ["alice", "default", "nowhere"] {
        let run = Device::try_as_user(&sb, &server, &format!("b-{folder}"), &bob, folder)
            .err()
            .unwrap_or_else(|| panic!("bob reached {folder}"));
        assert!(
            run.text()
                .contains(&format!("no folder \"{folder}\" for user \"bob\"")),
            "{}",
            run.text()
        );
    }
    let b = Device::as_user(&sb, &server, "b", &bob, "bob");
    b.synced();
    assert!(!b.exists("secret.txt"));

    // Each token lists what it reaches, and only that.
    let listed = run(
        &[
            "folders",
            "--server",
            &server.url,
            "--token-file",
            bob.to_str().unwrap(),
        ],
        &[],
    )
    .expect_ok();
    assert!(listed.stdout.contains("user bob"), "{}", listed.text());
    assert!(listed
        .stdout
        .lines()
        .any(|l| l.starts_with("bob ") && l.ends_with("manage")));
    assert!(!listed.stdout.contains("alice"), "{}", listed.text());

    // The data directory's token from before accounts is the first user's, and still syncs default.
    let first = Device::new(&sb, &server, "first");
    first.write("x", "x");
    first.synced();
    let log = first.cairn(&["log"]).expect_ok();
    assert!(log.stdout.contains("first (admin)"), "{}", log.text());

    // A token taken back is refused from the next request on, and what its device holds stays.
    server
        .admin(&["token", "remove", &token_of(&server, "bob")])
        .expect_ok();
    b.write("later", "kept");
    let run = b.sync();
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.text().contains("refused the access token"),
        "{}",
        run.text()
    );
    assert_eq!(b.read("later"), b"kept");
    // Another token for him works where the first did, and the folder is as it was.
    let again = sb.path("bob-again.token");
    server
        .admin(&[
            "token",
            "add",
            "bob",
            "--label",
            "b",
            "--token-file",
            again.to_str().unwrap(),
        ])
        .expect_ok();
    let b2 = Device::as_user(&sb, &server, "b2", &again, "bob");
    b2.synced();
    server.fsck();
}

#[test]
fn a_reader_gets_everything_and_what_it_changes_stays_with_it() {
    let sb = Sandbox::on_disk("accounts-reader");
    let mut server = Server::start(sb.path("data"));
    let alice = server.user("alice");
    let bob = server.user("bob");
    server
        .admin(&["folder", "add", "team", "--manager", "alice"])
        .expect_ok();
    server.admin(&["grant", "team", "bob", "read"]).expect_ok();
    let a = Device::as_user(&sb, &server, "a", &alice, "team");
    let b = Device::as_user(&sb, &server, "b", &bob, "team");
    a.write("f", "one");
    a.write("d/g", "two");
    a.write("d/h", "three");
    a.write("moved", "four");
    a.synced();
    b.synced();
    assert_eq!(b.read("d/h"), b"three");

    // Bob edits, adds, deletes and renames, which his access does not let go up.
    b.write("f", "bob's edit");
    b.write("new/inside.txt", "bob's new");
    std::fs::remove_file(b.path("d/g")).unwrap();
    std::fs::rename(b.path("moved"), b.path("renamed")).unwrap();
    let run = b.synced();
    let held = skipped_read_only(&run);
    for (path, what) in [
        ("f", "changed here"),
        ("new", "added here"),
        ("renamed", "moved or renamed here"),
    ] {
        assert!(
            held.iter()
                .any(|l| l.starts_with(&format!("skipped: {path}: ")) && l.contains(what)),
            "{path} {what}:\n{}",
            run.text()
        );
    }
    assert_eq!(held.len(), 3, "only the topmost of what is new: {held:?}");
    assert!(
        run.stdout.contains("3 change(s) here stay here"),
        "{}",
        run.text()
    );
    // What was deleted here came back; what was changed here stayed as it was.
    assert!(
        run.stdout
            .contains("d/g: deleted here in a folder that is read-only for you"),
        "{}",
        run.text()
    );
    assert_eq!(b.read("d/g"), b"two");
    assert_eq!(b.read("f"), b"bob's edit");
    assert_eq!(b.read("new/inside.txt"), b"bob's new");
    assert_eq!(b.read("renamed"), b"four");
    assert!(!b.exists("moved"));

    // Nothing went up, and the journal holds alice's commits alone.
    a.synced();
    assert_eq!(a.read("f"), b"one");
    assert!(!a.exists("new") && !a.exists("renamed") && a.exists("moved"));
    let log = a.cairn(&["log"]).expect_ok();
    assert!(!log.stdout.contains("(bob)"), "{}", log.text());

    // Said again by every sync, which changes nothing.
    let picture = Picture::of(&b.root);
    let again = b.synced();
    assert_eq!(skipped_read_only(&again), held);
    assert!(Picture::of(&b.root)
        .diff(&picture, Ignore::default())
        .is_empty());

    // The server's new version of what he changed arrives, and his stays beside it.
    a.write("f", "alice's second");
    a.write("d/h", "alice's third");
    a.synced();
    let run = b.synced();
    assert_eq!(b.read("f"), b"alice's second");
    assert_eq!(b.read("d/h"), b"alice's third");
    let copies = b.conflict_copies("", "f");
    assert_eq!(copies.len(), 1, "{}", run.text());
    assert_eq!(b.read(&copies[0]), b"bob's edit");
    assert!(
        skipped_read_only(&run)
            .iter()
            .any(|l| l.contains(&copies[0]) && l.contains("added here")),
        "{}",
        run.text()
    );
    a.synced();
    assert!(a.conflict_copies("", "f").is_empty());

    // Given write access, what he held back goes up, nothing lost on the way.
    server.admin(&["grant", "team", "bob", "write"]).expect_ok();
    let run = b.synced();
    assert!(skipped_read_only(&run).is_empty(), "{}", run.text());
    a.synced();
    assert_eq!(a.read("new/inside.txt"), b"bob's new");
    assert_eq!(a.read(&copies[0]), b"bob's edit");
    assert_eq!(a.read("renamed"), b"four");
    let log = a.cairn(&["log"]).expect_ok();
    assert!(log.stdout.contains("b (bob)"), "{}", log.text());
    converge(&[&a, &b]);
    server.fsck();
}

#[test]
fn a_reader_joining_with_files_of_its_own_keeps_them_and_takes_the_servers() {
    let sb = Sandbox::on_disk("accounts-join");
    let mut server = Server::start(sb.path("data"));
    let alice = server.user("alice");
    let bob = server.user("bob");
    server
        .admin(&["folder", "add", "team", "--manager", "alice"])
        .expect_ok();
    server.admin(&["grant", "team", "bob", "read"]).expect_ok();
    let a = Device::as_user(&sb, &server, "a", &alice, "team");
    a.write("same", "same bytes");
    a.write("theirs", "alice's");
    a.write("both", "alice's both");
    a.synced();

    let root = sb.path("b");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("same"), "same bytes").unwrap();
    std::fs::write(root.join("mine"), "bob's own").unwrap();
    std::fs::write(root.join("both"), "bob's both").unwrap();
    let b = Device::as_user(&sb, &server, "b", &bob, "team");
    let run = b.synced();
    assert_eq!(b.read("theirs"), b"alice's");
    assert_eq!(b.read("same"), b"same bytes");
    assert_eq!(b.read("mine"), b"bob's own");
    assert_eq!(b.read("both"), b"alice's both");
    let copies = b.conflict_copies("", "both");
    assert_eq!(copies.len(), 1, "{}", run.text());
    assert_eq!(b.read(&copies[0]), b"bob's both");
    let held = skipped_read_only(&run);
    assert!(
        held.iter().any(|l| l.starts_with("skipped: mine: ")),
        "{}",
        run.text()
    );
    a.synced();
    assert!(!a.exists("mine"));
    assert!(a.conflict_copies("", "both").is_empty());
    server.fsck();
}

#[test]
fn a_commit_queued_before_write_access_was_taken_stays_here() {
    let sb = Sandbox::on_disk("accounts-queued");
    let mut server = Server::start(sb.path("data"));
    let alice = server.user("alice");
    let bob = server.user("bob");
    server
        .admin(&["folder", "add", "team", "--manager", "alice"])
        .expect_ok();
    server.admin(&["grant", "team", "bob", "write"]).expect_ok();
    let a = Device::as_user(&sb, &server, "a", &alice, "team");
    let b = Device::as_user(&sb, &server, "b", &bob, "team");
    a.write("f", "base");
    a.synced();
    b.synced();

    // Bob's commit is queued, and his device dies before the server hears it.
    b.write("mine.txt", "queued");
    b.write("f", "bob's edit");
    let run = b.cairn_env(&["sync"], &[("CAIRN_FP_CRASH", "sync.enqueued")]);
    assert!(run.was_killed(), "{}", run.text());
    // Meanwhile he is made a reader.
    server.admin(&["grant", "team", "bob", "read"]).expect_ok();
    let run = b.synced();
    assert!(
        run.stdout
            .contains("a commit waiting here since an earlier sync was refused"),
        "{}",
        run.text()
    );
    assert_eq!(b.read("mine.txt"), b"queued");
    assert_eq!(b.read("f"), b"bob's edit");
    let held = skipped_read_only(&run);
    assert!(
        held.iter().any(|l| l.starts_with("skipped: mine.txt: ")),
        "{}",
        run.text()
    );
    assert!(
        held.iter().any(|l| l.starts_with("skipped: f: ")),
        "{}",
        run.text()
    );
    a.synced();
    assert!(!a.exists("mine.txt"));
    assert_eq!(a.read("f"), b"base");
    // What it had uploaded is unreferenced on the server, which is no damage.
    server.fsck();
}

#[test]
fn a_user_whose_access_is_taken_keeps_everything_and_is_told() {
    let sb = Sandbox::on_disk("accounts-revoked");
    let server = Server::start(sb.path("data"));
    let alice = server.user("alice");
    let bob = server.user("bob");
    server
        .admin(&["folder", "add", "team", "--manager", "alice"])
        .expect_ok();
    server.admin(&["grant", "team", "bob", "write"]).expect_ok();
    let a = Device::as_user(&sb, &server, "a", &alice, "team");
    let b = Device::as_user(&sb, &server, "b", &bob, "team");
    a.write("f", "shared");
    a.synced();
    b.synced();
    server.admin(&["revoke", "team", "bob"]).expect_ok();
    b.write("g", "his");
    let picture = Picture::of(&b.root);
    let run = b.sync();
    assert!(!run.ok(), "{}", run.text());
    assert!(
        run.text().contains("no folder \"team\" for user \"bob\""),
        "{}",
        run.text()
    );
    assert!(Picture::of(&b.root)
        .diff(&picture, Ignore::default())
        .is_empty());
    // Revoking what is not there says so.
    let run = server.admin(&["revoke", "team", "bob"]);
    assert!(
        !run.ok() && run.text().contains("has no access"),
        "{}",
        run.text()
    );
}

#[test]
fn a_manager_shares_and_nobody_else_can() {
    let sb = Sandbox::on_disk("accounts-share");
    let server = Server::start(sb.path("data"));
    let alice = server.user("alice");
    let bob = server.user("bob");
    server.user("carol");
    server
        .admin(&["folder", "add", "team", "--manager", "alice"])
        .expect_ok();
    let a = Device::as_user(&sb, &server, "a", &alice, "team");
    a.write("f", "x");
    a.synced();
    a.cairn(&["access", "grant", "bob", "read"]).expect_ok();
    let listed = a.cairn(&["access"]).expect_ok();
    assert_eq!(
        listed
            .stdout
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>(),
        ["alice manage", "bob read"]
    );
    let b = Device::as_user(&sb, &server, "b", &bob, "team");
    b.synced();
    assert_eq!(b.read("f"), b"x");
    // A reader, or a writer, says nothing about who has access.
    let run = b.cairn(&["access", "grant", "carol", "write"]);
    assert!(
        !run.ok() && run.text().contains("only a manager"),
        "{}",
        run.text()
    );
    a.cairn(&["access", "grant", "bob", "write"]).expect_ok();
    let run = b.cairn(&["access", "grant", "carol", "write"]);
    assert!(
        !run.ok() && run.text().contains("only a manager"),
        "{}",
        run.text()
    );
    // Nobody unknown, and the folder never without a manager.
    let run = a.cairn(&["access", "grant", "nobody", "read"]);
    assert!(
        !run.ok() && run.text().contains("no user"),
        "{}",
        run.text()
    );
    let run = a.cairn(&["access", "revoke", "alice"]);
    assert!(
        !run.ok() && run.text().contains("last manager"),
        "{}",
        run.text()
    );
    a.cairn(&["access", "grant", "bob", "manage"]).expect_ok();
    a.cairn(&["access", "revoke", "alice"]).expect_ok();
    let run = a.sync();
    assert!(
        !run.ok() && run.text().contains("no folder"),
        "{}",
        run.text()
    );
    assert_eq!(a.read("f"), b"x");
    // The new manager can give it back.
    b.cairn(&["access", "grant", "alice", "write"]).expect_ok();
    a.synced();
}

/// Invariant 5 in a folder this device may only read, where something deleted here comes back: a tree
/// emptied as a missing mount leaves it pauses, and nothing is brought back into the empty folder until a
/// person says the deletes are meant.
#[test]
fn a_readers_emptied_tree_pauses_rather_than_filling_the_empty_folder() {
    let sb = Sandbox::on_disk("accounts-emptied");
    let server = Server::start(sb.path("data"));
    let alice = server.user("alice");
    let bob = server.user("bob");
    server
        .admin(&["folder", "add", "team", "--manager", "alice"])
        .expect_ok();
    server.admin(&["grant", "team", "bob", "read"]).expect_ok();
    let a = Device::as_user(&sb, &server, "a", &alice, "team");
    for i in 0..20 {
        a.write(&format!("d/{i}"), format!("{i}"));
    }
    a.synced();
    let b = Device::as_user(&sb, &server, "b", &bob, "team");
    b.synced();
    assert_eq!(b.read("d/7"), b"7");

    // As a mount gone from under the root leaves it: the folder there, and empty.
    let held = sb.path("b-held");
    std::fs::rename(&b.root, &held).unwrap();
    std::fs::create_dir(&b.root).unwrap();
    std::fs::rename(held.join(".cairn"), b.root.join(".cairn")).unwrap();
    let run = b.sync();
    assert_eq!(run.code, Some(2), "{}", run.text());
    assert!(run.stdout.contains("--allow-mass-delete"), "{}", run.text());
    let names: Vec<_> = std::fs::read_dir(&b.root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        names,
        [std::ffi::OsString::from(".cairn")],
        "written into it"
    );

    // Back again, nothing happened.
    std::fs::rename(b.root.join(".cairn"), held.join(".cairn")).unwrap();
    std::fs::remove_dir(&b.root).unwrap();
    std::fs::rename(&held, &b.root).unwrap();
    let run = b.synced();
    assert!(skipped_read_only(&run).is_empty(), "{}", run.text());

    // Meant: in a folder this device may only read, what it deleted comes back, all of it.
    std::fs::remove_dir_all(b.path("d")).unwrap();
    assert_eq!(b.sync().code, Some(2));
    b.cairn(&["sync", "--allow-mass-delete"]).expect_ok();
    assert_eq!(b.read("d/7"), b"7");
    a.synced();
    assert_eq!(a.read("d/19"), b"19");
}

/// One plain HTTP/1.1 request to the server, as a browser or a stray client would make it: the status
/// line's code, the headers in lower case, and the body.
fn http(server: &Server, method: &str, path: &str, token: Option<&str>) -> (u16, String, String) {
    use std::io::{Read, Write};
    let addr = server.url.strip_prefix("http://").unwrap();
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        conn,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/html\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (code, head.to_lowercase(), body.to_string())
}

#[test]
fn a_browser_at_the_servers_address_gets_a_page_and_nothing_else_without_a_token() {
    let sb = Sandbox::fast("accounts-browser");
    let server = Server::start(sb.path("data"));
    let a = Device::new(&sb, &server, "a");
    a.write("secret.txt", "a's");
    a.synced();

    // The address itself, as the tray's "Open in browser" opens it: a page saying what is here.
    let (code, head, body) = http(&server, "GET", "/", None);
    assert_eq!(code, 200, "{head}\n{body}");
    assert!(head.contains("content-type: text/html"), "{head}");
    assert!(
        head.contains("content-security-policy: default-src 'none'"),
        "{head}"
    );
    assert!(body.contains("This is a Cairn file sync server"), "{body}");
    // It names nothing a stranger could use: no version, user, folder, device or file.
    for named in [env!("CARGO_PKG_VERSION"), "admin", "default", "secret"] {
        assert!(!body.contains(named), "the page names {named:?}: {body}");
    }
    let (code, ..) = http(&server, "HEAD", "/", None);
    assert_eq!(code, 200);
    // Only to read it: anything else there is no request the server knows.
    let (code, ..) = http(&server, "POST", "/", None);
    assert_eq!(code, 405);

    // Everything else still wants a token, and the right one.
    let token = std::fs::read_to_string(&server.token_file).unwrap();
    let token = token.trim();
    for path in [
        "/v1/me",
        "/v1/ns/default/info",
        "/favicon.ico",
        "/index.html",
    ] {
        for wrong in [None, Some("not-the-token")] {
            let (code, head, body) = http(&server, "GET", path, wrong);
            assert_eq!(code, 401, "{path} with {wrong:?}: {head}\n{body}");
            assert_eq!(body, "missing or wrong token", "{path}");
        }
    }
    let (code, _, body) = http(&server, "GET", "/v1/me", Some(token));
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"default\""), "{body}");
}
