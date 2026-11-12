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
