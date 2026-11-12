//! The broker's manifest, as the operator registers it with `PUT /v1/hosted/brokers`.

/// The manifest body (docs: hosted/schedule).
pub const MANIFEST_JSON: &str = r#"{
  "brokerId": "vam.schedule",
  "trustTier": "TRUST_TIER_FIRST_PARTY",
  "secretBindings": [],
  "egressDomains": [],
  "identityRequirement": { "userId": true, "agent": true },
  "runRequestPermission": {
    "grantTools": ["schedule_create", "schedule_update"],
    "loopGuardedTools": ["schedule_create", "schedule_update", "schedule_run_now"],
    "surface": "schedule",
    "maxRequestsPerMinute": 60,
    "maxRequestsPerDay": 5000,
    "maxActiveGrantsPerUser": 50
  },
  "tools": [
    { "toolName": "schedule_create", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" },
    { "toolName": "schedule_list", "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY" },
    { "toolName": "schedule_get", "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY" },
    { "toolName": "schedule_update", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" },
    { "toolName": "schedule_delete", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" },
    { "toolName": "schedule_run_now", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" }
  ]
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools;
    use vam_spec::v1::SideEffectClass;

    fn manifest() -> serde_json::Value {
        serde_json::from_str(MANIFEST_JSON).expect("manifest is JSON")
    }

    #[test]
    fn equals_the_e2e_piece_manifest() {
        let yaml = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../e2e/world/schedule-broker.yaml"
        ))
        .expect("piece file");
        let piece: serde_json::Value = serde_yaml::from_str(&yaml).expect("piece yaml");
        assert_eq!(piece["body"]["manifest"], manifest());
    }

    #[test]
    fn list_tools_side_effects_equal_the_manifest() {
        let m = manifest();
        let want: Vec<(String, i32)> = m["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| {
                let name = t["toolName"].as_str().expect("name").to_string();
                let se = match t["sideEffect"].as_str().expect("side effect") {
                    "SIDE_EFFECT_CLASS_READ_ONLY" => SideEffectClass::ReadOnly as i32,
                    "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" => SideEffectClass::MutableExternal as i32,
                    other => panic!("unexpected side effect {other}"),
                };
                (name, se)
            })
            .collect();
        let got: Vec<(String, i32)> = tools::tool_specs()
            .into_iter()
            .map(|t| (t.name, t.side_effect))
            .collect();
        assert_eq!(got, want);
    }
}
