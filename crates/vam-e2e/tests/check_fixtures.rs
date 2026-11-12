//! One failing fixture per `vam-e2e check` rule, built from a small valid catalog.

use std::fs;
use std::path::{Path, PathBuf};
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

struct Fx {
    dir: TempDir,
}

impl Fx {
    fn new() -> Fx {
        let fx = Fx {
            dir: TempDir::new().unwrap(),
        };
        fs::create_dir_all(fx.root().join("scenarios")).unwrap();
        fs::create_dir_all(fx.root().join("world")).unwrap();
        fx.write("scenarios/chat-replies.yaml", SCENARIO);
        fx.write("world/org-admin.yaml", ORG);
        fx.write("world/agent-plain.yaml", AGENT);
        fx
    }
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.root().join(rel);
        fs::write(&p, text).unwrap();
        p
    }
    /// Replaces `from` with `to` in the base scenario.
    fn edit(&self, from: &str, to: &str) {
        assert!(SCENARIO.contains(from), "{from} not in base");
        self.write("scenarios/chat-replies.yaml", &SCENARIO.replace(from, to));
    }
    fn errors(&self) -> Vec<String> {
        vam_e2e::check::check(self.root())
            .iter()
            .map(|e| e.to_string())
            .collect()
    }
    fn expect_error(&self, rule: &str, needle: &str) {
        let errs = self.errors();
        assert!(
            errs.iter()
                .any(|e| e.contains(&format!(": {rule}: ")) && e.contains(needle)),
            "want {rule}/{needle}, got {errs:#?}"
        );
    }
}

#[test]
fn base_catalog_is_clean() {
    assert_eq!(Fx::new().errors(), Vec::<String>::new());
}

#[test]
fn repo_catalog_is_clean() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e");
    let errs = vam_e2e::check::check(&root);
    assert!(errs.is_empty(), "{errs:#?}");
    let cat = vam_e2e::catalog::Catalog::load(&root);
    // The catalog grows with each phase: at least the Phase 1 set must load.
    assert!(cat.scenarios.len() >= 5 && cat.pieces.len() >= 6);
}

#[test]
fn unknown_field_is_a_schema_error() {
    let f = Fx::new();
    f.edit("status: active", "status: active\nbogus: 1");
    f.expect_error("schema", "bogus");
}

#[test]
fn bad_status_is_a_schema_error() {
    let f = Fx::new();
    f.edit("status: active", "status: someday");
    f.expect_error("schema", "someday");
}

#[test]
fn plan_numbered_id_and_file() {
    let f = Fx::new();
    fs::remove_file(f.root().join("scenarios/chat-replies.yaml")).unwrap();
    f.write(
        "scenarios/p4-check7.yaml",
        &SCENARIO.replace("chat-replies", "p4-check7"),
    );
    f.expect_error("plan-number", "id `p4-check7`");
    f.expect_error("plan-number", "file `p4-check7`");
}

#[test]
fn plan_numbered_piece() {
    let f = Fx::new();
    f.write(
        "world/agent-wave3.yaml",
        &AGENT.replace("agent-plain", "agent-wave3"),
    );
    f.expect_error("plan-number", "piece `agent-wave3`");
}

#[test]
fn plan_numbered_label() {
    let f = Fx::new();
    f.edit("e2e-step:hello", "e2e-step:h1-hello");
    f.edit("step: hello", "step: h1-hello");
    f.expect_error("plan-number", "label `h1-hello`");
}

#[test]
fn id_must_equal_file_stem() {
    let f = Fx::new();
    f.edit("id: chat-replies", "id: other-name");
    f.expect_error("id-file", "other-name");
}

#[test]
fn duplicate_ids() {
    let f = Fx::new();
    f.write("scenarios/copy.yaml", SCENARIO);
    f.expect_error("duplicate", "chat-replies");
}

#[test]
fn unknown_tag() {
    let f = Fx::new();
    f.edit("tags: [chat]", "tags: [chat, phase2]");
    f.expect_error("tags", "unknown tag `phase2`");
}

#[test]
fn pending_needs_expect_fail() {
    let f = Fx::new();
    f.edit("status: active", "status: pending");
    f.expect_error("expect-fail", "pending scenario needs");
}

#[test]
fn expect_fail_step_must_exist() {
    let f = Fx::new();
    f.edit(
        "status: active",
        "status: pending\nexpectFail: { step: when.5, class: env }",
    );
    f.expect_error("expect-fail", "when.5");
}

#[test]
fn active_must_not_have_expect_fail() {
    let f = Fx::new();
    f.edit(
        "status: active",
        "status: active\nexpectFail: { step: when.0, class: env }",
    );
    f.expect_error("expect-fail", "only a pending");
}

#[test]
fn given_piece_must_exist() {
    let f = Fx::new();
    f.edit("agent-plain]", "agent-plain, ghost-piece]");
    f.expect_error("given", "ghost-piece");
}

#[test]
fn step_alias_must_be_given() {
    let f = Fx::new();
    f.edit(
        "- chat: \"e2e-step:hello Hi\"",
        "- chat: { agent: nobody, text: \"e2e-step:hello Hi\" }",
    );
    f.expect_error("step-alias", "nobody");
}

#[test]
fn expect_needs_one_family() {
    let f = Fx::new();
    f.edit("{ run: succeeded }", "{ run: succeeded, event: x }");
    f.expect_error("expect", "exactly one");
}

#[test]
fn mock_with_user_turn_is_rejected() {
    let f = Fx::new();
    f.edit("when: { step: hello }", "when: { userTurn: 1 }");
    f.expect_error("mock", "userTurn");
}

#[test]
fn mock_unknown_condition_is_rejected() {
    let f = Fx::new();
    f.edit(
        "when: { step: hello }",
        "when: { step: hello, whenever: 1 }",
    );
    f.expect_error("mock", "whenever");
}

#[test]
fn process_scenario_needs_mock() {
    let f = Fx::new();
    f.edit(
        "mock:\n  - when: { step: hello }\n    reply: { text: Hi }\n",
        "",
    );
    f.expect_error("mock", "needs a `mock:` block");
}

#[test]
fn chat_label_needs_a_rule() {
    let f = Fx::new();
    f.edit("when: { step: hello }", "when: { step: other }");
    f.expect_error("mock-label", "step label `hello` has no mock rule");
    f.expect_error("mock-label", "`other` is never sent");
}

#[test]
fn chat_needs_a_label() {
    let f = Fx::new();
    f.edit("e2e-step:hello Hi", "Hi");
    f.expect_error("label", "needs an `e2e-step");
}

#[test]
fn piece_urn_needs_placeholder() {
    let f = Fx::new();
    f.write(
        "world/agent-plain.yaml",
        &AGENT.replace("{{name}}", "fixed"),
    );
    f.expect_error("piece-body", "{{name}}");
}

#[test]
fn a_fixed_agent_urn_needs_the_fixed_urn_flag_and_must_be_unique() {
    let f = Fx::new();
    let fixed = AGENT.replace("{{name}}", "fixed");
    f.write(
        "world/agent-plain.yaml",
        &format!("{fixed}fixedUrn: true\n"),
    );
    assert_eq!(f.errors(), Vec::<String>::new());
    f.write(
        "world/agent-other.yaml",
        &format!(
            "{}fixedUrn: true\n",
            fixed.replace("piece: agent-plain", "piece: agent-other")
        ),
    );
    f.expect_error("piece-body", "used by another piece");
}

#[test]
fn a_workspace_seed_names_its_fixture_workspace_and_thread() {
    let f = Fx::new();
    f.write(
        "world/seed-plain.yaml",
        "piece: seed-plain\nkind: workspace-seed\nfixture: x\n",
    );
    f.expect_error("piece-body", "workspaceId");
}

#[test]
fn piece_file_stem_must_match() {
    let f = Fx::new();
    f.write(
        "world/agent-plain.yaml",
        &AGENT.replace("piece: agent-plain", "piece: agent-other"),
    );
    f.expect_error("id-file", "agent-other");
}

#[test]
fn piece_requires_must_exist() {
    let f = Fx::new();
    f.write(
        "world/org-admin.yaml",
        "piece: org-admin\nkind: org\nrequires: [nowhere]\n",
    );
    f.expect_error("piece-requires", "nowhere");
}

#[test]
fn event_name_must_be_a_vamd_event() {
    let f = Fx::new();
    f.edit("{ run: succeeded }", "{ event: assistant.message }");
    f.expect_error("event", "assistant.message");
    // Both spellings of a real event pass.
    f.edit("{ run: succeeded }", "{ event: vam.v1.tool.completed }");
    assert!(
        f.errors().iter().all(|e| !e.contains(": event: ")),
        "{:?}",
        f.errors()
    );
}

#[test]
fn as_must_name_an_org_piece() {
    let f = Fx::new();
    f.edit(
        "- chat: \"e2e-step:hello Hi\"",
        "- chat: { text: \"e2e-step:hello Hi\", as: agent-plain }",
    );
    f.expect_error("step-alias", "as: agent-plain");
    let f = Fx::new();
    f.edit(
        "- chat: \"e2e-step:hello Hi\"",
        "- chat: \"e2e-step:hello Hi\"\n  - approve: { always: true, as: nobody }",
    );
    f.expect_error("step-alias", "as: nobody");
    let f = Fx::new();
    f.edit(
        "- chat: \"e2e-step:hello Hi\"",
        "- chat: \"e2e-step:hello Hi\"\n  - decide-gate: { option: go, as: nobody }",
    );
    f.expect_error("step-alias", "as: nobody");
}

#[test]
fn draft_as_refused_and_gate_forms_are_clean() {
    let f = Fx::new();
    f.edit(
        "- chat: \"e2e-step:hello Hi\"",
        "- chat: { text: \"e2e-step:hello Hi\", draft: true, as: org-admin }\n  - expect: { approval: pending, tool: write_file, fields: { alwaysOffered: true } }\n  - approve: { always: true, refused: true, as: org-admin }\n  - expect: { gate: pending, step: confirm, fields: { approvalMessage: \"Go on?\" } }\n  - decide-gate: { option: later, refused: true }",
    );
    assert_eq!(f.errors(), Vec::<String>::new());
}

#[test]
fn fields_belong_to_event_approval_and_gate_only() {
    let f = Fx::new();
    f.edit("{ run: succeeded }", "{ run: succeeded, fields: { a: 1 } }");
    f.expect_error("expect", "`fields`");
}

#[test]
fn tool_and_state_are_not_read_by_a_gate() {
    let f = Fx::new();
    f.edit("{ run: succeeded }", "{ gate: pending, tool: write_file }");
    f.expect_error("expect", "`tool`");
    let f = Fx::new();
    f.edit(
        "{ run: succeeded }",
        "{ gate: pending, step: s, state: waiting }",
    );
    f.expect_error("expect", "`state`");
}

#[test]
fn gate_is_a_family_of_its_own() {
    let f = Fx::new();
    f.edit("{ run: succeeded }", "{ gate: pending, approval: pending }");
    f.expect_error("expect", "exactly one");
    let f = Fx::new();
    f.edit("{ run: succeeded }", "{ gate: done }");
    f.expect_error("expect", "supports only `pending`");
}

const NO_MODEL: &str = "id: chat-replies
title: t
tags: [chat]
needs: process
given: [org-admin]
when:
  - expect: { node: { fields: { status.guestMode: process } } }
then:
  - expect: { plain-http: refused }
status: active
";

#[test]
fn process_scenario_without_a_model_message_needs_no_mock() {
    let f = Fx::new();
    f.write("scenarios/chat-replies.yaml", NO_MODEL);
    assert_eq!(f.errors(), Vec::<String>::new());
}

#[test]
fn node_needs_a_field() {
    let f = Fx::new();
    f.write(
        "scenarios/chat-replies.yaml",
        &NO_MODEL.replace(
            "{ fields: { status.guestMode: process } }",
            "{ fields: {} }",
        ),
    );
    f.expect_error("expect", "at least one field");
}

#[test]
fn plain_http_and_vmm_take_one_word() {
    let f = Fx::new();
    f.write(
        "scenarios/chat-replies.yaml",
        &NO_MODEL.replace("plain-http: refused", "plain-http: open"),
    );
    f.expect_error("expect", "supports only `refused`");
    f.write(
        "scenarios/chat-replies.yaml",
        &NO_MODEL.replace("plain-http: refused", "vmm: confined"),
    );
    f.expect_error("expect", "supports only `jailed`");
}

#[test]
fn new_expect_families_do_not_mix() {
    let f = Fx::new();
    f.write(
        "scenarios/chat-replies.yaml",
        &NO_MODEL.replace("plain-http: refused", "plain-http: refused, vmm: jailed"),
    );
    f.expect_error("expect", "exactly one of");
}

const SETS: &str = "sets:\n  - version: 0.1.0\n    tasks: [reply-task]\n";

fn with_model(f: &Fx, block: &str) {
    f.write("task-sets.yaml", SETS);
    f.edit("status: active", &format!("{block}status: active"));
}

#[test]
fn a_model_block_in_a_pinned_set_is_clean_and_k_defaults_to_five() {
    let f = Fx::new();
    with_model(&f, "model: { task: reply-task, version: 0.1.0 }\n");
    assert!(f.errors().is_empty(), "{:#?}", f.errors());
    let cat = vam_e2e::catalog::Catalog::load(f.root());
    let m = cat.scenarios[0].1.model.clone().unwrap();
    assert_eq!((m.k, m.modes.as_slice()), (5, &["on".to_string()][..]));
}

#[test]
fn a_model_block_names_a_pinned_version_and_task() {
    let f = Fx::new();
    with_model(&f, "model: { task: reply-task, version: 9.9.9 }\n");
    f.expect_error("task-set", "9.9.9");
    let f = Fx::new();
    with_model(&f, "model: { task: other-task, version: 0.1.0 }\n");
    f.expect_error("task-set", "other-task");
    let f = Fx::new();
    f.edit(
        "status: active",
        "model: { task: reply-task, version: 0.1.0 }\nstatus: active",
    );
    f.expect_error("task-set", "task-sets.yaml");
}

#[test]
fn a_model_block_rejects_a_zero_k_another_mode_and_an_unknown_endpoint() {
    let f = Fx::new();
    with_model(
        &f,
        "model: { task: reply-task, version: 0.1.0, k: 0, modes: [off], endpoint: nope }\n",
    );
    f.expect_error("model", "`k` must be at least 1");
    f.expect_error("model", "may only hold `on`");
    f.expect_error("model", "endpoint `nope`");
}

const FLOOD: &str = "id: chat-replies
title: A flood
tags: [members]
needs: process
given: [org-admin, { piece: org-member, as: flooder }]
when:
  - flood: { as: flooder, path: /v1/node, count: 300 }
then:
  - expect: { flood: { rateLimited: { atLeast: 20 } } }
status: active
";
const MEMBER: &str = "piece: org-member\nkind: org\nrole: Member\n";

fn flood_fx(scenario: &str) -> Fx {
    let f = Fx::new();
    f.write("world/org-member.yaml", MEMBER);
    f.write("scenarios/chat-replies.yaml", scenario);
    f
}

#[test]
fn flood_catalog_is_clean() {
    let f = flood_fx(FLOOD);
    assert!(f.errors().is_empty(), "{:#?}", f.errors());
}

#[test]
fn flood_needs_an_org_alias_a_count_and_a_path() {
    let f = flood_fx(&FLOOD.replace("flood: { as: flooder", "flood: { as: nobody"));
    f.expect_error("step-alias", "`as: nobody` is not an org piece");
    let f = flood_fx(&FLOOD.replace("count: 300", "count: 0"));
    f.expect_error("flood", "count of at least 1");
    let f = flood_fx(&FLOOD.replace("path: /v1/node", "path: v1/node"));
    f.expect_error("flood", "path starting with `/`");
}

#[test]
fn a_flood_expect_needs_an_earlier_flood_step_and_a_check() {
    let f = flood_fx(&FLOOD.replace(
        "  - flood: { as: flooder, path: /v1/node, count: 300 }\n",
        "  - expect: { run: succeeded }\n",
    ));
    f.expect_error("expect", "expects an earlier `flood` step");
    let f = flood_fx(&FLOOD.replace("rateLimited: { atLeast: 20 }", "retryAfter: later"));
    f.expect_error("expect", "supports only `present`");
    let f = flood_fx(&FLOOD.replace("flood: { rateLimited: { atLeast: 20 } }", "flood: {}"));
    f.expect_error("expect", "needs rateLimited, retryAfter or recovers");
}

#[test]
fn secret_absent_names_a_secret_piece_of_given() {
    let f = flood_fx(&FLOOD.replace(
        "flood: { rateLimited: { atLeast: 20 } }",
        "secret-absent: { secret: org-admin }",
    ));
    f.expect_error("step-alias", "is not a secret piece in `given`");
}

#[test]
fn the_new_expect_families_do_not_mix_with_another_family() {
    let f = flood_fx(&FLOOD.replace(
        "flood: { rateLimited: { atLeast: 20 } }",
        "flood: { recovers: true }, run: succeeded",
    ));
    f.expect_error("expect", "exactly one of");
}

#[test]
fn thread_max_bytes_must_be_positive() {
    let f = Fx::new();
    f.edit(
        "expect: { run: succeeded }",
        "expect: { thread: { role: tool, contains: x, maxBytes: 0 } }",
    );
    f.expect_error("expect", "`maxBytes` must be at least 1");
}

#[test]
fn a_skill_piece_names_only_pieces_it_requires() {
    let f = Fx::new();
    f.write(
        "world/skill-x.yaml",
        "piece: skill-x\nkind: skill\nbody:\n  spec:\n    resourceId: { urn: \"urn:vam:skill:x/e2e/{{name}}\" }\n    d: \"{{name:api-token}}\"\n",
    );
    f.expect_error(
        "piece-requires",
        "`{{name:api-token}}` names a piece that is not in `requires`",
    );
    f.write(
        "world/skill-y.yaml",
        "piece: skill-y\nkind: skill\nbody:\n  spec:\n    resourceId: { urn: \"urn:vam:skill:x/e2e/no-name-placeholder\" }\n",
    );
    f.expect_error("piece-body", "must contain `{{name}}`");
}

const IDENTITY: &str = "id: chat-replies
title: A password change
tags: [members]
needs: process
given: [org-admin, { piece: org-member, as: person }]
when:
  - login: { as: person }
  - change-password: { as: person, status: 200, contains: reloginRequired }
then:
  - login: { as: person, password: previous, refused: true }
  - identity-request: { as: person, credential: previous-session, path: /v1/me, status: 401 }
status: active
";

#[test]
fn identity_steps_catalog_is_clean() {
    let f = flood_fx(IDENTITY);
    assert!(f.errors().is_empty(), "{:#?}", f.errors());
}

#[test]
fn identity_steps_need_org_aliases_and_sane_arguments() {
    let f = flood_fx(&IDENTITY.replace("login: { as: person }", "login: { as: nobody }"));
    f.expect_error("step-alias", "`as: nobody` is not an org piece");
    let f = flood_fx(&IDENTITY.replace(
        "change-password: { as: person,",
        "change-password: { as: nobody,",
    ));
    f.expect_error("step-alias", "`as: nobody` is not an org piece");
    let f = flood_fx(&IDENTITY.replace(
        "change-password: { as: person, status: 200",
        "change-password: { status: 200",
    ));
    f.expect_error("change-password", "needs `via: key`");
    let f = flood_fx(&IDENTITY.replace("status: 200,", "status: 20,"));
    f.expect_error("change-password", "HTTP status");
    let f = flood_fx(&IDENTITY.replace("path: /v1/me", "path: v1/me"));
    f.expect_error("identity-request", "path starting with `/`");
    let f = flood_fx(&IDENTITY.replace("credential: previous-session", "credential: cookie"));
    assert!(
        f.errors().iter().any(|e| e.contains("cookie")),
        "{:#?}",
        f.errors()
    );
}
