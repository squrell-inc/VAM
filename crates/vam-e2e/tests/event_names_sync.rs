//! Keeps `KNOWN_EVENTS` in step with vamd without depending on vam-orchestrator: the sources
//! are read as text.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use vam_e2e::events::{KNOWN_EVENTS, PREFIX};

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn literals() -> BTreeSet<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../vam-orchestrator/src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(!files.is_empty(), "no sources under {}", src.display());
    let needle = format!("\"{PREFIX}");
    let mut found = BTreeSet::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        let mut rest = text.as_str();
        while let Some(i) = rest.find(&needle) {
            let tail = &rest[i + needle.len()..];
            let end = tail
                .find(|c: char| !(c.is_ascii_lowercase() || c == '_' || c == '.'))
                .unwrap_or(tail.len());
            if tail[end..].starts_with('"') {
                found.insert(tail[..end].to_string());
            }
            rest = tail;
        }
    }
    found
}

#[test]
fn every_known_event_is_a_vamd_event_type() {
    let found = literals();
    let missing: Vec<_> = KNOWN_EVENTS
        .iter()
        .filter(|k| !found.contains(**k))
        .collect();
    assert!(
        missing.is_empty(),
        "not in vam-orchestrator/src: {missing:?}"
    );
}

#[test]
fn known_events_have_no_duplicates() {
    let set: BTreeSet<_> = KNOWN_EVENTS.iter().collect();
    assert_eq!(set.len(), KNOWN_EVENTS.len());
}
