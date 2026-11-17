//! What a server offers beyond what every server of the protocol does, named in its features, and what a
//! client of it uses: a client newer than its server asks first, and does without what is not named.

mod common;
use common::*;
use serde_json::Value;

/// Every feature this build's server names.
const FEATURES: &[&str] = &["note_groups"];

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
