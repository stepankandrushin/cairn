//! What a server offers beyond what every server of the protocol does, named in its features, and what a
//! client of it uses: a client newer than its server asks first, and does without what is not named.

mod common;
use common::*;
use serde_json::{json, Value};

/// Every feature this build's server names.
const FEATURES: &[&str] = &[
    "note_groups",
    "item_hash",
    "tokens",
    "pairing",
    "passwords",
    "file_update",
    "thumbs",
];

fn token(server: &Server) -> String {
    std::fs::read_to_string(&server.token_file)
        .unwrap()
        .trim()
        .to_string()
}

fn names(v: &Value) -> Vec<String> {
    v.get("features")
        .map(|f| {
            f.as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_server_names_its_features_where_a_client_asks_and_none_before_them() {
    let sb = Sandbox::fast("api-features");
    let mut server = Server::start(sb.path("data"));
    let t = token(&server);
    let info = get(&server, "/v1/ns/cloud/info", Auth::Bearer(&t))
        .ok()
        .json();
    let me = get(&server, "/v1/me", Auth::Bearer(&t)).ok().json();
    assert_eq!(names(&info), FEATURES, "{info}");
    assert_eq!(names(&me), FEATURES, "{me}");

    // As a server before features answers: none named, and a client before them still told of grouped
    // notes the way it was.
    server.set_env(vec![(
        "CAIRN_FP_UNSUPPORTED".into(),
        "server.features".into(),
    )]);
    server.restart();
    let info = get(&server, "/v1/ns/cloud/info", Auth::Bearer(&t))
        .ok()
        .json();
    assert!(info.get("features").is_none(), "{info}");
    assert_eq!(info["note_groups"], true, "{info}");
    let me = get(&server, "/v1/me", Auth::Bearer(&t)).ok().json();
    assert!(me.get("features").is_none(), "{me}");

    // One left out alone.
    server.set_env(vec![(
        "CAIRN_FP_UNSUPPORTED".into(),
        "server.note_groups".into(),
    )]);
    server.restart();
    let info = get(&server, "/v1/ns/cloud/info", Auth::Bearer(&t))
        .ok()
        .json();
    assert!(!names(&info).contains(&"note_groups".to_string()), "{info}");
    assert!(info.get("note_groups").is_none(), "{info}");
}

const ROOT: &str = "00000000000000000000000000000000";

/// The item of the entry called `name` in a listing.
fn entry(listing: &Value, name: &str) -> Value {
    listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["display"] == name)
        .unwrap_or_else(|| panic!("no {name} in {listing}"))
        .clone()
}

#[test]
fn a_listed_file_carries_the_manifest_hash_its_device_committed() {
    let sb = Sandbox::fast("api-hash");
    let server = Server::start(sb.path("data"));
    let a = Device::new(&sb, &server, "a");
    a.write(
        "f",
        "the bytes a phone would hash before sending them again",
    );
    std::fs::create_dir(a.path("d")).unwrap();
    a.synced();
    let t = token(&server);
    let root = get(
        &server,
        &format!("/v1/ns/cloud/dir/{ROOT}"),
        Auth::Bearer(&t),
    )
    .ok()
    .json();
    let f = entry(&root, "f");
    let hash = f["hash"].as_str().expect("a file has its hash").to_string();
    assert_eq!(hash.len(), 64, "{f}");
    assert!(entry(&root, "d").get("hash").is_none(), "{root}");
    // The manifest the device's commit named.
    let changes = get(&server, "/v1/ns/cloud/changes?after=0", Auth::Bearer(&t))
        .ok()
        .text();
    assert!(
        changes.contains(&format!("\"manifest\":\"{hash}\"")),
        "{hash} not in {changes}"
    );
    // The same from the item's own view.
    let object = f["object"].as_str().unwrap();
    let item = get(
        &server,
        &format!("/v1/ns/cloud/item/{object}"),
        Auth::Bearer(&t),
    )
    .ok()
    .json();
    assert_eq!(item["item"]["hash"], hash.as_str(), "{item}");
}

fn read_token(file: &std::path::Path) -> String {
    std::fs::read_to_string(file).unwrap().trim().to_string()
}

fn patch(server: &Server, path: &str, auth: Auth, body: Value) -> Answer {
    request(
        server,
        "PATCH",
        path,
        auth,
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    )
}

fn delete(server: &Server, path: &str, auth: Auth) -> Answer {
    request(server, "DELETE", path, auth, &[], b"")
}

/// Each user lists, makes, relabels and takes back their own tokens and nobody else's; the one a
/// request came with is said, in the list and in `/v1/me`, with the hour it was last used; a token
/// taken back opens nothing from its next request on.
#[test]
fn a_user_manages_their_own_tokens_and_no_one_elses() {
    let sb = Sandbox::fast("api-tokens");
    let server = Server::start(sb.path("data"));
    let alice = read_token(&server.user("alice"));
    let bob = read_token(&server.user("bob"));

    let list = get(&server, "/v1/tokens", Auth::Bearer(&alice)).ok().json();
    let tokens = list["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 1, "{list}");
    assert_eq!(tokens[0]["kind"], "device", "{list}");
    assert_eq!(tokens[0]["current"], true, "{list}");
    let first = tokens[0]["id"].as_str().unwrap().to_string();
    let me = get(&server, "/v1/me", Auth::Bearer(&alice)).ok().json();
    assert_eq!(me["token"]["id"], first.as_str(), "{me}");
    assert_eq!(me["token"]["kind"], "device", "{me}");
    assert!(me.get("password").is_none(), "{me}");

    // A new one, shown once, which works at once and is not the current one.
    let made = post(
        &server,
        "/v1/tokens",
        Auth::Bearer(&alice),
        json!({"label": "phone"}),
    );
    assert_eq!(made.code, 201, "{}", made.text());
    let made = made.json();
    let phone = made["token"].as_str().unwrap().to_string();
    let phone_id = made["id"].as_str().unwrap().to_string();
    let me = get(&server, "/v1/me", Auth::Bearer(&phone)).ok().json();
    assert_eq!(me["user"], "alice");
    assert_eq!(me["token"]["label"], "phone", "{me}");
    let list = get(&server, "/v1/tokens", Auth::Bearer(&alice)).ok().json();
    let tokens = list["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2, "{list}");
    let p = tokens
        .iter()
        .find(|t| t["id"] == phone_id.as_str())
        .unwrap();
    assert!(p.get("current").is_none(), "{list}");
    assert!(
        list.to_string().find(&phone).is_none(),
        "a list never holds a token"
    );

    // A label that says nothing, or too much, is refused.
    for bad in [
        json!({"label": "  "}),
        json!({"label": "x".repeat(101)}),
        json!({"label": "a\nb"}),
    ] {
        let refused = post(&server, "/v1/tokens", Auth::Bearer(&alice), bad.clone());
        assert_eq!(refused.code, 400, "{bad}: {}", refused.text());
    }

    // Relabelled, by its owner alone.
    let path = format!("/v1/tokens/{phone_id}");
    let renamed = patch(
        &server,
        &path,
        Auth::Bearer(&alice),
        json!({"label": "old phone"}),
    )
    .ok()
    .json();
    assert_eq!(renamed["label"], "old phone", "{renamed}");
    for (what, answer) in [
        (
            "relabel",
            patch(&server, &path, Auth::Bearer(&bob), json!({"label": "mine"})),
        ),
        ("remove", delete(&server, &path, Auth::Bearer(&bob))),
    ] {
        assert_eq!(answer.code, 404, "bob's {what}: {}", answer.text());
    }
    let list = get(&server, "/v1/tokens", Auth::Bearer(&bob)).ok().json();
    assert_eq!(list["tokens"].as_array().unwrap().len(), 1, "{list}");

    // Taken back: it opens nothing any more, and the other token goes on.
    let gone = delete(&server, &path, Auth::Bearer(&alice));
    assert_eq!(gone.code, 204, "{}", gone.text());
    assert_eq!(get(&server, "/v1/me", Auth::Bearer(&phone)).code, 401);
    assert_eq!(delete(&server, &path, Auth::Bearer(&alice)).code, 404);
    get(&server, "/v1/me", Auth::Bearer(&alice)).ok();

    // The use, kept to the hour, shows in the admin's list as well: said by the first request of the
    // hour, without holding it up, so surely by now.
    let list = get(&server, "/v1/tokens", Auth::Bearer(&alice)).ok().json();
    let used = list["tokens"][0]["used"].as_i64();
    let list = server.admin(&["token", "list"]).expect_ok();
    let line = list
        .stdout
        .lines()
        .find(|l| l.starts_with(&first))
        .unwrap()
        .to_string();
    assert!(line.contains("  device  used "), "{line}");
    let hour = used.expect("a token used is said to be");
    assert_eq!(hour % 3600, 0, "{hour}");

    // Its own, the one this request came with: signed out with it.
    let mine = format!("/v1/tokens/{first}");
    assert_eq!(delete(&server, &mine, Auth::Bearer(&alice)).code, 204);
    assert_eq!(get(&server, "/v1/me", Auth::Bearer(&alice)).code, 401);
}

fn claim(server: &Server, code: &str, label: Option<&str>) -> Answer {
    let mut body = json!({"code": code});
    if let Some(l) = label {
        body["label"] = json!(l);
    }
    post(server, "/v1/pair/claim", Auth::None, body)
}

/// A signed-in device makes a code, with its link and QR code; another device, with no token, trades it
/// once for a token of its own of the same user's, which is listed as paired and works at once. A code
/// mistyped, used, or past its time trades for nothing.
#[test]
fn a_pairing_code_makes_one_token_of_the_users_once_and_in_time() {
    let sb = Sandbox::fast("api-pair");
    let mut server = Server::start(sb.path("data"));
    let alice = read_token(&server.user("alice"));
    let made = post(
        &server,
        "/v1/pair",
        Auth::Bearer(&alice),
        json!({"label": "phone", "origin": "https://cairn.example:8443/"}),
    )
    .ok()
    .json();
    let code = made["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 14, "{made}");
    assert_eq!(&code[4..5], "-", "{made}");
    let bare: String = code.chars().filter(|c| *c != '-').collect();
    assert_eq!(
        made["uri"],
        format!("cairn://pair?server=https%3A%2F%2Fcairn.example%3A8443&code={bare}").as_str(),
        "{made}"
    );
    assert!(made["qr_svg"].as_str().unwrap().contains("<svg"), "{made}");
    assert_eq!(made["expires_in"], 600);

    // Typed as a person types it: lower case, spaced.
    let typed = format!("{} {}", bare[..6].to_lowercase(), &bare[6..]);
    let paired = claim(&server, &typed, Some("iPhone"));
    assert_eq!(paired.code, 201, "{}", paired.text());
    let paired = paired.json();
    assert_eq!(paired["user"], "alice", "{paired}");
    let phone = paired["token"].as_str().unwrap().to_string();
    let me = get(&server, "/v1/me", Auth::Bearer(&phone)).ok().json();
    assert_eq!(me["user"], "alice");
    assert_eq!(me["token"]["kind"], "paired", "{me}");
    assert_eq!(me["token"]["label"], "iPhone", "{me}");
    assert_eq!(me["token"]["id"], paired["id"], "{me}");

    // Once only, and nothing for a wrong code.
    for (what, code) in [
        ("again", bare.as_str()),
        ("wrong", "0000-0000-0000"),
        ("not a code", "hello"),
    ] {
        let refused = claim(&server, code, None);
        assert_eq!(refused.code, 401, "{what}: {}", refused.text());
    }

    // The label the code was made with, when the device gives none; and a bad origin refused.
    let made = post(
        &server,
        "/v1/pair",
        Auth::Bearer(&alice),
        json!({"label": "tablet"}),
    )
    .ok()
    .json();
    assert!(
        made.get("uri").is_none() && made.get("qr_svg").is_none(),
        "{made}"
    );
    let paired = claim(&server, made["code"].as_str().unwrap(), None).json();
    let me = get(
        &server,
        "/v1/me",
        Auth::Bearer(paired["token"].as_str().unwrap()),
    )
    .ok()
    .json();
    assert_eq!(me["token"]["label"], "tablet", "{me}");
    for bad in ["ftp://x", "https://", "https://a/b", "https://a b"] {
        let refused = post(
            &server,
            "/v1/pair",
            Auth::Bearer(&alice),
            json!({"origin": bad}),
        );
        assert_eq!(refused.code, 400, "{bad}: {}", refused.text());
    }
    // Only signed in.
    assert_eq!(post(&server, "/v1/pair", Auth::None, json!({})).code, 401);
    let list = get(&server, "/v1/tokens", Auth::Bearer(&alice)).ok().json();
    let mut kinds: Vec<_> = list["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["kind"].as_str().unwrap().to_string())
        .collect();
    // Oldest first, and made within the same second here.
    kinds.sort();
    assert_eq!(kinds, ["device", "paired", "paired"], "{list}");

    // Past its time, it trades for nothing.
    server.set_env(vec![("CAIRN_FP_TUNE".into(), "server.pair_secs=1".into())]);
    server.restart();
    let made = post(&server, "/v1/pair", Auth::Bearer(&alice), json!({}))
        .ok()
        .json();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(
        claim(&server, made["code"].as_str().unwrap(), None).code,
        401
    );
}

/// `cairn-server user password` with what it reads on stdin.
fn set_password(server: &Server, user: &str, password: Option<&str>) -> Run {
    use std::io::Write;
    let mut cmd = std::process::Command::new(SERVER);
    cmd.args(["user", "password", user]);
    if password.is_none() {
        cmd.arg("--clear");
    }
    let mut child = cmd
        .arg("--data")
        .arg(&server.data)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(p) = password {
        writeln!(child.stdin.take().unwrap(), "{p}").unwrap();
    }
    Run::from(child.wait_with_output().unwrap())
}

fn sign_in(server: &Server, user: &str, password: &str) -> Answer {
    request(
        server,
        "POST",
        "/v1/session",
        Auth::None,
        &[
            ("Content-Type", "application/json"),
            (
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0",
            ),
        ],
        json!({"user": user, "password": password})
            .to_string()
            .as_bytes(),
    )
}

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

fn browsers(server: &Server, token: &str) -> usize {
    let list = get(server, "/v1/tokens", Auth::Bearer(token)).ok().json();
    list["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["kind"] == "browser")
        .count()
}

/// A user with a password signs a browser in with it, which makes a token for that browser alone: listed
/// with what the browser is, taken back by signing out or alone, and every one of them by a change of the
/// password. A wrong password and a user with none, or no such user, answer alike.
#[test]
fn a_password_signs_one_browser_in_and_each_is_signed_out_alone() {
    let sb = Sandbox::fast("api-password");
    let server = Server::start(sb.path("data"));
    let alice = read_token(&server.user("alice"));
    server.user("bob");
    let refused = sign_in(&server, "alice", "no password yet at all");
    assert_eq!(refused.code, 401, "{}", refused.text());

    let short = set_password(&server, "alice", Some("short"));
    assert!(
        !short.ok() && short.stderr.contains("at least 10"),
        "{}",
        short.text()
    );
    set_password(&server, "alice", Some("correct horse battery")).expect_ok();
    let me = get(&server, "/v1/me", Auth::Bearer(&alice)).ok().json();
    assert_eq!(me["password"], true, "{me}");

    let wrong = sign_in(&server, "alice", "correct horse battery!");
    let nobody = sign_in(&server, "carol", "correct horse battery");
    let none = sign_in(&server, "bob", "correct horse battery");
    for refused in [&wrong, &nobody, &none] {
        assert_eq!(refused.code, 401, "{}", refused.text());
        assert_eq!(refused.text(), "wrong user or password");
        assert!(refused.header("set-cookie").is_none());
    }

    let signed = sign_in(&server, "alice", "correct horse battery");
    assert_eq!(signed.code, 204, "{}", signed.text());
    let one = session_of(&signed);
    let me = get(&server, "/v1/me", Auth::Cookie(&one, false))
        .ok()
        .json();
    assert_eq!(me["user"], "alice");
    assert_eq!(me["token"]["kind"], "browser", "{me}");
    assert_eq!(me["token"]["label"], "Firefox on Linux", "{me}");
    let two = session_of(&sign_in(&server, "alice", "correct horse battery"));
    assert_eq!(browsers(&server, &alice), 2);

    // Signed out, its token goes with it, and the other browser stays in.
    let out = request(
        &server,
        "DELETE",
        "/v1/session",
        Auth::Cookie(&one, false),
        &[],
        b"",
    );
    assert_eq!(out.code, 204, "{}", out.text());
    assert_eq!(get(&server, "/v1/me", Auth::Cookie(&one, false)).code, 401);
    get(&server, "/v1/me", Auth::Cookie(&two, false)).ok();
    assert_eq!(browsers(&server, &alice), 1);

    // Changed by the user, who gives the one they have; then every browser it signed in is out.
    let change = |auth: Auth, body: Value| {
        request(
            &server,
            "PUT",
            "/v1/me/password",
            auth,
            &[("Content-Type", "application/json")],
            body.to_string().as_bytes(),
        )
    };
    let no = change(Auth::Bearer(&alice), json!({"new": "another long one"}));
    assert_eq!(no.code, 403, "{}", no.text());
    let no = change(
        Auth::Bearer(&alice),
        json!({"current": "correct horse battery", "new": "short"}),
    );
    assert_eq!(no.code, 400, "{}", no.text());
    let yes = change(
        Auth::Bearer(&alice),
        json!({"current": "correct horse battery", "new": "another long one"}),
    );
    assert_eq!(yes.code, 204, "{}", yes.text());
    assert_eq!(get(&server, "/v1/me", Auth::Cookie(&two, false)).code, 401);
    assert_eq!(browsers(&server, &alice), 0);
    assert_eq!(sign_in(&server, "alice", "correct horse battery").code, 401);
    let three = session_of(&sign_in(&server, "alice", "another long one"));

    // A browser's own change keeps it in.
    let kept = change(
        Auth::Cookie(&three, true),
        json!({"current": "another long one", "new": "a third long one"}),
    );
    assert_eq!(kept.code, 204, "{}", kept.text());
    get(&server, "/v1/me", Auth::Cookie(&three, false)).ok();

    // Taken away by the administrator: no sign-in by it, and no browser it signed in.
    set_password(&server, "alice", None).expect_ok();
    assert_eq!(
        get(&server, "/v1/me", Auth::Cookie(&three, false)).code,
        401
    );
    assert_eq!(sign_in(&server, "alice", "a third long one").code, 401);
    let me = get(&server, "/v1/me", Auth::Bearer(&alice)).ok().json();
    assert!(me.get("password").is_none(), "{me}");
}

/// A picture of `w` by `h`, as `format`.
fn picture(w: u32, h: u32, format: image::ImageFormat) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, format)
        .unwrap();
    out.into_inner()
}

/// A JPEG whose EXIF says it is to be turned a quarter clockwise to be seen, as a camera held upright
/// writes one.
fn turned_jpeg(w: u32, h: u32) -> Vec<u8> {
    let plain = picture(w, h, image::ImageFormat::Jpeg);
    let mut exif = b"Exif\0\0MM\x00\x2a\x00\x00\x00\x08\x00\x01".to_vec();
    exif.extend_from_slice(b"\x01\x12\x00\x03\x00\x00\x00\x01\x00\x06\x00\x00");
    exif.extend_from_slice(b"\x00\x00\x00\x00");
    let mut out = plain[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&exif);
    out.extend_from_slice(&plain[2..]);
    out
}

#[track_caller]
fn dimensions(answer: &Answer) -> (u32, u32) {
    assert_eq!(answer.code, 200, "{}", answer.text());
    assert_eq!(answer.header("content-type").as_deref(), Some("image/jpeg"));
    let img = image::load_from_memory_with_format(&answer.body, image::ImageFormat::Jpeg).unwrap();
    (img.width(), img.height())
}

/// A picture's thumbnail is a JPEG no larger than asked, turned as its EXIF says, made once and kept; a
/// file the server does not decode has none until a client sends its own, which every size is made from;
/// and a public link reads them but sends none.
#[test]
fn a_thumbnail_is_made_of_a_picture_or_sent_by_a_client_and_kept() {
    let sb = Sandbox::fast("api-thumbs");
    let server = Server::start(sb.path("data"));
    let a = Device::new(&sb, &server, "a");
    a.write("wide.png", picture(2000, 1000, image::ImageFormat::Png));
    a.write("upright.jpg", turned_jpeg(200, 100));
    a.write("notes.txt", "no picture at all");
    a.write("IMG_0001.HEIC", "bytes the server does not decode");
    a.synced();
    let t = token(&server);
    let root = get(
        &server,
        &format!("/v1/ns/cloud/dir/{ROOT}"),
        Auth::Bearer(&t),
    )
    .ok()
    .json();
    let id = |name: &str| entry(&root, name)["object"].as_str().unwrap().to_string();
    let thumb = |object: &str, size: u32| {
        get(
            &server,
            &format!("/v1/ns/cloud/thumb/{object}?size={size}"),
            Auth::Bearer(&t),
        )
    };

    assert_eq!(dimensions(&thumb(&id("wide.png"), 128)), (128, 64));
    assert_eq!(dimensions(&thumb(&id("wide.png"), 1024)), (1024, 512));
    assert_eq!(
        dimensions(&thumb(&id("upright.jpg"), 128)),
        (64, 128),
        "turned"
    );
    // Not larger than the picture itself.
    assert_eq!(dimensions(&thumb(&id("upright.jpg"), 512)), (100, 200));
    for name in ["notes.txt", "IMG_0001.HEIC"] {
        let none = thumb(&id(name), 256);
        assert_eq!(none.code, 404, "{name}: {}", none.text());
        assert_eq!(none.text(), "no thumbnail");
    }
    assert_eq!(thumb(&id("wide.png"), 300).code, 400);

    // Kept: by its revision, for good, and asked again by its tag, not sent again.
    let entry_rev = entry(&root, "wide.png")["revision"]
        .as_str()
        .unwrap()
        .to_string();
    let fixed = get(
        &server,
        &format!(
            "/v1/ns/cloud/thumb/{}?size=128&revision={entry_rev}",
            id("wide.png")
        ),
        Auth::Bearer(&t),
    );
    assert!(fixed.header("cache-control").unwrap().contains("immutable"));
    let etag = thumb(&id("wide.png"), 128).header("etag").unwrap();
    let again = request(
        &server,
        "GET",
        &format!("/v1/ns/cloud/thumb/{}?size=128", id("wide.png")),
        Auth::Bearer(&t),
        &[("If-None-Match", &etag)],
        b"",
    );
    assert_eq!(again.code, 304, "{}", again.text());

    // A client's own, for what the server does not decode.
    let heic = id("IMG_0001.HEIC");
    let put_thumb = |bytes: &[u8]| {
        request(
            &server,
            "PUT",
            &format!("/v1/ns/cloud/thumb/{heic}"),
            Auth::Bearer(&t),
            &[("Content-Type", "image/jpeg")],
            bytes,
        )
    };
    assert_eq!(put_thumb(b"not a jpeg").code, 400);
    assert_eq!(
        put_thumb(&picture(2000, 100, image::ImageFormat::Jpeg)).code,
        400
    );
    assert_eq!(
        put_thumb(&picture(512, 384, image::ImageFormat::Png)).code,
        400
    );
    let sent = put_thumb(&picture(512, 384, image::ImageFormat::Jpeg));
    assert_eq!(sent.code, 204, "{}", sent.text());
    assert_eq!(dimensions(&thumb(&heic, 256)), (256, 192));
    assert_eq!(dimensions(&thumb(&heic, 1024)), (512, 384));

    // By a public link: read, never sent.
    let link = post(
        &server,
        "/v1/ns/cloud/shares",
        Auth::Bearer(&t),
        json!({"object": ROOT}),
    )
    .ok()
    .json();
    let link = link["link"].as_str().unwrap().to_string();
    let by_link = get(
        &server,
        &format!("/v1/links/{link}/thumb/{}?size=128", id("wide.png")),
        Auth::None,
    );
    assert_eq!(dimensions(&by_link), (128, 64));
    let refused = request(
        &server,
        "PUT",
        &format!("/v1/links/{link}/thumb/{heic}"),
        Auth::None,
        &[],
        &picture(64, 64, image::ImageFormat::Jpeg),
    );
    assert_eq!(refused.code, 403, "{}", refused.text());
}
