//! Validation of a skill's connection egress declaration: `SkillManifest.requires_connector`
//! and `ToolSpec.egress` (design/connectors.md 4.1). Declared here, enforced by vamd.

use crate::v1::{
    tool_spec::ExecutionProvider, SideEffectClass, SkillManifest, ToolEgress, ToolSpec,
};
use crate::validation::ValidationError;

const METHODS: [&str; 6] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// Warning for a READ_ONLY tool that declares a method that can write.
pub const READ_ONLY_WRITE_METHOD_WARNING: &str =
    "a READ_ONLY tool declares an egress method other than GET or HEAD";

/// Warning for a tool that declares a write method (anything but GET or HEAD) by path prefix: a
/// prefix also admits every path beneath it, so a create can reach a forward or a send.
pub const WRITE_METHOD_PREFIX_WARNING: &str =
    "declares a write method (not GET or HEAD) by path prefix, which also admits every path beneath it; declare exact `paths` instead";

/// True when `methods` holds one that can write.
fn writes(methods: &[String]) -> bool {
    methods.iter().any(|m| m != "GET" && m != "HEAD")
}

/// True when a shape declares a write method together with a path prefix.
pub fn writes_by_prefix(e: &ToolEgress) -> bool {
    writes(&e.methods) && !e.path_prefixes.is_empty()
}

/// An exact path names one resource: it starts with '/', has at least one segment, no trailing
/// '/', no empty segment, no '.' or '..' segment, no '%' (an exact path is written decoded), no
/// query or fragment, and only bytes the envelope can match (printable ASCII that a URL path does
/// not re-encode: no space, '"', '<', '>', '`', '{', '}', '\\').
pub fn valid_exact_path(p: &str) -> bool {
    p.len() > 1
        && p.starts_with('/')
        && !p.ends_with('/')
        && !p.contains(['?', '#', '%', '"', '<', '>', '`', '{', '}', '\\'])
        && p.bytes().all(|b| b > 0x20 && b < 0x7f)
        && p[1..]
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

fn valid_dns_host(h: &str) -> bool {
    crate::connector::valid_host(h)
        && !h.contains("..")
        && !h.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

fn valid_path_prefix(p: &str) -> bool {
    p.starts_with('/')
        && !p.contains("//")
        && !p.contains(['?', '#', '%'])
        && !p.split('/').any(|seg| seg == "..")
}

/// Shape rules for one tool's `egress`. Errors are collected into `errors` at `egress[j]...`.
pub fn validate_tool_egress(tool: &ToolSpec, errors: &mut ValidationError) {
    if tool.egress.is_empty() {
        return;
    }
    if !matches!(
        tool.execution_provider,
        Some(ExecutionProvider::LocalExec(_))
    ) {
        errors.push(
            "egress",
            "Tool egress requires a local_exec execution provider",
            "egress_needs_local_exec",
        );
    }
    for (j, e) in tool.egress.iter().enumerate() {
        if e.binding.trim().is_empty() {
            errors.push(
                format!("egress[{j}].binding"),
                "Egress binding cannot be empty",
                "required",
            );
        }
        if !valid_dns_host(&e.host) {
            errors.push(
                format!("egress[{j}].host"),
                format!(
                    "Egress host must be an exact lowercase DNS name (no scheme, port, wildcard or IP), got '{}'",
                    e.host
                ),
                "egress_host",
            );
        }
        if e.methods.is_empty() {
            errors.push(
                format!("egress[{j}].methods"),
                "Egress methods cannot be empty",
                "required",
            );
        }
        for (k, m) in e.methods.iter().enumerate() {
            if !METHODS.contains(&m.as_str()) {
                errors.push(
                    format!("egress[{j}].methods[{k}]"),
                    format!(
                        "Egress method must be one of GET HEAD POST PUT PATCH DELETE, got '{m}'"
                    ),
                    "egress_method",
                );
            } else if e.methods[..k].contains(m) {
                errors.push(
                    format!("egress[{j}].methods[{k}]"),
                    format!("Duplicate egress method '{m}'"),
                    "unique",
                );
            }
        }
        if e.path_prefixes.is_empty() && e.paths.is_empty() {
            errors.push(
                format!("egress[{j}].path_prefixes"),
                "Egress needs at least one of path_prefixes (use \"/\" for the whole host) or paths (exact)",
                "required",
            );
        }
        for (k, p) in e.paths.iter().enumerate() {
            if !valid_exact_path(p) {
                errors.push(
                    format!("egress[{j}].paths[{k}]"),
                    format!(
                        "Egress exact path must start with '/', name at least one segment, end with no '/', and hold no '.' or '..' segment, '//', '?', '#', '%', space, '{{' or '}}', got '{p}'"
                    ),
                    "egress_path",
                );
            } else if e.paths[..k].contains(p) {
                errors.push(
                    format!("egress[{j}].paths[{k}]"),
                    format!("Duplicate egress exact path '{p}'"),
                    "unique",
                );
            }
        }
        for (k, p) in e.path_prefixes.iter().enumerate() {
            if !valid_path_prefix(p) {
                errors.push(
                    format!("egress[{j}].path_prefixes[{k}]"),
                    format!(
                        "Egress path prefix must start with '/' and hold no '..' segment, '//', '?', '#' or '%', got '{p}'"
                    ),
                    "egress_path_prefix",
                );
            }
        }
    }
}

/// Manifest-level rules: the connector URN shape, and egress needing a connector.
pub fn validate_manifest_egress(m: &SkillManifest, errors: &mut ValidationError) {
    let has_connector = !m.requires_connector.is_empty();
    if has_connector && !crate::connector::valid_urn(&m.requires_connector) {
        errors.push(
            "requires_connector",
            format!(
                "requires_connector must be {}<host>/<publisher>/<name>, got '{}'",
                crate::connector::URN_PREFIX,
                m.requires_connector
            ),
            "connector_urn",
        );
    }
    if !has_connector {
        for (i, t) in m.tools.iter().enumerate() {
            if !t.egress.is_empty() {
                errors.push(
                    format!("tools[{i}].egress"),
                    "Tool egress requires the skill to set requires_connector",
                    "egress_needs_connector",
                );
            }
        }
    }
}

/// Publish warnings: a READ_ONLY tool declaring a method other than GET or HEAD, and any tool
/// declaring a write method by path prefix (exact `paths` are the way to declare a write).
pub fn skill_egress_warnings(m: &SkillManifest) -> Vec<String> {
    let mut out = Vec::new();
    for t in &m.tools {
        if t.side_effect == SideEffectClass::ReadOnly as i32
            && t.egress.iter().any(|e| writes(&e.methods))
        {
            out.push(format!(
                "tool '{}': {READ_ONLY_WRITE_METHOD_WARNING}",
                t.name
            ));
        }
        if t.egress.iter().any(writes_by_prefix) {
            out.push(format!("tool '{}': {WRITE_METHOD_PREFIX_WARNING}", t.name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{LocalExecSpec, ToolEgress};
    use crate::validation::Validate;

    fn egress() -> ToolEgress {
        ToolEgress {
            binding: "graph".into(),
            host: "graph.microsoft.com".into(),
            methods: vec!["GET".into()],
            path_prefixes: vec!["/v1.0/me/messages".into()],
            paths: vec![],
        }
    }

    fn manifest(e: ToolEgress) -> SkillManifest {
        crate::encoding::json_to_proto::<SkillManifest>(
            "vam.v1.SkillManifest",
            &serde_json::json!({
                "resourceId": {"urn": "urn:vam:skill:local.vam.dev/e2e/x"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "requiresConnector": "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365",
                "tools": [{
                    "name": "t", "description": "d", "inputSchemaJson": "{}",
                    "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY",
                    "localExec": {"command": "python3", "entrypoint": "./p.py"}
                }]
            })
            .to_string(),
        )
        .map(|mut m: SkillManifest| {
            m.tools[0].egress = vec![e];
            m
        })
        .unwrap()
    }

    fn codes(m: &SkillManifest) -> Vec<String> {
        match m.validate() {
            Ok(()) => vec![],
            Err(e) => e
                .field_violations
                .iter()
                .map(|v| v.constraint.clone())
                .collect(),
        }
    }

    fn with(f: impl FnOnce(&mut ToolEgress)) -> SkillManifest {
        let mut e = egress();
        f(&mut e);
        manifest(e)
    }

    #[test]
    fn a_good_declaration_passes() {
        assert!(manifest(egress()).validate().is_ok());
    }

    #[test]
    fn no_new_fields_is_unchanged() {
        let mut m = manifest(egress());
        m.requires_connector.clear();
        m.tools[0].egress.clear();
        assert!(m.validate().is_ok());
    }

    #[test]
    fn bad_connector_urn_is_refused() {
        let mut m = manifest(egress());
        m.requires_connector = "microsoft-365".into();
        assert!(codes(&m).contains(&"connector_urn".to_string()));
    }

    #[test]
    fn egress_without_connector_is_refused() {
        let mut m = manifest(egress());
        m.requires_connector.clear();
        assert!(codes(&m).contains(&"egress_needs_connector".to_string()));
    }

    #[test]
    fn egress_without_local_exec_is_refused() {
        let mut m = manifest(egress());
        m.tools[0].execution_provider = None;
        assert!(codes(&m).contains(&"egress_needs_local_exec".to_string()));
        m.tools[0].execution_provider = Some(ExecutionProvider::LocalExec(LocalExecSpec {
            command: "python3".into(),
            entrypoint: "./p.py".into(),
            ..Default::default()
        }));
        assert!(m.validate().is_ok());
    }

    #[test]
    fn empty_binding_is_refused() {
        assert!(codes(&with(|e| e.binding = " ".into())).contains(&"required".to_string()));
    }

    #[test]
    fn bad_hosts_are_refused() {
        for h in [
            "",
            "https://graph.microsoft.com",
            "graph.microsoft.com:443",
            "*.microsoft.com",
            "Graph.Microsoft.com",
            "10.0.0.1",
            ".graph.com",
            "graph.com/",
        ] {
            let c = codes(&with(|e| e.host = h.into()));
            assert!(c.contains(&"egress_host".to_string()), "{h}");
        }
    }

    #[test]
    fn empty_bad_and_duplicate_methods_are_refused() {
        assert!(codes(&with(|e| e.methods.clear())).contains(&"required".to_string()));
        assert!(codes(&with(|e| e.methods = vec!["TRACE".into()]))
            .contains(&"egress_method".to_string()));
        assert!(
            codes(&with(|e| e.methods = vec!["get".into()])).contains(&"egress_method".to_string())
        );
        assert!(
            codes(&with(|e| e.methods = vec!["GET".into(), "GET".into()]))
                .contains(&"unique".to_string())
        );
    }

    #[test]
    fn empty_path_prefixes_are_refused_and_slash_is_allowed() {
        assert!(codes(&with(|e| e.path_prefixes.clear())).contains(&"required".to_string()));
        assert!(with(|e| e.path_prefixes = vec!["/".into()])
            .validate()
            .is_ok());
    }

    #[test]
    fn bad_path_prefixes_are_refused() {
        for p in ["v1.0", "/a/../b", "/..", "/a//b", "/a?x=1", "/a#f", "/a%2f"] {
            let c = codes(&with(|e| e.path_prefixes = vec![p.into()]));
            assert!(c.contains(&"egress_path_prefix".to_string()), "{p}");
        }
    }

    #[test]
    fn a_read_only_tool_with_a_write_method_warns_only() {
        let m = with(|e| e.methods = vec!["POST".into()]);
        assert!(m.validate().is_ok());
        let w = skill_egress_warnings(&m);
        assert!(w.iter().any(|x| x.contains(READ_ONLY_WRITE_METHOD_WARNING)));
        assert!(skill_egress_warnings(&manifest(egress())).is_empty());
    }

    #[test]
    fn exact_paths_alone_satisfy_the_shape() {
        let m = with(|e| {
            e.methods = vec!["POST".into()];
            e.path_prefixes.clear();
            e.paths = vec!["/v1.0/me/events".into(), "/v1.0/me/sendMail".into()];
        });
        let mut m = m;
        m.tools[0].side_effect = SideEffectClass::MutableExternal as i32;
        assert!(m.validate().is_ok());
        assert!(skill_egress_warnings(&m).is_empty());
    }

    #[test]
    fn a_shape_with_neither_prefixes_nor_paths_is_refused() {
        let c = codes(&with(|e| {
            e.path_prefixes.clear();
            e.paths.clear();
        }));
        assert!(c.contains(&"required".to_string()));
    }

    #[test]
    fn bad_exact_paths_are_refused() {
        for p in [
            "",
            "/",
            "v1.0/me/events",
            "/v1.0/me/events/",
            "/a//b",
            "/a/../b",
            "/a/./b",
            "/..",
            "/a?x=1",
            "/a#f",
            "/a%2fb",
            "/a%25",
            "/a b",
            "/messages/{id}/send",
            "/a\\b",
            "/caf\u{e9}",
            "/a\nb",
        ] {
            let c = codes(&with(|e| e.paths = vec![p.into()]));
            assert!(c.contains(&"egress_path".to_string()), "{p:?}");
        }
        let c = codes(&with(|e| e.paths = vec!["/a".into(), "/a".into()]));
        assert!(c.contains(&"unique".to_string()));
    }

    #[test]
    fn good_exact_paths_pass() {
        for p in [
            "/v1.0/me/events",
            "/v1.0/me/sendMail",
            "/v1.0/me/messages/$count",
            "/a",
        ] {
            assert!(with(|e| e.paths = vec![p.into()]).validate().is_ok(), "{p}");
        }
    }

    #[test]
    fn a_write_method_by_prefix_warns_and_by_exact_path_does_not() {
        let by_prefix = with(|e| e.methods = vec!["POST".into()]);
        assert!(by_prefix.validate().is_ok());
        let w = skill_egress_warnings(&by_prefix);
        assert!(w.iter().any(|x| x.contains(WRITE_METHOD_PREFIX_WARNING)));
        let by_path = with(|e| {
            e.methods = vec!["POST".into()];
            e.path_prefixes.clear();
            e.paths = vec!["/v1.0/me/events".into()];
        });
        assert!(!skill_egress_warnings(&by_path)
            .iter()
            .any(|x| x.contains(WRITE_METHOD_PREFIX_WARNING)));
        // A read by prefix, and a read with both fields, stay quiet.
        assert!(skill_egress_warnings(&manifest(egress())).is_empty());
        assert!(skill_egress_warnings(&with(|e| e.paths = vec!["/v1.0/me".into()])).is_empty());
    }

    #[test]
    fn both_fields_round_trip_through_json() {
        let m = with(|e| e.paths = vec!["/v1.0/me/events".into()]);
        let v = crate::encoding::proto_to_json("vam.v1.SkillManifest", &m).unwrap();
        assert!(v.contains("requiresConnector"));
        assert!(v.contains("pathPrefixes"));
        assert!(v.contains("\"paths\""));
        let back: SkillManifest =
            crate::encoding::json_to_proto("vam.v1.SkillManifest", &v).unwrap();
        assert_eq!(back, m);
    }
}
