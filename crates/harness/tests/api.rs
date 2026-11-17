//! What a server offers beyond what every server of the protocol does, named in its features, and what a
//! client of it uses: a client newer than its server asks first, and does without what is not named.

mod common;
use common::*;
use serde_json::Value;

/// Every feature this build's server names.
const FEATURES: &[&str] = &["note_groups", "item_hash"];

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
