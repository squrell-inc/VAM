//! The forms a secret can leak in (for `expect: { secret-absent }`), and the skill package
//! archive a `skill` world piece publishes.
//!
//! A hit names the form only. The secret value is never put in a message, a brief or a log:
//! callers get a form name and a place.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::Value;

/// The shortest run of consecutive secret bytes that counts as a leak.
pub const MIN_RUN: usize = 12;

/// Every searchable form of `value`: `(form name, text to look for)`. Raw first, so a raw leak
/// is reported as `raw`. Encoded forms are trimmed to the bytes the neighbours cannot change
/// (the leading chars that mix in what comes before, the trailing chars that mix in what comes
/// after), so a secret inside a larger blob is still found, at each of the three alignments.
pub fn secret_forms(value: &str) -> Vec<(String, String)> {
    let mut forms: Vec<(String, String)> = vec![("raw".into(), value.to_string())];
    for k in 0..3usize {
        let filler = "a".repeat(k);
        let bytes = format!("{filler}{value}");
        // Chars that depend on the filler: 8k bits fill ceil(8k/6) chars.
        let skip = (8 * k).div_ceil(6);
        for (name, enc) in [
            ("base64", STANDARD.encode(&bytes)),
            ("base64url", URL_SAFE.encode(&bytes)),
            ("base64-unpadded", STANDARD_NO_PAD.encode(&bytes)),
            ("base64url-unpadded", URL_SAFE_NO_PAD.encode(&bytes)),
        ] {
            let enc = enc.trim_end_matches('=');
            // The last chars depend on what follows the value.
            let end = enc.len().saturating_sub(2);
            if skip < end {
                forms.push((format!("{name} at offset {k}"), enc[skip..end].to_string()));
            }
        }
    }
    let lower: String = value.bytes().map(|b| format!("{b:02x}")).collect();
    forms.push(("hex (upper)".into(), lower.to_uppercase()));
    forms.push(("hex (lower)".into(), lower));
    let pct = |upper: bool| -> String {
        let mut out = String::new();
        for b in value.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ if upper => out.push_str(&format!("%{b:02X}")),
                _ => out.push_str(&format!("%{b:02x}")),
            }
        }
        out
    };
    forms.push(("percent-encoded (upper)".into(), pct(true)));
    forms.push(("percent-encoded (lower)".into(), pct(false)));
    if let Ok(quoted) = serde_json::to_string(value) {
        if let Some(inner) = quoted.strip_prefix('"').and_then(|q| q.strip_suffix('"')) {
            forms.push(("JSON-escaped".into(), inner.to_string()));
        }
    }
    // Forms that equal an earlier one say nothing new.
    let mut seen = std::collections::BTreeSet::new();
    forms.retain(|(_, text)| !text.is_empty() && seen.insert(text.clone()));
    forms
}

/// The first form of `value` found in `text`, by name: an exact or encoded form, else any run
/// of `MIN_RUN` or more consecutive bytes of the value (a split or partial leak).
pub fn find_form(value: &str, text: &str) -> Option<String> {
    for (name, needle) in secret_forms(value) {
        if text.contains(&needle) {
            return Some(name);
        }
    }
    let bytes = value.as_bytes();
    if bytes.len() >= MIN_RUN {
        for w in bytes.windows(MIN_RUN) {
            // Windows that cut a multi-byte character are not text; skip them.
            if let Ok(run) = std::str::from_utf8(w) {
                if text.contains(run) {
                    return Some(format!("a run of {MIN_RUN} or more consecutive bytes"));
                }
            }
        }
    }
    None
}

/// Every string in `v`, decoded (a JSON string is searched as the text it holds, not as
/// escaped on the wire). A string that is itself JSON (an event payload) is searched whole and
/// also decoded one level.
pub fn strings_of(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            out.push(s.clone());
            let t = s.trim_start();
            if t.starts_with('{') || t.starts_with('[') {
                if let Ok(inner) = serde_json::from_str::<Value>(s) {
                    strings_of(&inner, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| strings_of(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings_of(x, out)),
        _ => {}
    }
}

/// Pass the manifest fields `requiresConnector` and per-tool `egress` to vamd. Flip to `true`
/// when vamd admits them in a skill manifest (until then a manifest that carries them is
/// refused at publish, so the runner leaves them out and the scenarios run to their first
/// expectation instead).
pub const PASS_SKILL_CONNECTOR_FIELDS: bool = true;

/// The manifest as published: without the connector fields while vamd does not admit them.
fn published_manifest(manifest: &Value) -> Value {
    let mut m = manifest.clone();
    if !PASS_SKILL_CONNECTOR_FIELDS {
        if let Some(o) = m.as_object_mut() {
            o.remove("requiresConnector");
        }
        for t in m
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(o) = t.as_object_mut() {
                o.remove("egress");
            }
        }
    }
    m
}

/// The package a `skill` piece publishes: `SKILLS.md` and `manifest.json` at the archive root,
/// as a tar.gz, then `files` (path, bytes) at their own paths. `manifest` is the piece's
/// `body.spec` with its placeholders replaced.
pub fn skill_archive(
    manifest: &Value,
    urn: &str,
    name: &str,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let manifest = &published_manifest(manifest);
    let manifest_text = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    let description = manifest
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("Fixture skill for end-to-end scenarios.");
    let skills_md = format!(
        "---\nid: \"{urn}\"\nname: \"{name}\"\nversion: \"1.0.0\"\ndescription: \"{description}\"\n---\n\n# {name}\n\n{description}\n"
    );
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for (path, bytes) in [
        ("SKILLS.md", skills_md.as_bytes()),
        ("manifest.json", manifest_text.as_slice()),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, path, bytes)
            .map_err(|e| e.to_string())?;
    }
    for (path, bytes) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o755);
        h.set_cksum();
        tar.append_data(&mut h, path, bytes.as_slice())
            .map_err(|e| e.to_string())?;
    }
    let gz = tar.into_inner().map_err(|e| e.to_string())?;
    gz.finish().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    const V: &str = "tok-0123456789abcdef01234567/+=&\"z";

    #[test]
    fn the_raw_form_is_found_first_and_named() {
        assert_eq!(find_form(V, &format!("x {V} y")).as_deref(), Some("raw"));
        assert_eq!(find_form(V, "nothing here"), None);
    }

    #[test]
    fn each_encoded_form_is_found() {
        for (want, text) in [
            ("base64 at offset 0", STANDARD.encode(V)),
            ("base64url-unpadded at offset 0", URL_SAFE_NO_PAD.encode(V)),
            (
                "hex (lower)",
                V.bytes().map(|b| format!("{b:02x}")).collect(),
            ),
            (
                "hex (upper)",
                V.bytes().map(|b| format!("{b:02X}")).collect(),
            ),
            (
                "percent-encoded (upper)",
                "tok-0123456789abcdef01234567%2F%2B%3D%26%22z".into(),
            ),
            (
                "JSON-escaped",
                "tok-0123456789abcdef01234567/+=&\\\"z".into(),
            ),
        ] {
            let found = find_form(V, &format!("pre {text} post"));
            assert!(found.is_some(), "{want} not found");
        }
    }

    #[test]
    fn a_secret_inside_a_larger_base64_blob_is_found_at_every_alignment() {
        for prefix in ["", "u:", "user:", "userr:"] {
            let blob = STANDARD.encode(format!("{prefix}{V}tail"));
            assert!(find_form(V, &blob).is_some(), "prefix {prefix:?}");
        }
    }

    #[test]
    fn a_split_secret_is_found_by_its_run() {
        let (a, b) = V.split_at(V.len() / 2);
        let found = find_form(V, &format!("{a}\n{b}")).expect("a half is 12+ bytes");
        assert!(found.contains("run of"), "{found}");
        // Under the minimum run, nothing is found.
        assert_eq!(find_form(V, &V[..MIN_RUN - 1]), None);
    }

    #[test]
    fn a_hit_never_carries_the_value() {
        let found = find_form(V, &STANDARD.encode(V)).unwrap();
        assert!(!found.contains(V) && !found.contains("0123456789"));
    }

    #[test]
    fn strings_are_searched_as_the_text_they_hold() {
        let body: Value = serde_json::json!({
            "records": [{ "message": { "parts": [{ "text": V }] } }],
            "payload": serde_json::json!({ "x": V }).to_string()
        });
        let mut out = Vec::new();
        strings_of(&body, &mut out);
        assert!(out.iter().any(|s| s == V));
        assert_eq!(out.iter().filter(|s| s.as_str() == V).count(), 2);
    }

    #[test]
    fn extra_files_are_packed_and_connector_fields_follow_the_switch() {
        let m = serde_json::json!({
            "requiresConnector": "urn:x",
            "tools": [{ "name": "t", "egress": [{ "host": "h" }] }]
        });
        let files = vec![("probe.py".to_string(), b"print(1)".to_vec())];
        let bytes = skill_archive(&m, "urn:vam:skill:local.vam.dev/e2e/x", "x", &files).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
        let mut seen = Vec::new();
        for e in ar.entries().unwrap() {
            let mut e = e.unwrap();
            let mut text = String::new();
            e.read_to_string(&mut text).unwrap();
            seen.push((e.path().unwrap().to_string_lossy().to_string(), text));
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[2], ("probe.py".to_string(), "print(1)".to_string()));
        assert_eq!(
            seen[1].1.contains("requiresConnector"),
            PASS_SKILL_CONNECTOR_FIELDS
        );
        assert_eq!(seen[1].1.contains("egress"), PASS_SKILL_CONNECTOR_FIELDS);
    }

    #[test]
    fn the_skill_archive_holds_both_files_at_the_root() {
        let m = serde_json::json!({"resourceId": {"urn": "urn:vam:skill:local.vam.dev/e2e/x"}});
        let bytes = skill_archive(&m, "urn:vam:skill:local.vam.dev/e2e/x", "x", &[]).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
        let mut seen = Vec::new();
        for e in ar.entries().unwrap() {
            let mut e = e.unwrap();
            let mut text = String::new();
            e.read_to_string(&mut text).unwrap();
            seen.push((e.path().unwrap().to_string_lossy().to_string(), text));
        }
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, "SKILLS.md");
        assert!(seen[0].1.contains("name: \"x\""));
        assert_eq!(seen[1].0, "manifest.json");
        assert!(seen[1].1.contains("local.vam.dev/e2e/x"));
    }
}
