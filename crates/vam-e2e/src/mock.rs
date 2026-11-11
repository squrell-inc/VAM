//! Inline `mock:` blocks: validation through the mock rule types, and script files for vamd.

use crate::scenario::Scenario;
use std::path::Path;
use vam_inference::mock_script::MockScript;
use vam_inference::MockScripts;

/// The script a scenario's `mock:` block makes, `id` = scenario id (S2: no plan numbers).
pub fn to_script(s: &Scenario) -> Result<MockScript, String> {
    let json = serde_json::json!({ "id": s.id, "rules": s.mock });
    let script: MockScript = serde_json::from_value(json).map_err(|e| e.to_string())?;
    MockScripts::from_scripts(vec![script.clone()]).map_err(|e| e.to_string())?;
    Ok(script)
}

/// The `when.step` labels the raw rules use.
pub fn rule_labels(s: &Scenario) -> Vec<String> {
    s.mock
        .iter()
        .filter_map(|r| r.get("when")?.get("step")?.as_str().map(String::from))
        .collect()
}

/// Writes `<dir>/<id>.json` for each scenario with a mock block (scripts load at vamd start).
pub fn write_scripts(dir: &Path, scenarios: &[&Scenario]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    for s in scenarios.iter().filter(|s| !s.mock.is_empty()) {
        let json = serde_json::json!({ "id": s.id, "rules": s.mock });
        std::fs::write(
            dir.join(format!("{}.json", s.id)),
            serde_json::to_vec_pretty(&json)?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_round_trip_through_the_mock_loader() {
        let s: Scenario = serde_yaml::from_str(
            "id: s\ntitle: t\ntags: [chat]\nneeds: process\nwhen: [{approve: latest}]\nstatus: active\nmock:\n  - when: { step: a }\n    reply: { text: hi }\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        write_scripts(dir.path(), &[&s]).unwrap();
        let loaded = MockScripts::load_dir(dir.path()).unwrap();
        assert!(loaded.get("s").is_some());
        assert_eq!(rule_labels(&s), vec!["a".to_string()]);
    }
}
