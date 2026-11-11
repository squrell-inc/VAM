//! `vam receipt show` rendering and the offline `vam receipt verify`.

use std::path::{Path, PathBuf};
use std::process::Command;

use ed25519_dalek::SigningKey;
use vam_security::receipt::{appendix_digest, sign_receipt, ReceiptSigner};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn signed_document(seed: u8) -> (String, ReceiptSigner) {
    let signer = ReceiptSigner::new(SigningKey::from_bytes(&[seed; 32]));
    let appendix = serde_json::json!({"label": "written by the model", "finalReply": "hello"});
    let receipt = serde_json::json!({
        "version": vam_spec::receipt::RECEIPT_VERSION,
        "runId": "urn:vam:run:local.vam.dev/acme/r",
        "state": "SUCCEEDED",
        "modelAppendixDigest": appendix_digest(&appendix).unwrap(),
    });
    let signature = sign_receipt(&signer, &receipt).unwrap();
    let doc = serde_json::json!({
        "receipt": receipt, "modelAppendix": appendix, "signature": signature,
    });
    (doc.to_string(), signer)
}

/// The built `vam`, with no host configured: verify must not need one.
fn vam(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_vam"))
        .args(args)
        .current_dir(cwd)
        .env_remove("VAM_HOST")
        .env_remove("VAM_API_KEY")
        .env("HOME", cwd)
        .env("XDG_CONFIG_HOME", cwd)
        .output()
        .expect("run vam")
}

fn combined(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn markdown_matches_the_golden_file() {
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("receipt_full.json")).unwrap())
            .unwrap();
    let md = vam_cli::cmd::receipt::render_markdown(&doc);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(fixture("receipt_full.md"), &md).unwrap();
    }
    let golden = std::fs::read_to_string(fixture("receipt_full.md")).unwrap();
    assert_eq!(md, golden, "---- rendered ----\n{md}");
}

#[test]
fn the_journaled_effects_section_matches_its_golden_file() {
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("receipt_effects.json")).unwrap())
            .unwrap();
    let md = vam_cli::cmd::receipt::render_markdown(&doc);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(fixture("receipt_effects.md"), &md).unwrap();
    }
    let golden = std::fs::read_to_string(fixture("receipt_effects.md")).unwrap();
    assert_eq!(md, golden, "---- rendered ----\n{md}");
    assert!(!md.contains("effects not journaled"));
    assert!(md.contains("- **Duplicates:** 0"));
    assert!(md.contains("- **Unknown:** 1"));
}

#[test]
fn the_renderer_adds_no_facts_and_keeps_the_tiers_apart() {
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("receipt_full.json")).unwrap())
            .unwrap();
    let md = vam_cli::cmd::receipt::render_markdown(&doc);
    // Isolation is printed as stored; effects are the stored statement, never a number.
    assert!(md.contains("process guest, no microVM"));
    assert!(md.contains("effects not journaled on this version"));
    // Model text appears only after the appendix heading.
    let split = md.find("## Model appendix").expect("appendix section");
    assert!(md[..split].find("Closed the issue.").is_none());
    assert!(md[..split].find("Asked the user.").is_none());
    assert!(md[split..].contains("Written by the model, not verified by the host"));
    assert!(md[split..].contains("Closed the issue."));
    // The gate reference is shown on the allowed and the denied line.
    assert_eq!(md.matches("gate step confirm").count(), 2);

    // Nothing is invented for a minimal document.
    let minimal = serde_json::json!({"receipt": {"runId": "r1"}});
    let md = vam_cli::cmd::receipt::render_markdown(&minimal);
    assert!(md.contains("**Run:** r1"));
    for absent in [
        "## Pins",
        "## Isolation",
        "## Tool calls",
        "## Procedure",
        "## Effects",
        "## Model appendix",
    ] {
        assert!(!md.contains(absent), "{absent} in:\n{md}");
    }
}

#[test]
fn the_harness_line_names_the_profile_and_only_the_overrides() {
    let render = |harness: serde_json::Value| {
        vam_cli::cmd::receipt::render_markdown(
            &serde_json::json!({"receipt": {"runId": "r1", "harness": harness}}),
        )
    };
    let md = render(serde_json::json!({
        "profile": "base", "version": "0.1.0", "digest": "sha256:aa",
        "overrides": {"loop": {"detect": true}}
    }));
    assert!(
        md.contains("**Harness:** base v0.1.0 (overrides: loop.detect=true)"),
        "{md}"
    );
    assert!(md.contains("**Harness digest:** sha256:aa"));
    let md = render(serde_json::json!({
        "profile": "base", "version": "0.1.0", "digest": "sha256:aa"
    }));
    assert!(md.contains("**Harness:** base v0.1.0\n"), "{md}");
    assert!(!md.contains("overrides"));
    // A receipt without the field prints no harness section.
    let none =
        vam_cli::cmd::receipt::render_markdown(&serde_json::json!({"receipt": {"runId": "r1"}}));
    assert!(!none.contains("Harness"));
}

#[test]
fn verify_passes_offline_and_says_the_key_is_not_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let (doc, signer) = signed_document(7);
    let file = dir.path().join("r.json");
    std::fs::write(&file, doc).unwrap();
    let out = vam(&["receipt", "verify", file.to_str().unwrap()], dir.path());
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains(signer.key_id()), "{text}");
    assert!(text.contains("key not pinned"), "{text}");
}

#[test]
fn verify_fails_on_a_changed_byte_and_names_the_signature() {
    let dir = tempfile::tempdir().unwrap();
    let (doc, _) = signed_document(7);
    let file = dir.path().join("r.json");
    std::fs::write(&file, doc.replacen("SUCCEEDED", "SUCCEEDEE", 1)).unwrap();
    let out = vam(&["receipt", "verify", file.to_str().unwrap()], dir.path());
    assert_eq!(out.status.code(), Some(9), "{}", combined(&out));
    assert!(combined(&out).to_lowercase().contains("signature"));
}

#[test]
fn trusted_keys_pin_the_signer() {
    let dir = tempfile::tempdir().unwrap();
    let (doc, signer) = signed_document(7);
    let file = dir.path().join("r.json");
    std::fs::write(&file, doc).unwrap();
    let keys = |signer: &ReceiptSigner| {
        serde_json::json!({"signingKeys": [{
            "keyId": signer.key_id(), "publicKey": signer.public_key_hex(), "state": "current"
        }]})
        .to_string()
    };
    let good = dir.path().join("good.json");
    std::fs::write(&good, keys(&signer)).unwrap();
    let out = vam(
        &[
            "receipt",
            "verify",
            file.to_str().unwrap(),
            "--trusted-keys",
            good.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert!(out.status.success(), "{}", combined(&out));
    assert!(!combined(&out).contains("not pinned"));

    let other = ReceiptSigner::new(SigningKey::from_bytes(&[8u8; 32]));
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, keys(&other)).unwrap();
    let out = vam(
        &[
            "receipt",
            "verify",
            file.to_str().unwrap(),
            "--trusted-keys",
            bad.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_eq!(out.status.code(), Some(9), "{}", combined(&out));
    assert!(combined(&out).contains(signer.key_id()));
}

#[test]
fn verify_of_a_missing_or_malformed_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let out = vam(&["receipt", "verify", "nope.json"], dir.path());
    assert_eq!(out.status.code(), Some(2), "{}", combined(&out));
    let file = dir.path().join("junk.json");
    std::fs::write(&file, "not json").unwrap();
    let out = vam(&["receipt", "verify", file.to_str().unwrap()], dir.path());
    assert_eq!(out.status.code(), Some(9), "{}", combined(&out));
}

#[test]
fn a_triggered_run_names_its_trigger_and_a_person_run_does_not() {
    let doc = serde_json::json!({"receipt": {
        "runId": "r1", "requestedBy": "u1", "requestSurface": "echo",
        "requestTrigger": {"brokerId": "b1", "grantId": "rg_1", "triggerRef": "daily",
                           "fireAtMs": 1_700_000_000_000_i64, "fireKind": "scheduled"}}});
    let md = vam_cli::cmd::receipt::render_markdown(&doc);
    assert!(md.contains(
        "- **Requested by:** u1, surface echo — daily via grant rg_1 (b1), fire time 2023-11-14T22:13:20Z\n"
    ));
    assert!(!md.contains("Request surface"));
    let person = serde_json::json!({"receipt": {"runId": "r1", "requestedBy": "u1", "requestSurface": "cli"}});
    let md = vam_cli::cmd::receipt::render_markdown(&person);
    assert!(md.contains("- **Request surface:** cli\n"));
    let partial = serde_json::json!({"receipt": {"requestedBy": "u1", "requestTrigger": {"grantId": "rg_1"}}});
    assert!(vam_cli::cmd::receipt::render_markdown(&partial).contains("u1 — via grant rg_1\n"));
}
