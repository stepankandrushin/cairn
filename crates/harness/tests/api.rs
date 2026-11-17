//! What a server offers beyond what every server of the protocol does, named in its features, and what a
//! client of it uses: a client newer than its server asks first, and does without what is not named.

mod common;
use common::*;
use serde_json::{json, Value};

/// Every feature this build's server names.
const FEATURES: &[&str] = &["note_groups", "item_hash", "tokens"];

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
