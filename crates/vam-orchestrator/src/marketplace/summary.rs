//! What a listing installs and can reach, for the admin who is about to trust it.
//!
//! The per-artifact capability summaries come from the shared package check
//! (`vam_spec::package`); this module folds them into the listing's `summary`: every artifact, a
//! flat list of tools with their side effects and approvals, and one entry per host the tools can
//! reach, with the union of methods and paths. A `/` path prefix shows as `anyPath` ("any path on
//! the host"), and a write declared by prefix as `writesByPrefix`.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use vam_spec::package::CapabilitySummary;

/// The listing's `summary` object.
pub fn listing_summary(artifacts: &[CapabilitySummary], connector: Option<&str>) -> Value {
    #[derive(Default)]
    struct Host {
        methods: Vec<String>,
        paths: Vec<(String, bool, bool)>,
        can_write: bool,
        writes_by_prefix: bool,
        bindings: Vec<String>,
        text: Vec<String>,
    }
    let mut hosts: BTreeMap<String, Host> = BTreeMap::new();
    let mut tools: Vec<Value> = Vec::new();
    for a in artifacts {
        for t in &a.tools {
            let mut tool = json!({
                "skill": a.urn,
                "name": t.name,
                "description": t.description,
                "sideEffect": t.side_effect,
                "approvalRequired": t.approval_required,
            });
            if !t.show_card.is_empty() {
                tool["showCard"] = json!(t.show_card);
            }
            if !t.ask_card.is_empty() {
                tool["askCard"] = json!(t.ask_card);
            }
            tools.push(tool);
            for e in &t.egress {
                let h = hosts.entry(e.host.clone()).or_default();
                for m in &e.methods {
                    if !h.methods.contains(m) {
                        h.methods.push(m.clone());
                    }
                }
                for p in &e.paths {
                    let key = (p.path.clone(), p.exact, p.any_path);
                    if !h.paths.contains(&key) {
                        h.paths.push(key);
                    }
                }
                h.can_write |= e.can_write;
                h.writes_by_prefix |= e.writes_by_prefix;
                if !e.binding.is_empty() && !h.bindings.contains(&e.binding) {
                    h.bindings.push(e.binding.clone());
                }
                if !h.text.contains(&e.text) {
                    h.text.push(e.text.clone());
                }
            }
        }
    }
    let egress: Vec<Value> = hosts
        .into_iter()
        .map(|(host, mut h)| {
            h.methods.sort();
            h.paths.sort();
            let any_path = h.paths.iter().any(|p| p.2);
            json!({
                "host": host,
                "methods": h.methods,
                "paths": h.paths.iter().map(|(path, exact, any)| json!({
                    "path": path, "exact": exact, "anyPath": any
                })).collect::<Vec<_>>(),
                "anyPath": any_path,
                "canWrite": h.can_write,
                "writesByPrefix": h.writes_by_prefix,
                "bindings": h.bindings,
                "text": h.text,
            })
        })
        .collect();
    let requires: Vec<&str> = {
        let mut r: Vec<&str> = artifacts
            .iter()
            .map(|a| a.requires_connector.as_str())
            .filter(|c| !c.is_empty())
            .collect();
        r.sort_unstable();
        r.dedup();
        r
    };
    // The connector files the listing ships: who publishes each, how members sign in, which
    // server it reaches, with which vetted tools. The admin reads this before installing.
    let connectors: Vec<Value> = artifacts
        .iter()
        .filter_map(|a| {
            let c = a.connector.as_ref()?;
            let mut v = serde_json::to_value(c).ok()?;
            v["urn"] = json!(a.urn);
            v["displayName"] = json!(a.display_name);
            Some(v)
        })
        .collect();
    json!({
        "artifacts": artifacts,
        "tools": tools,
        "egress": egress,
        "connector": connector,
        "connectors": connectors,
        "requiresConnector": requires,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::package::{EgressSummary, PathReach, ToolSummary};

    fn skill(urn: &str, tools: Vec<ToolSummary>) -> CapabilitySummary {
        CapabilitySummary {
            kind: "skill".into(),
            urn: urn.into(),
            tools,
            ..Default::default()
        }
    }

    fn tool(name: &str, egress: Vec<EgressSummary>) -> ToolSummary {
        ToolSummary {
            name: name.into(),
            description: String::new(),
            side_effect: "read_only".into(),
            approval_required: false,
            show_card: String::new(),
            ask_card: String::new(),
            egress,
        }
    }

    fn eg(
        host: &str,
        methods: &[&str],
        paths: Vec<PathReach>,
        write_prefix: bool,
    ) -> EgressSummary {
        EgressSummary {
            binding: String::new(),
            host: host.into(),
            methods: methods.iter().map(|m| m.to_string()).collect(),
            paths,
            can_write: methods.iter().any(|m| *m != "GET" && *m != "HEAD"),
            writes_by_prefix: write_prefix,
            text: format!("{} on {host}", methods.join("/")),
        }
    }

    fn reach(path: &str, exact: bool) -> PathReach {
        PathReach {
            path: path.into(),
            exact,
            any_path: path == "/" && !exact,
        }
    }

    #[test]
    fn a_whole_host_prefix_is_named_any_path() {
        let s = listing_summary(
            &[skill(
                "urn:vam:skill:d/ns/notes",
                vec![tool(
                    "search",
                    vec![eg(
                        "notes.example.test",
                        &["GET"],
                        vec![reach("/", false)],
                        false,
                    )],
                )],
            )],
            None,
        );
        assert_eq!(s["egress"][0]["host"], "notes.example.test");
        assert_eq!(s["egress"][0]["anyPath"], true);
        assert_eq!(s["egress"][0]["paths"][0]["anyPath"], true);
        assert_eq!(s["egress"][0]["paths"][0]["exact"], false);
        assert_eq!(s["tools"][0]["name"], "search");
        assert!(s["connector"].is_null());
    }

    #[test]
    fn hosts_are_merged_with_the_union_of_methods_and_paths() {
        let s = listing_summary(
            &[skill(
                "urn:vam:skill:d/ns/mail",
                vec![
                    tool(
                        "read",
                        vec![eg(
                            "graph.microsoft.com",
                            &["GET"],
                            vec![reach("/v1.0/me/messages", false)],
                            false,
                        )],
                    ),
                    tool(
                        "send",
                        vec![eg(
                            "graph.microsoft.com",
                            &["POST"],
                            vec![reach("/v1.0/me/sendMail", true)],
                            false,
                        )],
                    ),
                ],
            )],
            Some("urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365"),
        );
        let egress = s["egress"].as_array().unwrap();
        assert_eq!(egress.len(), 1);
        assert_eq!(egress[0]["methods"], json!(["GET", "POST"]));
        assert_eq!(egress[0]["paths"].as_array().unwrap().len(), 2);
        assert_eq!(egress[0]["anyPath"], false);
        assert_eq!(egress[0]["canWrite"], true);
        assert_eq!(
            s["connector"],
            "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365"
        );
    }

    #[test]
    fn a_write_by_prefix_is_flagged() {
        let s = listing_summary(
            &[skill(
                "urn:vam:skill:d/ns/x",
                vec![tool(
                    "create",
                    vec![eg(
                        "graph.microsoft.com",
                        &["POST"],
                        vec![reach("/v1.0/me/events", false)],
                        true,
                    )],
                )],
            )],
            None,
        );
        assert_eq!(s["egress"][0]["writesByPrefix"], true);
    }
}
