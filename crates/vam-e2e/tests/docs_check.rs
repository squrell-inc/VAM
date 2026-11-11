//! `vam-e2e check --docs`: the docs lint, driven through the built CLI on a fixture tree.
//! A lint of the repository is not a catalog scenario (every scenario starts a `vamd`), so it is
//! proved here at the contract layer.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

const SCENARIO: &str = "id: chat-replies
title: A chat gets a reply
tags: [chat]
needs: process
given: [org-admin, agent-plain]
when:
  - chat: \"e2e-step:hello Hi\"
then:
  - expect: { run: succeeded }
mock:
  - when: { step: hello }
    reply: { text: Hi }
status: active
";
const ORG: &str = "piece: org-admin\nkind: org\n";
const AGENT: &str = "piece: agent-plain\nkind: agent\nbody:\n  spec:\n    resourceId: { urn: \"urn:vam:agent:x/e2e/{{name}}\" }\n";

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

/// A valid one-scenario catalog under `<fx>/e2e` and one docs page under `<fx>/docs`.
fn fixture(page: &str) -> TempDir {
    let fx = TempDir::new().unwrap();
    write(fx.path(), "e2e/scenarios/chat-replies.yaml", SCENARIO);
    write(fx.path(), "e2e/world/org-admin.yaml", ORG);
    write(fx.path(), "e2e/world/agent-plain.yaml", AGENT);
    write(fx.path(), "docs/security.md", page);
    fx
}

// The claim word list is `e2e/docs-claims.yaml`, built into the binary; `<root>/docs-claims.yaml`
// overrides it.
#[test]
fn docs_claim_without_proof_fails_check() {
    let fx = fixture("---\ntitle: Security\nstatus: shipped\n---\n\nThe VMM runs jailed.\n");
    let out = Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
        .arg("check")
        .arg("--root")
        .arg(fx.path().join("e2e"))
        .arg("--docs")
        .arg(fx.path().join("docs"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a shipped page with an isolation claim and no proof must fail check\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("docs-proof") && stdout.contains("security.md"),
        "want rule docs-proof naming the page, got: {stdout}"
    );
}

fn run(fx: &TempDir) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
        .arg("check")
        .arg("--root")
        .arg(fx.path().join("e2e"))
        .arg("--docs")
        .arg(fx.path().join("docs"))
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn expect_fail(page: &str, rule: &str) {
    let (code, out) = run(&fixture(page));
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!(": {rule}: ")),
        "want {rule}, got {out}"
    );
}

#[test]
fn docs_claim_with_an_active_proof_passes_check() {
    let fx = fixture(
        "---\ntitle: Security\nstatus: shipped\nproves: [chat-replies]\n---\n\nThe VMM runs jailed.\n",
    );
    let (code, out) = run(&fx);
    assert_eq!(code, Some(0), "{out}");
}

#[test]
fn docs_claim_words_in_code_fences_and_comments_do_not_count() {
    let fx = fixture(
        "---\ntitle: T\nstatus: shipped\n---\n\nPlain text.\n\n```\njail\n```\n\n<!-- sandbox -->\n",
    );
    assert_eq!(run(&fx).0, Some(0));
}

#[test]
fn docs_status_missing_or_unknown_fails() {
    expect_fail("---\ntitle: T\n---\n\nHi.\n", "docs-status");
    expect_fail("---\ntitle: T\nstatus: shiped\n---\n\nHi.\n", "docs-status");
}

#[test]
fn docs_proof_ref_must_exist() {
    expect_fail(
        "---\ntitle: T\nstatus: shipped\nproves: [no-such-scenario]\n---\n\nHi.\n",
        "docs-proof-ref",
    );
    expect_fail(
        "---\ntitle: T\nstatus: shipped\nproves: [crates/nope/tests/x.rs]\n---\n\nHi.\n",
        "docs-proof-ref",
    );
    expect_fail(
        "---\ntitle: T\nstatus: shipped\nproves: [\"crates/vam-e2e/tests/docs_check.rs::no_such_fn\"]\n---\n\nHi.\n",
        "docs-proof-ref",
    );
}

#[test]
fn docs_pending_and_retired_scenarios_are_not_proofs() {
    for (status, page_status, ok) in [
        ("pending", "shipped", false),
        ("pending", "stale", false),
        ("pending", "planned", true),
        ("retired", "planned", false),
    ] {
        let fx = fixture(&format!(
            "---\ntitle: T\nstatus: {page_status}\nproves: [chat-replies]\n---\n\nHi.\n"
        ));
        write(
            fx.path(),
            "e2e/scenarios/chat-replies.yaml",
            &SCENARIO.replace("status: active", &format!("status: {status}")),
        );
        let (code, out) = run(&fx);
        // Only the docs rule is asserted: the fixture catalog may itself need more for `check`.
        assert_eq!(
            out.contains(": docs-proof-ref: "),
            !ok,
            "{status} on {page_status}: {out} (exit {code:?})"
        );
    }
}

#[test]
fn docs_a_real_test_path_and_fn_is_a_proof() {
    let fx = fixture(
        "---\ntitle: T\nstatus: shipped\nproves: [\"crates/x/tests/y.rs::my_fn\"]\n---\n\nThe VMM runs jailed.\n",
    );
    write(fx.path(), "crates/x/tests/y.rs", "#[test]\nfn my_fn() {}\n");
    assert_eq!(run(&fx).0, Some(0));
}

#[test]
fn docs_hand_written_status_note_fails() {
    expect_fail(
        "---\ntitle: T\nstatus: planned\n---\n\n:::note[Status: Implemented]\nx\n:::\n",
        "docs-admonition",
    );
}

#[test]
fn docs_baseline_exempts_and_only_shrinks() {
    // A listed failing page is exempt.
    let fx = fixture("---\ntitle: T\n---\n\nHi.\n");
    write(fx.path(), "e2e/docs-baseline.txt", "# c\nsecurity.md\n");
    assert_eq!(run(&fx).0, Some(0));
    // A listed page that now passes is an error.
    let fx = fixture("---\ntitle: T\nstatus: planned\n---\n\nHi.\n");
    write(fx.path(), "e2e/docs-baseline.txt", "security.md\n");
    let (code, out) = run(&fx);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains(": docs-baseline: "), "{out}");
    // A listed page that does not exist is an error; an unlisted failing page is not exempt.
    let fx = fixture("---\ntitle: T\n---\n\nHi.\n");
    write(fx.path(), "e2e/docs-baseline.txt", "gone.md\n");
    let (code, out) = run(&fx);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains("docs-baseline") && out.contains("docs-status"),
        "{out}"
    );
}

#[test]
fn docs_readme_keys_live_in_a_leading_comment() {
    let fx = fixture("---\ntitle: T\nstatus: planned\n---\n\nHi.\n");
    let readme = fx.path().join("README.md");
    let run_readme = |fx: &TempDir| {
        let out = Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
            .arg("check")
            .arg("--root")
            .arg(fx.path().join("e2e"))
            .arg("--docs")
            .arg(fx.path().join("docs"))
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    // The README default is `<root>/../README.md`, so no --readme is needed.
    write(
        fx.path(),
        "README.md",
        "<!-- docs:\nstatus: shipped\nproves: [chat-replies]\n-->\n# VAM\n\nJailed.\n",
    );
    assert_eq!(run_readme(&fx).0, Some(0));
    fs::write(&readme, "# VAM\n\nJailed.\n").unwrap();
    let (code, out) = run_readme(&fx);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("README.md: docs-status"), "{out}");
}

#[test]
fn docs_explicit_path_that_is_absent_exits_2() {
    let fx = fixture("---\ntitle: T\nstatus: planned\n---\n");
    let out = Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
        .arg("check")
        .arg("--root")
        .arg(fx.path().join("e2e"))
        .arg("--docs")
        .arg(fx.path().join("nope"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// The repository's own docs: every page passes or is in the baseline, and the baseline only
/// shrinks. A code PR that retires a scenario or renames a cited test fails here.
#[test]
fn repo_docs_are_honest() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = repo.join("e2e");
    let errs = vam_e2e::docs::check_docs(
        &root,
        &repo.join("docs/src/content/docs"),
        Some(&repo.join("README.md")),
    );
    assert!(errs.is_empty(), "{errs:#?}");
    assert!(
        vam_e2e::docs::baseline_len(&root) <= 208,
        "the baseline may not grow"
    );
}

const SHIPPED: &str = "---\ntitle: Security\nstatus: shipped\n---\n\nPlain text.\n";

fn with_register(page: &str, yaml: &str) -> TempDir {
    let fx = fixture(page);
    write(fx.path(), "e2e/docs-features.yaml", yaml);
    fx
}

fn expect_register_fail(page: &str, yaml: &str, want: &str) {
    let (code, out) = run(&with_register(page, yaml));
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(": docs-feature: ") && out.contains(want),
        "want docs-feature `{want}`, got {out}"
    );
}

#[test]
fn docs_feature_row_with_pages_and_a_proof_passes_and_is_counted() {
    let fx = with_register(
        "---\ntitle: Security\nstatus: shipped\nproves: [chat-replies]\n---\n\nPlain.\n",
        "features:\n  - id: f\n    area: A\n    surface: s\n    dev: [security]\n    arch: [security]\n    proves: [chat-replies]\n  - id: g\n    area: A\n    surface: s\n    dev: []\n    arch: [security]\n    unproven: no test yet\n    pending_page: true\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_vam-e2e"))
        .args(["check", "--root"])
        .arg(fx.path().join("e2e"))
        .arg("--docs")
        .arg(fx.path().join("docs"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("features: 2, proven 1, with a developer page 1"),
        "{stdout}"
    );
}

#[test]
fn docs_feature_page_must_exist_unless_pending() {
    expect_register_fail(
        SHIPPED,
        "features:\n  - id: f\n    area: A\n    surface: s\n    dev: [nope]\n    arch: [security]\n    unproven: x\n",
        "page `nope` does not exist",
    );
}

#[test]
fn docs_feature_needs_proves_or_a_reason() {
    expect_register_fail(
        SHIPPED,
        "features:\n  - id: f\n    area: A\n    surface: s\n    dev: [security]\n    arch: [security]\n",
        "needs `proves` or `unproven",
    );
}

#[test]
fn docs_feature_proof_must_resolve() {
    expect_register_fail(
        SHIPPED,
        "features:\n  - id: f\n    area: A\n    surface: s\n    dev: [security]\n    arch: [security]\n    proves: [no-such-scenario]\n",
        "no scenario `no-such-scenario`",
    );
}

#[test]
fn docs_feature_page_out_of_baseline_must_hold_the_proof() {
    expect_register_fail(
        SHIPPED,
        "features:\n  - id: f\n    area: A\n    surface: s\n    dev: [security]\n    arch: [security]\n    proves: [chat-replies]\n",
        "names none of the feature's proofs",
    );
}

#[test]
fn docs_feature_stale_pending_flag_fails() {
    expect_register_fail(
        SHIPPED,
        "features:\n  - id: f\n    area: A\n    surface: s\n    dev: [security]\n    arch: [security]\n    unproven: x\n    pending_page: true\n",
        "`pending_page` is set but every page exists",
    );
}

fn with_redirects(json: &str, extra_page: Option<(&str, &str)>) -> (Option<i32>, String) {
    let fx = fixture(SHIPPED);
    write(fx.path(), "docs/redirects.json", json);
    if let Some((rel, text)) = extra_page {
        write(fx.path(), rel, text);
    }
    run(&fx)
}

fn expect_redirect_fail(json: &str, extra_page: Option<(&str, &str)>, want: &str) {
    let (code, out) = with_redirects(json, extra_page);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(": docs-redirect: ") && out.contains(want),
        "want docs-redirect `{want}`, got {out}"
    );
}

#[test]
fn docs_redirect_to_an_existing_page_passes() {
    let (code, out) = with_redirects(
        r#"{ "/old/": { "to": "/security/", "kind": "retired" } }"#,
        None,
    );
    assert_eq!(code, Some(0), "{out}");
}

#[test]
fn docs_redirect_target_must_exist() {
    expect_redirect_fail(
        r#"{ "/old/": { "to": "/missing/", "kind": "retired" } }"#,
        None,
        "target `/missing/` is not a page",
    );
}

#[test]
fn docs_redirect_old_page_must_be_gone() {
    expect_redirect_fail(
        r#"{ "/security/": { "to": "/other/", "kind": "retired" } }"#,
        Some(("docs/other.md", SHIPPED)),
        "its page still exists",
    );
}

#[test]
fn docs_redirect_kind_must_be_known() {
    expect_redirect_fail(
        r#"{ "/old/": { "to": "/security/", "kind": "gone" } }"#,
        None,
        "is not `retired` or `design`",
    );
}

#[test]
fn docs_redirect_no_page_may_link_to_the_old_url() {
    expect_redirect_fail(
        r#"{ "/old/": { "to": "/security/", "kind": "retired" } }"#,
        Some((
            "docs/other.md",
            "---\ntitle: O\nstatus: shipped\n---\n\nSee [old](/old/).\n",
        )),
        "links to `/old/`",
    );
}

#[test]
fn docs_a_bare_source_file_is_not_a_proof() {
    let fx = fixture(
        "---\ntitle: Security\nstatus: shipped\nproves: [crates/x/src/y.rs]\n---\n\nPlain.\n",
    );
    write(fx.path(), "crates/x/src/y.rs", "#[test]\nfn my_fn() {}\n");
    let (code, out) = run(&fx);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains("docs-proof-ref") && out.contains("is not a test file"),
        "{out}"
    );
}

#[test]
fn docs_a_fn_without_a_test_attribute_is_not_a_proof() {
    let fx = fixture(
        "---\ntitle: Security\nstatus: shipped\nproves: [crates/x/src/y.rs::helper]\n---\n\nPlain.\n",
    );
    write(
        fx.path(),
        "crates/x/src/y.rs",
        "fn helper() {}\n#[test]\nfn real() {}\n",
    );
    let (code, out) = run(&fx);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains("docs-proof-ref") && out.contains("has no test `fn helper`"),
        "{out}"
    );
}

#[test]
fn docs_a_tokio_test_fn_in_src_is_a_proof() {
    let fx = fixture(
        "---\ntitle: Security\nstatus: shipped\nproves: [crates/x/src/y.rs::real]\n---\n\nPlain.\n",
    );
    write(
        fx.path(),
        "crates/x/src/y.rs",
        "#[tokio::test(flavor = \"multi_thread\")]\nasync fn real() {}\n",
    );
    let (code, out) = run(&fx);
    assert_eq!(code, Some(0), "{out}");
}
