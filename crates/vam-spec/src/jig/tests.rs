use super::*;
use serde_json::json;

fn gate_data() -> Value {
    json!({
        "question": "Which supplier, and when?",
        "response_schema": {
            "type": "object",
            "required": ["supplier"],
            "properties": {
                "supplier": {"type": "string", "enum": ["acme", "globex"]},
                "qty": {"type": "integer", "minimum": 1},
                "rush": {"type": "boolean"},
                "due": {"type": "string", "format": "date"},
                "notes": {"type": "string", "maxLength": 500},
                "sites": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}}
            }
        }
    })
}

fn kinds(r: Result<(), Vec<JigError>>) -> Vec<(String, JigErrorKind)> {
    match r {
        Ok(()) => Vec::new(),
        Err(errs) => errs.into_iter().map(|e| (e.path, e.kind)).collect(),
    }
}

/// A one-node IR over a tiny input schema, for the error fixtures.
fn ir_with(nodes: Value) -> Value {
    json!({
        "version": 1, "catalog": "vam/1", "jig": "vam.test", "root": "root",
        "state": {"name": "", "count": 0},
        "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}, "n": {"type": "integer"}}},
        "nodes": nodes
    })
}

#[test]
fn registry_loads() {
    assert!(registry().is_ok(), "{:?}", registry().err());
    let mut names = builtin_names();
    names.sort();
    assert_eq!(
        names,
        [
            "vam.approve",
            "vam.gate_default",
            "vam.playbook_start",
            "vam.progress",
            "vam.question"
        ]
    );
}

#[test]
fn every_builtin_passes_check_ir() {
    for name in builtin_names() {
        let b = builtin(&name).unwrap();
        assert_eq!(kinds(check_ir(&b.ir())), vec![], "{name}");
    }
}

#[test]
fn generated_forms_pass_check_ir() {
    let ir = form_ir("vam.gate_default", &gate_data()).unwrap();
    assert_eq!(kinds(check_ir(&ir)), vec![]);
    // Required first, then name order; every property is in the submit.
    assert_eq!(
        ir["nodes"]["fields"]["children"],
        json!([
            "f_supplier",
            "f_due",
            "f_notes",
            "f_qty",
            "f_rush",
            "f_sites"
        ])
    );
    assert_eq!(ir["nodes"]["f_supplier"]["type"], "Select");
    assert_eq!(ir["nodes"]["f_due"]["type"], "DateInput");
    assert_eq!(ir["nodes"]["f_notes"]["type"], "TextArea");
    assert_eq!(ir["nodes"]["f_qty"]["props"]["min"], 1);
    assert_eq!(ir["nodes"]["f_sites"]["type"], "MultiSelect");
    assert_eq!(
        ir["nodes"]["send"]["props"]["action"]["with"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(ir["state"]["qty"], 1);

    let start = json!({"playbook": "onboard", "inputs_schema": {"type": "object", "properties": {"name": {"type": "string"}}}});
    let ir = form_ir("vam.playbook_start", &start).unwrap();
    assert_eq!(kinds(check_ir(&ir)), vec![]);
}

#[test]
fn a_field_with_no_form_element_is_refused() {
    let data = json!({"question": "q", "response_schema": {"type": "object", "properties": {"x": {"type": "object"}}}});
    let errs = check_data("vam.gate_default", &data).unwrap_err();
    assert_eq!(errs[0].path, "/response_schema/properties/x");
}

#[test]
fn ir_error_unknown_primitive() {
    let ir = ir_with(json!({"root": {"type": "Carousel"}}));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/type".into(), JigErrorKind::UnknownPrimitive)]
    );
}

#[test]
fn ir_error_reserved_query_and_mutation() {
    let ir = ir_with(json!({"root": {"type": "Query"}}));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/type".into(), JigErrorKind::Reserved)]
    );
    let ir = ir_with(
        json!({"root": {"type": "Button", "props": {"label": "x", "action": {"event": "mutation"}}}}),
    );
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![(
            "/nodes/root/props/action/event".into(),
            JigErrorKind::Reserved
        )]
    );
}

#[test]
fn ir_error_unknown_prop_missing_prop_and_wrong_type() {
    let ir = ir_with(json!({"root": {"type": "Text", "props": {"colour": "red"}}}));
    let k = kinds(check_ir(&ir));
    assert!(k.contains(&("/nodes/root/props/colour".into(), JigErrorKind::UnknownProp)));
    assert!(k.contains(&("/nodes/root/props/text".into(), JigErrorKind::MissingProp)));

    let ir = ir_with(json!({"root": {"type": "Text", "props": {"text": 5}}}));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/props/text".into(), JigErrorKind::WrongType)]
    );
}

#[test]
fn ir_error_bad_token() {
    let ir = ir_with(json!({"root": {"type": "Text", "props": {"text": "x", "tone": "purple"}}}));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/props/tone".into(), JigErrorKind::BadToken)]
    );
}

#[test]
fn ir_error_binding_to_a_missing_path_or_state() {
    let ir = ir_with(json!({"root": {"type": "Text", "props": {"text": {"$data": "/nope"}}}}));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/props/text".into(), JigErrorKind::BadBinding)]
    );
    let ir = ir_with(json!({"root": {"type": "Text", "props": {"text": {"$state": "nope"}}}}));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/props/text".into(), JigErrorKind::BadBinding)]
    );
}

#[test]
fn ir_error_binding_of_the_wrong_type() {
    // ProgressBar.value is a number; /title is a string.
    let ir = ir_with(
        json!({"root": {"type": "ProgressBar", "props": {"value": {"$data": "/title"}, "max": 3}}}),
    );
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/props/value".into(), JigErrorKind::WrongType)]
    );
    // An input's value must be a state binding.
    let ir = ir_with(
        json!({"root": {"type": "Input", "props": {"label": "x", "value": {"$data": "/title"}}}}),
    );
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/props/value".into(), JigErrorKind::WrongType)]
    );
}

#[test]
fn ir_error_unknown_action_and_unknown_state_in_action() {
    let ir = ir_with(
        json!({"root": {"type": "Button", "props": {"label": "x", "action": {"event": "openUrl"}}}}),
    );
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![(
            "/nodes/root/props/action/event".into(),
            JigErrorKind::UnknownAction
        )]
    );
    let ir = ir_with(
        json!({"root": {"type": "Button", "props": {"label": "x", "action": {"event": "submit", "with": ["ghost"]}}}}),
    );
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![(
            "/nodes/root/props/action/with/0".into(),
            JigErrorKind::BadBinding
        )]
    );
}

#[test]
fn ir_error_children() {
    let ir = ir_with(json!({"root": {"type": "Text", "props": {"text": "x"}, "children": ["a"]}}));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![(
            "/nodes/root/children".into(),
            JigErrorKind::ChildrenNotAllowed
        )]
    );
    let ir = ir_with(json!({"root": {"type": "Stack", "children": ["ghost"]}}));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root/children/0".into(), JigErrorKind::UnknownNode)]
    );
    let ir = ir_with(json!({
        "root": {"type": "Stack", "children": ["b"]},
        "b": {"type": "Stack", "children": ["root"]}
    }));
    assert_eq!(
        kinds(check_ir(&ir)),
        vec![("/nodes/root".into(), JigErrorKind::Cycle)]
    );
}

#[test]
fn ir_error_catalog_version_and_root() {
    let mut ir = ir_with(json!({"root": {"type": "Stack"}}));
    ir["catalog"] = json!("vam/2");
    ir["version"] = json!(2);
    ir["root"] = json!("gone");
    let k = kinds(check_ir(&ir));
    assert!(k.contains(&("/catalog".into(), JigErrorKind::UnknownCatalog)));
    assert!(k.contains(&("/version".into(), JigErrorKind::UnsupportedVersion)));
    assert!(k.contains(&("/root".into(), JigErrorKind::UnknownNode)));
}

#[test]
fn progress_accepts_the_scenario_shape() {
    let ok = json!({"label": "Checking", "done": 1, "total": 2});
    assert!(check_data("vam.progress", &ok).is_ok());
    let full =
        json!({"label": "Checking", "done": 1, "total": 2, "message": "half", "card_id": "checks"});
    assert!(check_data("vam.progress", &full).is_ok());
}

#[test]
fn progress_refuses_a_wrong_type_with_a_named_path() {
    // The bad data of progress-shows-while-the-task-runs.
    let bad = json!({"label": "Checking", "done": "one", "total": 2});
    let errs = check_data("vam.progress", &bad).unwrap_err();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].path, "/done");
    assert_eq!(errs[0].kind, JigErrorKind::InvalidData);
    assert!(errs[0].message.contains("integer"), "{}", errs[0].message);
}

#[test]
fn check_data_refuses_extra_oversize_and_missing() {
    let extra = json!({"label": "x", "done": 1, "total": 2, "percent": 50});
    assert_eq!(
        kinds(check_data("vam.progress", &extra)),
        vec![("/percent".into(), JigErrorKind::InvalidData)]
    );
    let long = json!({"label": "x".repeat(201), "done": 1, "total": 2});
    assert_eq!(
        kinds(check_data("vam.progress", &long)),
        vec![("/label".into(), JigErrorKind::InvalidData)]
    );
    let missing = json!({"label": "x", "done": 1});
    assert_eq!(
        kinds(check_data("vam.progress", &missing)),
        vec![("/total".into(), JigErrorKind::InvalidData)]
    );
    let negative = json!({"label": "x", "done": -1, "total": 2});
    assert_eq!(
        kinds(check_data("vam.progress", &negative)),
        vec![("/done".into(), JigErrorKind::InvalidData)]
    );
    assert_eq!(
        check_data("vam.nothing", &json!({})).unwrap_err()[0].kind,
        JigErrorKind::UnknownJig
    );
}

#[test]
fn question_data_and_result_rules() {
    assert!(check_data(
        "vam.question",
        &json!({"question": "Which name should I use?"})
    )
    .is_ok());
    assert!(check_data("vam.question", &json!({"question": "q", "choices": []})).is_err());

    // Free text: a string. A number is refused.
    let free = json!({"question": "Which name should I use?"});
    assert!(check_result("vam.question", &free, &json!({"answer": "Ada"})).is_ok());
    let errs = check_result("vam.question", &free, &json!({"answer": 42})).unwrap_err();
    assert_eq!(errs[0].path, "/answer");
    assert_eq!(errs[0].kind, JigErrorKind::InvalidResult);

    // Choices: only an offered one (jig decision 14).
    let offered = json!({"question": "Pick", "choices": ["red", "blue"]});
    assert!(check_result("vam.question", &offered, &json!({"answer": "blue"})).is_ok());
    let errs = check_result("vam.question", &offered, &json!({"answer": "green"})).unwrap_err();
    assert_eq!(errs[0].path, "/answer");
    assert!(errs[0].message.contains("offered"));
    assert!(check_result(
        "vam.question",
        &offered,
        &json!({"answer": "blue", "extra": 1})
    )
    .is_err());
    assert!(check_result("vam.question", &offered, &json!({})).is_err());
}

#[test]
fn approve_result() {
    assert!(check_result(
        "vam.approve",
        &json!({}),
        &json!({"decision": "deny", "reason": "no"})
    )
    .is_ok());
    assert!(check_result("vam.approve", &json!({}), &json!({"decision": "maybe"})).is_err());
    // Always deny is a decision (the ActionCard's fourth choice).
    assert!(check_result(
        "vam.approve",
        &json!({}),
        &json!({"decision": "always_deny"})
    )
    .is_ok());
    // A submit result carries the `set` names as well as `with`: without
    // `decision` (a `with`-only button) the result is refused.
    assert!(check_result("vam.approve", &json!({}), &json!({"reason": "only with"})).is_err());
}

#[test]
fn every_approve_button_submits_a_decision_through_set() {
    let b = builtin("vam.approve").unwrap();
    let ir = b.ir();
    let mut seen = Vec::new();
    for (_, node) in ir["nodes"].as_object().unwrap() {
        if node["type"] == "Button" {
            let set = &node["props"]["action"]["set"];
            let decision = set["decision"].as_str().expect("a button sets decision");
            // The union of `set` and `with` is a valid result.
            let mut result = serde_json::Map::new();
            result.insert("decision".into(), json!(decision));
            result.insert("reason".into(), json!("x"));
            assert!(check_result("vam.approve", &json!({}), &result.into()).is_ok());
            seen.push(decision.to_string());
        }
    }
    seen.sort();
    assert_eq!(seen, ["always", "always_deny", "approve", "deny"]);
}

#[test]
fn gate_result_holds_only_offered_items() {
    let data = gate_data();
    let ok = json!({"supplier": "acme", "qty": 2, "sites": ["a"]});
    assert!(check_result("vam.gate_default", &data, &ok).is_ok());
    let bad = json!({"supplier": "initech"});
    assert_eq!(
        kinds(check_result("vam.gate_default", &data, &bad)),
        vec![("/supplier".into(), JigErrorKind::InvalidResult)]
    );
    let bad = json!({"supplier": "acme", "sites": ["a", "z"]});
    assert_eq!(
        kinds(check_result("vam.gate_default", &data, &bad)),
        vec![("/sites/1".into(), JigErrorKind::InvalidResult)]
    );
    let bad = json!({"qty": 2});
    assert_eq!(
        kinds(check_result("vam.gate_default", &data, &bad)),
        vec![("/supplier".into(), JigErrorKind::InvalidResult)]
    );
    let bad = json!({"supplier": "acme", "due": "tomorrow"});
    assert_eq!(
        kinds(check_result("vam.gate_default", &data, &bad)),
        vec![("/due".into(), JigErrorKind::InvalidResult)]
    );
}

#[test]
fn progress_has_no_result() {
    let data = json!({"label": "x", "done": 1, "total": 2});
    assert!(check_result("vam.progress", &data, &json!({})).is_ok());
    assert!(check_result("vam.progress", &data, &json!({"x": 1})).is_err());
}

#[test]
fn digest_is_stable_and_printed() {
    let d = catalog_digest();
    assert_eq!(d.len(), 64);
    assert!(d.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(d, catalog_digest());
    println!("spec/jig digest: {d}");
}

#[test]
fn embedded_files_are_in_sync_with_the_disk() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/jig");
    for (path, content) in FILES {
        let on_disk = std::fs::read_to_string(root.join(path)).unwrap();
        assert_eq!(on_disk, content, "{path}");
    }
    // No file under spec/jig is left out of the digest.
    let mut found = 0;
    for entry in walk(&root) {
        assert!(
            FILES.iter().any(|(p, _)| root.join(p) == entry),
            "{entry:?} is not in FILES"
        );
        found += 1;
    }
    assert_eq!(found, FILES.len());
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// List and the item binding, and the skill-card helpers.

const FIX: &str = "../../e2e/world/fixtures/mail-calendar-cards/jigs";

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIX)
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn mail_schema() -> Value {
    json!({"type":"object","required":["messages"],"properties":{"messages":{"type":"array","items":{"type":"object","properties":{"from":{"type":"string"},"subject":{"type":"string"}}}}}})
}

fn slots_schema() -> Value {
    json!({"type":"object","required":["slots"],"properties":{"slots":{"type":"array","items":{"type":"object","properties":{"start":{"type":"string"},"end":{"type":"string"}}}}}})
}

fn send_input() -> Value {
    json!({"type":"object","properties":{"to":{"type":"string"},"subject":{"type":"string"},"body":{"type":"string"}}})
}

fn book_input() -> Value {
    json!({"type":"object","properties":{"slot":{"type":"string"}}})
}

fn list_ir(extra: Value) -> Value {
    let mut nodes = json!({
        "root": {"type":"List","props":{"items":{"$data":"/messages"}},"children":["row"]},
        "row": {"type":"Text","props":{"text":{"$item":"/subject"}}}
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        nodes[k] = v;
    }
    json!({"version":1,"catalog":"vam/1","root":"root","state":{},"nodes":nodes})
}

#[test]
fn good_fixtures_pass() {
    check_card(&fixture("mail_list.json"), &mail_schema()).unwrap();
    check_card(&fixture("slot_list.json"), &slots_schema()).unwrap();
    check_ask_card(&fixture("send_approval.json"), &send_input()).unwrap();
    check_ask_card(&fixture("book_approval.json"), &book_input()).unwrap();
}

#[test]
fn bad_fixtures_are_refused() {
    let r = kinds(check_card(&fixture("bad_binding.json"), &mail_schema()));
    assert!(
        r.contains(&("/nodes/f0/props/text".into(), JigErrorKind::BadBinding)),
        "{r:?}"
    );
    let r = kinds(check_ask_card(&fixture("bad_submit.json"), &send_input()));
    assert!(
        r.contains(&(
            "/nodes/ok/props/action/with/2".into(),
            JigErrorKind::BadBinding
        )),
        "{r:?}"
    );
    let r = kinds(check_ask_card(
        &fixture("unknown_primitive.json"),
        &send_input(),
    ));
    assert!(
        r.contains(&("/nodes/in_to/type".into(), JigErrorKind::UnknownPrimitive)),
        "{r:?}"
    );
}

#[test]
fn list_with_item_text_checks() {
    check_card(&list_ir(json!({})), &mail_schema()).unwrap();
}

#[test]
fn nested_list_is_refused() {
    let ir = list_ir(json!({
        "row": {"type":"List","props":{"items":{"$data":"/messages"}},"children":["leaf"]},
        "leaf": {"type":"Text","props":{"text":"x"}}
    }));
    let r = kinds(check_card(&ir, &mail_schema()));
    assert_eq!(
        r,
        vec![("/nodes/row/type".to_string(), JigErrorKind::BadBinding)]
    );
}

#[test]
fn item_outside_a_list_is_refused() {
    let ir = json!({"version":1,"catalog":"vam/1","root":"t","state":{},
        "nodes":{"t":{"type":"Text","props":{"text":{"$item":"/subject"}}}}});
    let r = kinds(check_card(&ir, &mail_schema()));
    assert_eq!(
        r,
        vec![("/nodes/t/props/text".to_string(), JigErrorKind::BadBinding)]
    );
    // A node shared between the list and the rest of the card counts as outside.
    let ir = json!({"version":1,"catalog":"vam/1","root":"s","state":{},"nodes":{
        "s":{"type":"Stack","children":["l","row"]},
        "l":{"type":"List","props":{"items":{"$data":"/messages"}},"children":["row"]},
        "row":{"type":"Text","props":{"text":{"$item":"/subject"}}}}});
    let r = kinds(check_card(&ir, &mail_schema()));
    assert_eq!(
        r,
        vec![(
            "/nodes/row/props/text".to_string(),
            JigErrorKind::BadBinding
        )]
    );
}

#[test]
fn item_pointer_missing_from_the_item_schema_is_refused() {
    let ir = list_ir(json!({"row": {"type":"Text","props":{"text":{"$item":"/sender"}}}}));
    let r = kinds(check_card(&ir, &mail_schema()));
    assert_eq!(
        r,
        vec![(
            "/nodes/row/props/text".to_string(),
            JigErrorKind::BadBinding
        )]
    );
    let ir = list_ir(json!({"row": {"type":"Text","props":{"text":{"$item":""}}}}));
    assert_eq!(kinds(check_card(&ir, &mail_schema())).len(), 1);
}

#[test]
fn list_shape_rules() {
    // Two children.
    let ir = list_ir(
        json!({"root": {"type":"List","props":{"items":{"$data":"/messages"}},"children":["row","row"]}}),
    );
    assert_eq!(
        kinds(check_card(&ir, &mail_schema())),
        vec![("/nodes/root/children".to_string(), JigErrorKind::BadShape)]
    );
    // Items as a literal.
    let ir = list_ir(json!({"root": {"type":"List","props":{"items":[1,2]},"children":["row"]}}));
    assert!(kinds(check_card(&ir, &mail_schema())).contains(&(
        "/nodes/root/props/items".to_string(),
        JigErrorKind::WrongType
    )));
    // Items bound to a non-array.
    let ir = list_ir(
        json!({"root": {"type":"List","props":{"items":{"$data":"/n"}},"children":["row"]}}),
    );
    let schema = json!({"type":"object","properties":{"n":{"type":"integer"}}});
    assert!(kinds(check_card(&ir, &schema)).contains(&(
        "/nodes/root/props/items".to_string(),
        JigErrorKind::WrongType
    )));
    // $item on an action prop or a state-bound prop.
    let ir = list_ir(
        json!({"row": {"type":"Input","props":{"label":"x","value":{"$item":"/subject"}}}}),
    );
    assert!(!kinds(check_card(&ir, &mail_schema())).is_empty());
}

#[test]
fn builtins_still_check_and_list_is_in_the_catalog() {
    for n in builtin_names() {
        check_ir(&builtin(&n).unwrap().ir()).unwrap();
    }
    assert!(CATALOG_JSON.contains("\"List\""));
    assert_eq!(LIST_PAINT_CAP, 100);
}

#[test]
fn ask_card_decision_is_reserved() {
    // A declared decision must be a string defaulting to "".
    let mut ir = fixture("send_approval.json");
    ir["state"]["decision"] = json!("approve");
    let r = kinds(check_ask_card(&ir, &send_input()));
    assert!(
        r.contains(&("/state/decision".to_string(), JigErrorKind::WrongType)),
        "{r:?}"
    );
    ir["state"]["decision"] = json!("");
    check_ask_card(&ir, &send_input()).unwrap();
    // Never submitted through `with`.
    let mut ir = fixture("send_approval.json");
    ir["nodes"]["ok"]["props"]["action"]["with"] = json!(["to", "decision"]);
    let input =
        json!({"type":"object","properties":{"to":{"type":"string"},"decision":{"type":"string"}}});
    let r = kinds(check_ask_card(&ir, &input));
    assert!(
        r.contains(&(
            "/nodes/ok/props/action/with/1".to_string(),
            JigErrorKind::BadBinding
        )),
        "{r:?}"
    );
    // The buttons set only approve or deny.
    let mut ir = fixture("send_approval.json");
    ir["nodes"]["ok"]["props"]["action"]["set"]["decision"] = json!("maybe");
    let r = kinds(check_ask_card(&ir, &send_input()));
    assert!(
        r.contains(&(
            "/nodes/ok/props/action/set/decision".to_string(),
            JigErrorKind::WrongType
        )),
        "{r:?}"
    );
}

#[test]
fn bind_args_prefills_state_and_never_sets_decision() {
    let ir = fixture("send_approval.json");
    let bound = bind_args(
        &ir,
        &json!({"to":"a@b.c","subject":"Hi","body":5,"other":1}),
    );
    assert_eq!(bound["state"]["to"], "a@b.c");
    assert_eq!(bound["state"]["subject"], "Hi");
    assert_eq!(
        bound["state"]["body"], "",
        "a value of another kind is ignored"
    );
    assert!(bound["state"].get("other").is_none());
    let mut ir = fixture("send_approval.json");
    ir["state"]["decision"] = json!("");
    let bound = bind_args(&ir, &json!({"decision":"approve"}));
    assert_eq!(bound["state"]["decision"], "");
}
