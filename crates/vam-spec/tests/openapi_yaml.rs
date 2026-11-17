//! `docs/api/openapi.yaml` is embedded in vamd and served at `/openapi.yaml`.
//! A YAML slip would ship silently, so parse it and check its shape.

use std::path::PathBuf;

fn load() -> serde_yaml::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/api/openapi.yaml");
    let text = std::fs::read_to_string(path).unwrap();
    serde_yaml::from_str(&text).unwrap()
}

#[test]
fn openapi_parses_as_yaml_with_the_inbox_contract() {
    let doc = load();
    assert!(doc["openapi"].as_str().unwrap().starts_with("3.1"));
    let paths = &doc["paths"];
    for p in ["/v1/runs", "/v1/runs/{urn}/messages", "/v1/runs/{urn}/stop"] {
        assert!(paths[p].is_mapping(), "missing path {p}");
    }
    let schemas = &doc["components"]["schemas"];
    for s in [
        "InboxReceipt",
        "SendRunMessageRequest",
        "SendRunMessageResponse",
        "StopRunResponse",
        "DiscardedMessage",
    ] {
        assert!(schemas[s].is_mapping(), "missing schema {s}");
    }
    assert!(schemas["RunSpec"]["properties"]["inboxMode"].is_mapping());
    assert!(schemas["CreateRunResponse"]["properties"]["inbox"].is_mapping());
}

#[test]
fn openapi_has_the_self_service_password_change() {
    let doc = load();
    let post = &doc["paths"]["/v1/me/password"]["post"];
    assert!(post.is_mapping(), "missing POST /v1/me/password");
    for code in ["200", "400", "401", "403", "429"] {
        assert!(
            post["responses"][code].is_mapping(),
            "missing response {code}"
        );
    }
    assert!(post["responses"]["429"]["headers"]["Retry-After"].is_mapping());
    assert!(post["responses"]["429"]["headers"]["X-VAM-Rate-Limited-Reason"].is_mapping());
    let schemas = &doc["components"]["schemas"];
    for s in ["ChangeMyPasswordRequest", "ChangeMyPasswordResponse"] {
        assert!(schemas[s].is_mapping(), "missing schema {s}");
    }
    let updated = &schemas["ChangeMyPasswordResponse"]["properties"]["passwordUpdatedAt"];
    assert_eq!(updated["type"].as_str(), Some("string"));
    assert_eq!(updated["format"].as_str(), Some("int64"));
}

#[test]
fn openapi_has_generated_ids_and_case_insensitive_login_names() {
    let doc = load();
    let create = &doc["paths"]["/v1/users"]["post"];
    for code in ["400", "403", "409"] {
        assert!(
            create["responses"][code].is_mapping(),
            "missing response {code}"
        );
    }
    let text = create["description"].as_str().unwrap();
    for needle in ["generates", "254", "ignoring"] {
        assert!(text.contains(needle), "create description lacks {needle}");
    }
    let schemas = &doc["components"]["schemas"];
    let user_id = &schemas["CreateUserRequest"]["properties"]["userId"];
    assert_eq!(user_id["deprecated"].as_bool(), Some(true));
    assert!(user_id["description"].as_str().unwrap().contains("empty"));
    let username = &schemas["CreateUserRequest"]["properties"]["username"];
    assert_eq!(username["maxLength"].as_u64(), Some(254));
    let account_id = schemas["UserAccount"]["properties"]["userId"]["description"]
        .as_str()
        .unwrap();
    assert!(account_id.contains("generated"), "{account_id}");
    assert!(
        schemas["LoginRequest"]["properties"]["username"]["description"]
            .as_str()
            .unwrap()
            .contains("ignoring letter case")
    );
    for p in ["/v1/users/{id}/keys", "/v1/users/{id}/keys/{key_id}"] {
        let verb = if p.ends_with("{key_id}") {
            "delete"
        } else {
            "get"
        };
        let d = doc["paths"][p][verb]["description"].as_str().unwrap();
        assert!(d.contains("login name"), "{p} lacks login name text");
    }
    let ns = schemas["GetMeResponse"]["properties"]["defaultNamespace"]["description"]
        .as_str()
        .unwrap();
    assert!(ns.contains("`+tag`"), "{ns}");
}

#[test]
fn every_local_ref_resolves() {
    fn walk(v: &serde_yaml::Value, doc: &serde_yaml::Value, bad: &mut Vec<String>) {
        match v {
            serde_yaml::Value::Mapping(m) => {
                for (k, val) in m {
                    if k.as_str() == Some("$ref") {
                        if let Some(r) = val.as_str().and_then(|r| r.strip_prefix("#/")) {
                            let mut cur = doc;
                            for seg in r.split('/') {
                                cur = &cur[seg];
                            }
                            if cur.is_null() {
                                bad.push(r.to_string());
                            }
                        }
                    }
                    walk(val, doc, bad);
                }
            }
            serde_yaml::Value::Sequence(s) => s.iter().for_each(|i| walk(i, doc, bad)),
            _ => {}
        }
    }
    let doc = load();
    let mut bad = Vec::new();
    walk(&doc, &doc, &mut bad);
    assert!(bad.is_empty(), "dangling $ref: {bad:?}");
}
