//! The broker's manifest, as the operator registers it with `PUT /v1/hosted/brokers`.
//!
//! The one source: `vam-web-broker manifest` prints it, `vam node init --broker web` and
//! `scripts/run-web-broker.sh` register it. The search key is a vault binding, never an argument.

/// The manifest body (docs: hosted/web).
pub const MANIFEST_JSON: &str = include_str!("../manifest.json");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_json_for_vam_web_with_both_tools() {
        let v: serde_json::Value = serde_json::from_str(MANIFEST_JSON).expect("manifest is JSON");
        assert_eq!(v["brokerId"], "vam.web");
        let tools: Vec<&str> = v["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|t| t["toolName"].as_str())
            .collect();
        assert_eq!(tools, ["web_search", "web_fetch"]);
        assert!(
            v.get("unixPath").is_none(),
            "the socket path is not part of the manifest"
        );
    }
}
