//! Jig: the closed UI catalog, the IR checker and the built-in jigs
//! (`design/jig.md`, E10 Phase 3 AR-3.4).
//!
//! The files in `spec/jig/` are the source of truth and are embedded here.
//! Three checks, all pure:
//!
//! - [`check_ir`]: an IR against the catalog (unknown primitive or prop, wrong
//!   type, binding to a missing path, reserved `Query` / `Mutation`).
//! - [`check_data`]: the data of a `show` / `ask_user` against a built-in's
//!   input schema.
//! - [`check_result`]: a submitted answer against a built-in's result schema
//!   and its offered-items rule (jig decision 14).
//!
//! Every error names a path (a JSON pointer into the checked value).

use std::collections::{HashMap, HashSet};
use std::fmt;

use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::schema_format::check_format;

const CATALOG_JSON: &str = include_str!("../../../spec/jig/catalog.json");
const IR_SCHEMA_JSON: &str = include_str!("../../../spec/jig/ir.schema.json");
const IR_D_TS: &str = include_str!("../../../spec/jig/ir.d.ts");
const A2UI_MD: &str = include_str!("../../../spec/jig/a2ui.md");
const APPROVE_JSON: &str = include_str!("../../../spec/jig/builtins/approve.json");
const QUESTION_JSON: &str = include_str!("../../../spec/jig/builtins/question.json");
const PROGRESS_JSON: &str = include_str!("../../../spec/jig/builtins/progress.json");
const PLAYBOOK_START_JSON: &str = include_str!("../../../spec/jig/builtins/playbook_start.json");
const GATE_DEFAULT_JSON: &str = include_str!("../../../spec/jig/builtins/gate_default.json");

/// Every file under `spec/jig/`, in digest order.
const FILES: [(&str, &str); 9] = [
    ("a2ui.md", A2UI_MD),
    ("builtins/approve.json", APPROVE_JSON),
    ("builtins/gate_default.json", GATE_DEFAULT_JSON),
    ("builtins/playbook_start.json", PLAYBOOK_START_JSON),
    ("builtins/progress.json", PROGRESS_JSON),
    ("builtins/question.json", QUESTION_JSON),
    ("catalog.json", CATALOG_JSON),
    ("ir.d.ts", IR_D_TS),
    ("ir.schema.json", IR_SCHEMA_JSON),
];

/// A `List` paints at most this many elements, then "and N more" (the
/// renderer's rule; the checker cannot know the data).
pub const LIST_PAINT_CAP: usize = 100;

/// The catalog name and version the IR must carry.
pub const CATALOG_NAME: &str = "vam/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JigErrorKind {
    BadShape,
    UnsupportedVersion,
    UnknownCatalog,
    UnknownNode,
    Cycle,
    UnknownPrimitive,
    UnknownProp,
    MissingProp,
    WrongType,
    BadToken,
    BadBinding,
    /// `Query` / `Mutation`: reserved for E11 and refused.
    Reserved,
    UnknownAction,
    ChildrenNotAllowed,
    UnknownJig,
    InvalidData,
    InvalidResult,
}

/// One problem, with the JSON pointer of the value it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JigError {
    pub path: String,
    pub kind: JigErrorKind,
    pub message: String,
}

impl JigError {
    fn new(path: impl Into<String>, kind: JigErrorKind, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for JigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() {
            "/"
        } else {
            &self.path
        };
        write!(f, "{path}: {}", self.message)
    }
}

impl std::error::Error for JigError {}

// ---------------------------------------------------------------------------
// Catalog and built-ins, parsed once.

#[derive(Debug)]
struct PropSpec {
    name: String,
    ty: String,
    required: bool,
    bind_state: bool,
    values: Vec<String>,
}

#[derive(Debug)]
struct PrimitiveSpec {
    children: bool,
    props: Vec<PropSpec>,
}

#[derive(Debug)]
struct Catalog {
    name: String,
    tokens: HashMap<String, Vec<String>>,
    primitives: HashMap<String, PrimitiveSpec>,
    reserved: Vec<String>,
}

#[derive(Debug)]
struct FormSpec {
    schema: String,
    into: String,
    submit: String,
}

/// A built-in jig as shipped in `spec/jig/builtins/`.
#[derive(Debug)]
pub struct Builtin {
    name: String,
    input_schema: Value,
    result_schema: Value,
    /// JSON pointer into the input data of the schema the result is checked
    /// against (a gate's `response_schema`), when the result depends on it.
    result_from: Option<String>,
    form: Option<FormSpec>,
    ir: Value,
}

impl Builtin {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    pub fn result_schema(&self) -> &Value {
        &self.result_schema
    }

    /// The static IR, with the schemas filled in. For `vam.gate_default` and
    /// `vam.playbook_start` the form fields are empty: use [`form_ir`].
    pub fn ir(&self) -> Value {
        let mut ir = self.ir.clone();
        if let Some(obj) = ir.as_object_mut() {
            obj.insert("inputSchema".into(), self.input_schema.clone());
            obj.insert("resultSchema".into(), self.result_schema.clone());
        }
        ir
    }
}

struct Registry {
    catalog: Catalog,
    builtins: Vec<Builtin>,
}

static REGISTRY: Lazy<Result<Registry, String>> = Lazy::new(load_registry);

fn registry() -> Result<&'static Registry, Vec<JigError>> {
    REGISTRY.as_ref().map_err(|message| {
        vec![JigError::new(
            "",
            JigErrorKind::BadShape,
            format!("embedded jig files are invalid: {message}"),
        )]
    })
}

fn parse(text: &str, what: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| format!("{what}: {e}"))
}

fn str_of<'a>(v: &'a Value, key: &str, what: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{what}: '{key}' must be a string"))
}

fn load_registry() -> Result<Registry, String> {
    let catalog = load_catalog(&parse(CATALOG_JSON, "catalog.json")?)?;
    let sources = [
        ("approve.json", APPROVE_JSON),
        ("question.json", QUESTION_JSON),
        ("progress.json", PROGRESS_JSON),
        ("playbook_start.json", PLAYBOOK_START_JSON),
        ("gate_default.json", GATE_DEFAULT_JSON),
    ];
    let mut builtins = Vec::new();
    for (file, text) in sources {
        builtins.push(load_builtin(&parse(text, file)?, file)?);
    }
    Ok(Registry { catalog, builtins })
}

fn load_catalog(v: &Value) -> Result<Catalog, String> {
    let name = str_of(v, "catalog", "catalog.json")?.to_string();
    let mut tokens = HashMap::new();
    for (k, list) in v
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or("catalog.json: 'tokens' must be an object")?
    {
        tokens.insert(k.clone(), string_list(list, "catalog.json tokens")?);
    }
    let reserved = string_list(
        v.get("reserved").unwrap_or(&Value::Null),
        "catalog.json reserved",
    )?;
    let mut primitives = HashMap::new();
    for (pname, p) in v
        .get("primitives")
        .and_then(Value::as_object)
        .ok_or("catalog.json: 'primitives' must be an object")?
    {
        let mut props = Vec::new();
        for prop in p
            .get("props")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("catalog.json {pname}: 'props' must be an array"))?
        {
            props.push(PropSpec {
                name: str_of(prop, "name", pname)?.to_string(),
                ty: str_of(prop, "type", pname)?.to_string(),
                required: prop
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                bind_state: prop.get("bind").and_then(Value::as_str) == Some("state"),
                values: match prop.get("values") {
                    Some(list) => string_list(list, pname)?,
                    None => Vec::new(),
                },
            });
        }
        primitives.insert(
            pname.clone(),
            PrimitiveSpec {
                children: p.get("children").and_then(Value::as_bool).unwrap_or(false),
                props,
            },
        );
    }
    Ok(Catalog {
        name,
        tokens,
        primitives,
        reserved,
    })
}

fn string_list(v: &Value, what: &str) -> Result<Vec<String>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{what}: expected strings"))
            })
            .collect(),
        _ => Err(format!("{what}: expected an array")),
    }
}

fn load_builtin(v: &Value, file: &str) -> Result<Builtin, String> {
    let form = match v.get("form") {
        None => None,
        Some(f) => Some(FormSpec {
            schema: str_of(f, "schema", file)?.to_string(),
            into: str_of(f, "into", file)?.to_string(),
            submit: str_of(f, "submit", file)?.to_string(),
        }),
    };
    Ok(Builtin {
        name: str_of(v, "name", file)?.to_string(),
        input_schema: v
            .get("inputSchema")
            .cloned()
            .ok_or_else(|| format!("{file}: missing inputSchema"))?,
        result_schema: v
            .get("resultSchema")
            .cloned()
            .ok_or_else(|| format!("{file}: missing resultSchema"))?,
        result_from: v
            .get("resultFrom")
            .and_then(Value::as_str)
            .map(str::to_string),
        form,
        ir: v
            .get("ir")
            .cloned()
            .ok_or_else(|| format!("{file}: missing ir"))?,
    })
}

/// The built-in jig named `name` (for example `vam.question`).
pub fn builtin(name: &str) -> Result<&'static Builtin, Vec<JigError>> {
    registry()?
        .builtins
        .iter()
        .find(|b| b.name == name)
        .ok_or_else(|| {
            vec![JigError::new(
                "/jig",
                JigErrorKind::UnknownJig,
                format!("no built-in jig named '{name}'"),
            )]
        })
}

/// Names of the built-in jigs.
pub fn builtin_names() -> Vec<String> {
    match registry() {
        Ok(r) => r.builtins.iter().map(|b| b.name.clone()).collect(),
        Err(_) => Vec::new(),
    }
}

/// SHA-256 (hex) over every file of `spec/jig/`, path and content, in a fixed
/// order. The VAM app pins this to know its vendored copy is current.
pub fn catalog_digest() -> String {
    let mut hasher = Sha256::new();
    for (path, content) in FILES {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(content.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// check_ir

/// Check an IR against the catalog. Returns every problem found.
pub fn check_ir(ir: &Value) -> Result<(), Vec<JigError>> {
    let catalog = &registry()?.catalog;
    let mut errs = Vec::new();
    let Some(obj) = ir.as_object() else {
        return Err(vec![JigError::new(
            "",
            JigErrorKind::BadShape,
            "the IR must be an object",
        )]);
    };
    for key in obj.keys() {
        if !matches!(
            key.as_str(),
            "version"
                | "catalog"
                | "jig"
                | "root"
                | "nodes"
                | "state"
                | "inputSchema"
                | "resultSchema"
        ) {
            errs.push(JigError::new(
                format!("/{key}"),
                JigErrorKind::BadShape,
                "unknown top-level field",
            ));
        }
    }
    if obj.get("version") != Some(&json!(1)) {
        errs.push(JigError::new(
            "/version",
            JigErrorKind::UnsupportedVersion,
            "version must be 1",
        ));
    }
    match obj.get("catalog").and_then(Value::as_str) {
        Some(c) if c == catalog.name => {}
        Some(c) => errs.push(JigError::new(
            "/catalog",
            JigErrorKind::UnknownCatalog,
            format!(
                "catalog '{c}' is not known; this host speaks '{}'",
                catalog.name
            ),
        )),
        None => errs.push(JigError::new(
            "/catalog",
            JigErrorKind::UnknownCatalog,
            "catalog must name a catalog and version, for example 'vam/1'",
        )),
    }
    if !obj.get("jig").is_some_and(Value::is_string) {
        errs.push(JigError::new(
            "/jig",
            JigErrorKind::BadShape,
            "jig must be a string",
        ));
    }
    let empty = Map::new();
    let state = match obj.get("state") {
        None => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => {
            errs.push(JigError::new(
                "/state",
                JigErrorKind::BadShape,
                "state must be an object",
            ));
            &empty
        }
    };
    let Some(nodes) = obj.get("nodes").and_then(Value::as_object) else {
        errs.push(JigError::new(
            "/nodes",
            JigErrorKind::BadShape,
            "nodes must be an object",
        ));
        return Err(errs);
    };
    match obj.get("root").and_then(Value::as_str) {
        Some(root) if nodes.contains_key(root) => {}
        Some(root) => errs.push(JigError::new(
            "/root",
            JigErrorKind::UnknownNode,
            format!("root '{root}' is not a node"),
        )),
        None => errs.push(JigError::new(
            "/root",
            JigErrorKind::BadShape,
            "root must be a node id",
        )),
    }
    let scopes = list_scopes(
        obj.get("root").and_then(Value::as_str),
        nodes,
        obj.get("inputSchema"),
    );
    let ctx = Ctx {
        catalog,
        nodes,
        state,
        input_schema: obj.get("inputSchema"),
        scopes,
    };
    for (id, node) in nodes {
        check_node(&ctx, id, node, &mut errs);
    }
    check_cycles(obj.get("root").and_then(Value::as_str), nodes, &mut errs);
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

struct Ctx<'a> {
    catalog: &'a Catalog,
    nodes: &'a Map<String, Value>,
    state: &'a Map<String, Value>,
    input_schema: Option<&'a Value>,
    scopes: ListScopes,
}

/// Where each node sits relative to `List` nodes, from a walk from the root.
#[derive(Default)]
struct ListScopes {
    /// Nodes under a `List`, with the schema of one array element.
    inside: HashMap<String, Value>,
    /// The `$data` pointer of the array that feeds each node in `inside`.
    from: HashMap<String, String>,
    /// Nodes reached without passing a `List`.
    outside: HashSet<String>,
    /// `List` nodes reached while already inside a `List`.
    nested: HashSet<String>,
}

fn list_scopes(
    root: Option<&str>,
    nodes: &Map<String, Value>,
    input_schema: Option<&Value>,
) -> ListScopes {
    fn walk(
        id: &str,
        item: Option<&Value>,
        from: Option<&str>,
        nodes: &Map<String, Value>,
        input_schema: Option<&Value>,
        sc: &mut ListScopes,
        seen: &mut HashSet<(String, bool)>,
    ) {
        let Some(node) = nodes.get(id) else {
            return;
        };
        if !seen.insert((id.to_string(), item.is_some())) {
            return;
        }
        match item {
            Some(schema) => {
                sc.inside
                    .entry(id.to_string())
                    .or_insert_with(|| schema.clone());
                if let Some(from) = from {
                    sc.from
                        .entry(id.to_string())
                        .or_insert_with(|| from.to_string());
                }
            }
            None => {
                sc.outside.insert(id.to_string());
            }
        }
        let children: Vec<&str> = node
            .get("children")
            .and_then(Value::as_array)
            .map(|c| c.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut child_item = item.cloned();
        let mut child_from = from.map(str::to_string);
        if node.get("type").and_then(Value::as_str) == Some("List") {
            if item.is_some() {
                sc.nested.insert(id.to_string());
                return;
            }
            let element = node
                .pointer("/props/items/$data")
                .and_then(Value::as_str)
                .and_then(|ptr| input_schema.and_then(|s| resolve_schema(s, ptr)))
                .and_then(|found| found.get("items").cloned())
                .unwrap_or_else(|| json!({}));
            child_item = Some(element);
            child_from = node
                .pointer("/props/items/$data")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        for child in children {
            walk(
                child,
                child_item.as_ref(),
                child_from.as_deref(),
                nodes,
                input_schema,
                sc,
                seen,
            );
        }
    }
    let mut sc = ListScopes::default();
    if let Some(root) = root {
        walk(
            root,
            None,
            None,
            nodes,
            input_schema,
            &mut sc,
            &mut HashSet::new(),
        );
    }
    sc
}

fn check_node(ctx: &Ctx, id: &str, node: &Value, errs: &mut Vec<JigError>) {
    let base = format!("/nodes/{id}");
    let Some(n) = node.as_object() else {
        errs.push(JigError::new(
            &base,
            JigErrorKind::BadShape,
            "a node must be an object",
        ));
        return;
    };
    for key in n.keys() {
        if !matches!(
            key.as_str(),
            "type" | "props" | "children" | "visible" | "hidden"
        ) {
            errs.push(JigError::new(
                format!("{base}/{key}"),
                JigErrorKind::BadShape,
                "unknown node field",
            ));
        }
    }
    let Some(ty) = n.get("type").and_then(Value::as_str) else {
        errs.push(JigError::new(
            format!("{base}/type"),
            JigErrorKind::BadShape,
            "type must be a string",
        ));
        return;
    };
    if ctx.catalog.reserved.iter().any(|r| r == ty) {
        errs.push(JigError::new(
            format!("{base}/type"),
            JigErrorKind::Reserved,
            format!(
                "'{ty}' is reserved and not available in {}",
                ctx.catalog.name
            ),
        ));
        return;
    }
    let Some(prim) = ctx.catalog.primitives.get(ty) else {
        errs.push(JigError::new(
            format!("{base}/type"),
            JigErrorKind::UnknownPrimitive,
            format!("'{ty}' is not in catalog {}", ctx.catalog.name),
        ));
        return;
    };
    let item: Option<(&Value, &str)> = if ctx.scopes.outside.contains(id) {
        None
    } else {
        ctx.scopes.inside.get(id).map(|schema| {
            (
                schema,
                ctx.scopes.from.get(id).map(String::as_str).unwrap_or(""),
            )
        })
    };
    if ty == "List" {
        if ctx.scopes.nested.contains(id) {
            errs.push(JigError::new(
                format!("{base}/type"),
                JigErrorKind::BadBinding,
                "a List cannot sit inside a List",
            ));
        }
        if n.get("children").and_then(Value::as_array).map(Vec::len) != Some(1) {
            errs.push(JigError::new(
                format!("{base}/children"),
                JigErrorKind::BadShape,
                "a List has exactly one child, the item template",
            ));
        }
    }
    let props_empty = Map::new();
    let props = match n.get("props") {
        None => &props_empty,
        Some(Value::Object(m)) => m,
        Some(_) => {
            errs.push(JigError::new(
                format!("{base}/props"),
                JigErrorKind::BadShape,
                "props must be an object",
            ));
            &props_empty
        }
    };
    for (name, value) in props {
        let path = format!("{base}/props/{name}");
        match prim.props.iter().find(|p| &p.name == name) {
            None => errs.push(JigError::new(
                path,
                JigErrorKind::UnknownProp,
                format!("{ty} has no prop '{name}'"),
            )),
            Some(spec) => check_prop(ctx, spec, &path, value, item, errs),
        }
    }
    for spec in &prim.props {
        if spec.required && !props.contains_key(&spec.name) {
            errs.push(JigError::new(
                format!("{base}/props/{}", spec.name),
                JigErrorKind::MissingProp,
                format!("{ty} needs '{}'", spec.name),
            ));
        }
    }
    if let Some(children) = n.get("children") {
        if !prim.children {
            errs.push(JigError::new(
                format!("{base}/children"),
                JigErrorKind::ChildrenNotAllowed,
                format!("{ty} takes no children"),
            ));
        } else if let Some(list) = children.as_array() {
            for (i, child) in list.iter().enumerate() {
                match child.as_str() {
                    Some(c) if ctx.nodes.contains_key(c) => {}
                    Some(c) => errs.push(JigError::new(
                        format!("{base}/children/{i}"),
                        JigErrorKind::UnknownNode,
                        format!("child '{c}' is not a node"),
                    )),
                    None => errs.push(JigError::new(
                        format!("{base}/children/{i}"),
                        JigErrorKind::BadShape,
                        "a child is a node id",
                    )),
                }
            }
        } else {
            errs.push(JigError::new(
                format!("{base}/children"),
                JigErrorKind::BadShape,
                "children must be an array",
            ));
        }
    }
    for key in ["visible", "hidden"] {
        if let Some(b) = n.get(key) {
            check_binding(ctx, &format!("{base}/{key}"), b, "any", item, errs);
        }
    }
}

fn is_binding(v: &Value) -> bool {
    v.as_object().is_some_and(|m| {
        m.keys()
            .any(|k| k == "$data" || k == "$state" || k == "$item")
    })
}

fn check_prop(
    ctx: &Ctx,
    spec: &PropSpec,
    path: &str,
    value: &Value,
    item: Option<(&Value, &str)>,
    errs: &mut Vec<JigError>,
) {
    if spec.ty == "list" {
        if value.get("$data").is_some() {
            check_binding(ctx, path, value, "list", item, errs);
        } else {
            errs.push(JigError::new(
                path,
                JigErrorKind::WrongType,
                format!(
                    "'{}' takes a {{\"$data\": pointer}} binding to an array",
                    spec.name
                ),
            ));
        }
        return;
    }
    if spec.ty == "action" {
        if is_binding(value) {
            errs.push(JigError::new(
                path,
                JigErrorKind::WrongType,
                "an action cannot be bound",
            ));
        } else {
            check_action(ctx, path, value, errs);
        }
        return;
    }
    if is_binding(value) {
        check_binding(ctx, path, value, &spec.ty, item, errs);
        if spec.bind_state && value.get("$state").is_none() {
            errs.push(JigError::new(
                path,
                JigErrorKind::WrongType,
                format!("'{}' takes a {{\"$state\": name}} binding", spec.name),
            ));
        }
        return;
    }
    if spec.bind_state {
        errs.push(JigError::new(
            path,
            JigErrorKind::WrongType,
            format!("'{}' takes a {{\"$state\": name}} binding", spec.name),
        ));
        return;
    }
    if let Some(token) = spec.ty.strip_prefix("token:") {
        let ok = value.as_str().is_some_and(|s| {
            ctx.catalog
                .tokens
                .get(token)
                .is_some_and(|allowed| allowed.iter().any(|a| a == s))
        });
        if !ok {
            errs.push(JigError::new(
                path,
                JigErrorKind::BadToken,
                format!("not one of the {token} tokens"),
            ));
        }
        return;
    }
    let ok = match spec.ty.as_str() {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => is_integer(value),
        "boolean" => value.is_boolean(),
        "enum" => value
            .as_str()
            .is_some_and(|s| spec.values.iter().any(|v| v == s)),
        "stringList" => value
            .as_array()
            .is_some_and(|a| a.iter().all(Value::is_string)),
        "options" => value.as_array().is_some_and(|a| {
            a.iter().all(|o| {
                o.is_string()
                    || o.as_object().is_some_and(|m| {
                        m.get("value").is_some_and(Value::is_string)
                            && m.get("label").is_some_and(Value::is_string)
                    })
            })
        }),
        _ => false,
    };
    if !ok {
        errs.push(JigError::new(
            path,
            JigErrorKind::WrongType,
            format!("expected {}", spec.ty),
        ));
    }
}

fn is_integer(v: &Value) -> bool {
    v.as_i64().is_some() || v.as_u64().is_some() || v.as_f64().is_some_and(|f| f.fract() == 0.0)
}

/// Does a value of kind `actual` fit a prop of type `expected`?
fn fits(expected: &str, actual: &str) -> bool {
    match expected {
        "any" => true,
        "string" => matches!(actual, "string" | "number" | "integer" | "boolean"),
        "number" => matches!(actual, "number" | "integer"),
        "integer" => actual == "integer",
        "boolean" => actual == "boolean",
        "stringList" | "options" | "list" => actual == "array",
        e if e == "enum" || e.starts_with("token:") => actual == "string",
        _ => false,
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Null => "null",
    }
}

fn check_binding(
    ctx: &Ctx,
    path: &str,
    b: &Value,
    expected: &str,
    item: Option<(&Value, &str)>,
    errs: &mut Vec<JigError>,
) {
    let Some(m) = b.as_object().filter(|m| m.len() == 1) else {
        errs.push(JigError::new(
            path,
            JigErrorKind::BadBinding,
            "a binding has exactly one key, $data, $state or $item",
        ));
        return;
    };
    if let Some(ptr) = m.get("$item") {
        let Some(ptr) = ptr.as_str().filter(|p| p.starts_with('/')) else {
            errs.push(JigError::new(
                path,
                JigErrorKind::BadBinding,
                "$item is a JSON pointer starting with '/'",
            ));
            return;
        };
        let Some((element, list)) = item else {
            errs.push(JigError::new(
                path,
                JigErrorKind::BadBinding,
                "$item is only valid inside a List",
            ));
            return;
        };
        match resolve_schema(element, ptr) {
            None => errs.push(JigError::new(
                path,
                JigErrorKind::BadBinding,
                format!("'{ptr}' is not a path in the items of {list}"),
            )),
            Some(found) => {
                if let Some(t) = found.get("type").and_then(Value::as_str) {
                    if !fits(expected, t) {
                        errs.push(JigError::new(
                            path,
                            JigErrorKind::WrongType,
                            format!("'{ptr}' is a {t}, expected {expected}"),
                        ));
                    }
                }
            }
        }
    } else if let Some(name) = m.get("$state") {
        let Some(name) = name.as_str() else {
            errs.push(JigError::new(
                path,
                JigErrorKind::BadBinding,
                "$state is a name",
            ));
            return;
        };
        match ctx.state.get(name) {
            None => errs.push(JigError::new(
                path,
                JigErrorKind::BadBinding,
                format!("state '{name}' is not declared"),
            )),
            Some(default) if !fits(expected, kind_of(default)) => errs.push(JigError::new(
                path,
                JigErrorKind::WrongType,
                format!(
                    "state '{name}' holds a {}, expected {expected}",
                    kind_of(default)
                ),
            )),
            Some(_) => {}
        }
    } else if let Some(ptr) = m.get("$data") {
        let Some(ptr) = ptr.as_str().filter(|p| p.starts_with('/')) else {
            errs.push(JigError::new(
                path,
                JigErrorKind::BadBinding,
                "$data is a JSON pointer starting with '/'",
            ));
            return;
        };
        let Some(schema) = ctx.input_schema else {
            errs.push(JigError::new(
                path,
                JigErrorKind::BadBinding,
                format!("'{ptr}' is bound but the IR has no inputSchema"),
            ));
            return;
        };
        match resolve_schema(schema, ptr) {
            None => errs.push(JigError::new(
                path,
                JigErrorKind::BadBinding,
                format!("'{ptr}' is not a path in inputSchema"),
            )),
            Some(found) => {
                if let Some(t) = found.get("type").and_then(Value::as_str) {
                    if !fits(expected, t) {
                        errs.push(JigError::new(
                            path,
                            JigErrorKind::WrongType,
                            format!("'{ptr}' is a {t}, expected {expected}"),
                        ));
                    }
                }
            }
        }
    } else {
        errs.push(JigError::new(
            path,
            JigErrorKind::BadBinding,
            "a binding has exactly one key, $data, $state or $item",
        ));
    }
}

/// Walk a JSON pointer through `properties` (and `items` for an index).
fn resolve_schema<'a>(schema: &'a Value, ptr: &str) -> Option<&'a Value> {
    let mut cur = schema;
    for seg in ptr.split('/').skip(1) {
        cur = match cur.get("properties").and_then(|p| p.get(seg)) {
            Some(next) => next,
            None if seg.bytes().all(|b| b.is_ascii_digit()) => cur.get("items")?,
            None => return None,
        };
    }
    Some(cur)
}

fn check_action(ctx: &Ctx, path: &str, action: &Value, errs: &mut Vec<JigError>) {
    let Some(m) = action.as_object() else {
        errs.push(JigError::new(
            path,
            JigErrorKind::WrongType,
            "an action is an object",
        ));
        return;
    };
    let event = m.get("event").and_then(Value::as_str).unwrap_or("");
    if ctx.catalog.reserved.iter().any(|r| r == event) {
        errs.push(JigError::new(
            format!("{path}/event"),
            JigErrorKind::Reserved,
            format!(
                "'{event}' is reserved and not available in {}",
                ctx.catalog.name
            ),
        ));
        return;
    }
    let allowed: &[&str] = match event {
        "submit" => &["event", "with", "set"],
        "set" => &["event", "state", "value"],
        _ => {
            errs.push(JigError::new(
                format!("{path}/event"),
                JigErrorKind::UnknownAction,
                "the actions are 'submit' and 'set'",
            ));
            return;
        }
    };
    for key in m.keys() {
        if !allowed.contains(&key.as_str()) {
            errs.push(JigError::new(
                format!("{path}/{key}"),
                JigErrorKind::UnknownProp,
                format!("a '{event}' action has no field '{key}'"),
            ));
        }
    }
    let known_state = |name: &str, at: String, errs: &mut Vec<JigError>| {
        if !ctx.state.contains_key(name) {
            errs.push(JigError::new(
                at,
                JigErrorKind::BadBinding,
                format!("state '{name}' is not declared"),
            ));
        }
    };
    if event == "submit" {
        if let Some(with) = m.get("with") {
            match with.as_array() {
                Some(list) => {
                    for (i, w) in list.iter().enumerate() {
                        match w.as_str() {
                            Some(name) => known_state(name, format!("{path}/with/{i}"), errs),
                            None => errs.push(JigError::new(
                                format!("{path}/with/{i}"),
                                JigErrorKind::WrongType,
                                "a state name",
                            )),
                        }
                    }
                }
                None => errs.push(JigError::new(
                    format!("{path}/with"),
                    JigErrorKind::WrongType,
                    "with is a list of state names",
                )),
            }
        }
        if let Some(set) = m.get("set") {
            match set.as_object() {
                Some(map) => {
                    for name in map.keys() {
                        known_state(name, format!("{path}/set/{name}"), errs);
                    }
                }
                None => errs.push(JigError::new(
                    format!("{path}/set"),
                    JigErrorKind::WrongType,
                    "set is an object of state names to literals",
                )),
            }
        }
    } else {
        match m.get("state").and_then(Value::as_str) {
            Some(name) => known_state(name, format!("{path}/state"), errs),
            None => errs.push(JigError::new(
                format!("{path}/state"),
                JigErrorKind::MissingProp,
                "a 'set' action names a state",
            )),
        }
        if !m.contains_key("value") {
            errs.push(JigError::new(
                format!("{path}/value"),
                JigErrorKind::MissingProp,
                "a 'set' action has a value",
            ));
        }
    }
}

fn check_cycles(root: Option<&str>, nodes: &Map<String, Value>, errs: &mut Vec<JigError>) {
    fn visit(
        id: &str,
        nodes: &Map<String, Value>,
        stack: &mut Vec<String>,
        done: &mut Vec<String>,
        errs: &mut Vec<JigError>,
    ) {
        if done.iter().any(|d| d == id) {
            return;
        }
        if stack.iter().any(|s| s == id) {
            errs.push(JigError::new(
                format!("/nodes/{id}"),
                JigErrorKind::Cycle,
                "a node is its own ancestor",
            ));
            return;
        }
        stack.push(id.to_string());
        if let Some(children) = nodes
            .get(id)
            .and_then(|n| n.get("children"))
            .and_then(Value::as_array)
        {
            for child in children.iter().filter_map(Value::as_str) {
                if nodes.contains_key(child) {
                    visit(child, nodes, stack, done, errs);
                }
            }
        }
        stack.pop();
        done.push(id.to_string());
    }
    if let Some(root) = root.filter(|r| nodes.contains_key(*r)) {
        visit(root, nodes, &mut Vec::new(), &mut Vec::new(), errs);
    }
}

// ---------------------------------------------------------------------------
// Skill cards: a jig shipped with a skill, checked against a tool's schemas.

/// The reserved state variable an ask card's buttons set to `approve` / `deny`.
pub const DECISION: &str = "decision";

/// Check `data` against a tool's JSON schema (a card's data is the tool result).
pub fn check_data_against(schema: &Value, data: &Value) -> Result<(), Vec<JigError>> {
    let mut errs = Vec::new();
    validate_schema(schema, data, "", JigErrorKind::InvalidData, &mut errs);
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

/// Check a skill card: the IR with `schema` as its `inputSchema` (the tool's
/// output schema for a `show` card). A missing `jig` name is filled in.
pub fn check_card(ir: &Value, schema: &Value) -> Result<(), Vec<JigError>> {
    check_ir(&with_schema(ir, schema))
}

fn with_schema(ir: &Value, schema: &Value) -> Value {
    let mut ir = ir.clone();
    if let Some(obj) = ir.as_object_mut() {
        obj.insert("inputSchema".into(), schema.clone());
        obj.entry("jig").or_insert_with(|| json!("skill.card"));
    }
    ir
}

/// Check an ask card against the tool's input schema. `decision` is reserved:
/// it need not be declared (the host adds it), a declared one must be a string
/// defaulting to "", it is never in a submit's `with`, and the buttons set it
/// to `approve` or `deny`. Every `with` name is a top-level input property
/// whose type fits the state variable's default.
pub fn check_ask_card(ir: &Value, tool_input_schema: &Value) -> Result<(), Vec<JigError>> {
    let mut ir = with_schema(ir, tool_input_schema);
    let mut errs = Vec::new();
    if let Some(obj) = ir.as_object_mut() {
        let state = obj.entry("state").or_insert_with(|| json!({}));
        if let Some(state) = state.as_object_mut() {
            match state.get(DECISION) {
                None => {
                    state.insert(DECISION.into(), json!(""));
                }
                Some(v) if v == &json!("") => {}
                Some(_) => errs.push(JigError::new(
                    format!("/state/{DECISION}"),
                    JigErrorKind::WrongType,
                    "'decision' is reserved: declare it as a string defaulting to \"\" or leave it out",
                )),
            }
        }
    }
    if let Err(e) = check_ir(&ir) {
        errs.extend(e);
    }
    let state = ir.get("state").and_then(Value::as_object);
    let props = tool_input_schema.get("properties");
    for (id, node) in ir
        .get("nodes")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let Some(action) = node.pointer("/props/action") else {
            continue;
        };
        if action.get("event").and_then(Value::as_str) != Some("submit") {
            continue;
        }
        let base = format!("/nodes/{id}/props/action");
        if let Some(set) = action.get("set").and_then(Value::as_object) {
            if let Some(d) = set.get(DECISION) {
                if !matches!(d.as_str(), Some("approve" | "deny")) {
                    errs.push(JigError::new(
                        format!("{base}/set/{DECISION}"),
                        JigErrorKind::WrongType,
                        "decision is 'approve' or 'deny'",
                    ));
                }
            }
        }
        for (i, name) in action
            .get("with")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .enumerate()
        {
            let at = format!("{base}/with/{i}");
            if name == DECISION {
                errs.push(JigError::new(
                    at,
                    JigErrorKind::BadBinding,
                    "'decision' is reserved and set by the buttons, not submitted",
                ));
                continue;
            }
            match props.and_then(|p| p.get(name)) {
                None => errs.push(JigError::new(
                    at,
                    JigErrorKind::BadBinding,
                    format!("'{name}' is not an input of the tool"),
                )),
                Some(prop) => {
                    let ty = prop.get("type").and_then(Value::as_str);
                    let held = state.and_then(|s| s.get(name)).map(kind_of);
                    if let (Some(ty), Some(held)) = (ty, held) {
                        if ty != held && !(ty == "number" && held == "integer") {
                            errs.push(JigError::new(
                                at,
                                JigErrorKind::WrongType,
                                format!("state '{name}' holds a {held}, the tool input is a {ty}"),
                            ));
                        }
                    }
                }
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

/// Start state variables named like a tool argument at its value. A value of
/// another kind than the variable's default is ignored, and `decision` is
/// never set.
pub fn bind_args(ir: &Value, args: &Value) -> Value {
    let mut ir = ir.clone();
    let (Some(state), Some(args)) = (
        ir.get_mut("state").and_then(Value::as_object_mut),
        args.as_object(),
    ) else {
        return ir;
    };
    for (name, value) in args {
        if name == DECISION {
            continue;
        }
        if let Some(slot) = state.get_mut(name) {
            let same = kind_of(slot) == kind_of(value)
                || (matches!(kind_of(slot), "number" | "integer")
                    && matches!(kind_of(value), "number" | "integer"));
            if same {
                *slot = value.clone();
            }
        }
    }
    ir
}

// ---------------------------------------------------------------------------
// Schema subset (draft-07): the keywords the built-ins and gates use.

fn type_matches(ty: &str, v: &Value) -> bool {
    match ty {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.is_number() && is_integer(v),
        _ => true,
    }
}

fn validate_schema(
    schema: &Value,
    v: &Value,
    path: &str,
    kind: JigErrorKind,
    errs: &mut Vec<JigError>,
) {
    let Some(s) = schema.as_object() else {
        return;
    };
    macro_rules! err {
        ($($arg:tt)*) => {
            errs.push(JigError::new(path, kind, format!($($arg)*)))
        };
    }
    if let Some(t) = s.get("type") {
        let names: Vec<&str> = match t {
            Value::String(one) => vec![one.as_str()],
            Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|n| type_matches(n, v)) {
            err!("expected {}, got {}", names.join(" or "), kind_of(v));
            return;
        }
    }
    if let Some(Value::Array(allowed)) = s.get("enum") {
        if !allowed.contains(v) {
            let shown: Vec<String> = allowed.iter().map(|a| a.to_string()).collect();
            err!("must be one of {}", shown.join(", "));
        }
    }
    if let Some(c) = s.get("const") {
        if c != v {
            err!("must equal {c}");
        }
    }
    match v {
        Value::String(text) => {
            let len = text.chars().count() as u64;
            if let Some(max) = s.get("maxLength").and_then(Value::as_u64) {
                if len > max {
                    err!("longer than {max} characters");
                }
            }
            if let Some(min) = s.get("minLength").and_then(Value::as_u64) {
                if len < min {
                    err!("shorter than {min} characters");
                }
            }
            if let Some(format) = s.get("format").and_then(Value::as_str) {
                if let Err(e) = check_format(format, text) {
                    err!("{}", e);
                }
            }
        }
        Value::Number(n) => {
            if let (Some(min), Some(x)) = (s.get("minimum").and_then(Value::as_f64), n.as_f64()) {
                if x < min {
                    err!("below the minimum {min}");
                }
            }
            if let (Some(max), Some(x)) = (s.get("maximum").and_then(Value::as_f64), n.as_f64()) {
                if x > max {
                    err!("above the maximum {max}");
                }
            }
        }
        Value::Array(items) => {
            if let Some(max) = s.get("maxItems").and_then(Value::as_u64) {
                if items.len() as u64 > max {
                    err!("more than {max} items");
                }
            }
            if let Some(min) = s.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min {
                    err!("fewer than {min} items");
                }
            }
            if let Some(item_schema) = s.get("items") {
                for (i, item) in items.iter().enumerate() {
                    validate_schema(item_schema, item, &format!("{path}/{i}"), kind, errs);
                }
            }
        }
        Value::Object(map) => {
            let props = s.get("properties").and_then(Value::as_object);
            if let Some(Value::Array(required)) = s.get("required") {
                for name in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(name) {
                        errs.push(JigError::new(
                            format!("{path}/{name}"),
                            kind,
                            "required and missing",
                        ));
                    }
                }
            }
            for (name, value) in map {
                let child = format!("{path}/{name}");
                match props.and_then(|p| p.get(name)) {
                    Some(prop_schema) => validate_schema(prop_schema, value, &child, kind, errs),
                    None => match s.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            errs.push(JigError::new(child, kind, "not an accepted field"))
                        }
                        Some(extra @ Value::Object(_)) => {
                            validate_schema(extra, value, &child, kind, errs)
                        }
                        _ => {}
                    },
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// check_data, check_result

/// Check the data of a `show` / `ask_user` against a built-in's input schema.
pub fn check_data(jig: &str, data: &Value) -> Result<(), Vec<JigError>> {
    let b = builtin(jig)?;
    let mut errs = Vec::new();
    validate_schema(
        &b.input_schema,
        data,
        "",
        JigErrorKind::InvalidData,
        &mut errs,
    );
    if errs.is_empty() {
        if let Some(form) = &b.form {
            if let Some(schema) = data.pointer(&form.schema) {
                if let Err(e) = generate_fields(schema) {
                    errs.push(JigError::new(
                        format!("{}{}", form.schema, e.path),
                        JigErrorKind::InvalidData,
                        e.message,
                    ));
                }
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

/// Check a submitted answer for `jig`. `data` is what the card was shown with:
/// the offered items live there (a question's `choices`, a gate's schema).
///
/// - `vam.question`: `answer` is a string; with `choices` it is one of them.
/// - `vam.gate_default`, `vam.playbook_start`: the result is checked against
///   the schema in the data, so a selection holds only offered items.
/// - `vam.approve`: `decision` is `approve`, `deny`, `always` or `always_deny`.
///
/// `result` is what a `submit` carries: the names in the button action's `set`
/// together with the names in its `with` (the renderer merges both). A button
/// that only names `with` would leave `decision` out, so the built-in approve
/// card sets `decision` and sends `reason` with it.
/// - `vam.progress`: asks nothing, so only an empty object is accepted.
pub fn check_result(jig: &str, data: &Value, result: &Value) -> Result<(), Vec<JigError>> {
    let b = builtin(jig)?;
    let mut errs = Vec::new();
    match &b.result_from {
        Some(ptr) => match data.pointer(ptr) {
            Some(schema) if schema.is_object() => {
                validate_schema(schema, result, "", JigErrorKind::InvalidResult, &mut errs)
            }
            _ => errs.push(JigError::new(
                "",
                JigErrorKind::InvalidResult,
                format!("the card data has no schema at '{ptr}' to check the answer against"),
            )),
        },
        None => validate_schema(
            &b.result_schema,
            result,
            "",
            JigErrorKind::InvalidResult,
            &mut errs,
        ),
    }
    if errs.is_empty() && jig == "vam.question" {
        if let Some(Value::Array(choices)) = data.get("choices") {
            if let Some(answer) = result.get("answer") {
                if !choices.contains(answer) {
                    errs.push(JigError::new(
                        "/answer",
                        JigErrorKind::InvalidResult,
                        "not one of the offered choices",
                    ));
                }
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

// ---------------------------------------------------------------------------
// The generated form (vam.gate_default, vam.playbook_start)

struct GeneratedField {
    key: String,
    node: Value,
    default: Value,
}

fn generate_fields(schema: &Value) -> Result<Vec<GeneratedField>, JigError> {
    let bad = |path: String, message: String| JigError::new(path, JigErrorKind::BadShape, message);
    let Some(obj) = schema.as_object() else {
        return Err(bad(String::new(), "a form schema is an object".into()));
    };
    if obj
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t != "object")
    {
        return Err(bad("/type".into(), "a form schema has type object".into()));
    }
    let empty = Map::new();
    let props = obj
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required: Vec<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort_by_key(|k| (!required.contains(&k.as_str()), k.to_string()));
    let mut out = Vec::new();
    for key in keys {
        let prop = &props[key];
        let at = format!("/properties/{key}");
        let label = prop
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(key)
            .to_string();
        let ty = prop.get("type").and_then(Value::as_str).unwrap_or("");
        let format = prop.get("format").and_then(Value::as_str);
        let state_ref = json!({"$state": key});
        let default_of = |fallback: Value| prop.get("default").cloned().unwrap_or(fallback);
        let (node, default) = match ty {
            "string" if prop.get("enum").is_some_and(Value::is_array) => (
                json!({"type": "Select", "props": {"label": label, "value": state_ref, "options": prop["enum"]}}),
                default_of(json!("")),
            ),
            "string" if format == Some("date") => (
                json!({"type": "DateInput", "props": {"label": label, "value": state_ref}}),
                default_of(json!("")),
            ),
            "string" if format == Some("vam-workspace-file") => (
                json!({"type": "FileInput", "props": {"label": label, "value": state_ref}}),
                default_of(json!("")),
            ),
            "string" => {
                let long = prop.get("maxLength").and_then(Value::as_u64).unwrap_or(0) > 200;
                let primitive = if long { "TextArea" } else { "Input" };
                (
                    json!({"type": primitive, "props": {"label": label, "value": state_ref}}),
                    default_of(json!("")),
                )
            }
            "number" | "integer" => {
                let mut props_json = json!({"label": label, "value": state_ref});
                if let Some(min) = prop.get("minimum") {
                    props_json["min"] = min.clone();
                }
                if let Some(max) = prop.get("maximum") {
                    props_json["max"] = max.clone();
                }
                let floor = prop.get("minimum").cloned().unwrap_or(json!(0));
                (
                    json!({"type": "NumberInput", "props": props_json}),
                    default_of(floor),
                )
            }
            "boolean" => (
                json!({"type": "Toggle", "props": {"label": label, "value": state_ref}}),
                default_of(json!(false)),
            ),
            "array" => {
                let items = prop.get("items");
                let offered = items.and_then(|i| i.get("enum")).filter(|e| e.is_array());
                let strings = items
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "string");
                match (offered, strings) {
                    (Some(options), true) => (
                        json!({"type": "MultiSelect", "props": {"label": label, "value": state_ref, "options": options}}),
                        default_of(json!([])),
                    ),
                    _ => {
                        return Err(bad(
                            at,
                            "an array field needs string items with an enum".into(),
                        ))
                    }
                }
            }
            other => {
                return Err(bad(
                    at,
                    format!("a field of type '{other}' has no form element"),
                ))
            }
        };
        out.push(GeneratedField {
            key: key.clone(),
            node,
            default,
        });
    }
    Ok(out)
}

/// The IR of `vam.gate_default` or `vam.playbook_start` for one gate or
/// playbook: the static frame plus the form generated from the schema in
/// `data` (see `spec/jig/builtins/gate_default.json` for the rule).
pub fn form_ir(jig: &str, data: &Value) -> Result<Value, Vec<JigError>> {
    let b = builtin(jig)?;
    let Some(form) = &b.form else {
        return Err(vec![JigError::new(
            "/jig",
            JigErrorKind::UnknownJig,
            format!("'{jig}' has no generated form"),
        )]);
    };
    check_data(jig, data)?;
    let schema = data.pointer(&form.schema).unwrap_or(&Value::Null);
    let fields = generate_fields(schema).map_err(|e| vec![e])?;
    let mut ir = b.ir();
    let mut field_ids = Vec::new();
    let mut state = Map::new();
    let mut with = Vec::new();
    for f in fields {
        let id = format!("f_{}", f.key);
        ir["nodes"][&id] = f.node;
        field_ids.push(json!(id));
        state.insert(f.key.clone(), f.default);
        with.push(json!(f.key));
    }
    ir["nodes"][&form.into]["children"] = Value::Array(field_ids);
    ir["nodes"][&form.submit]["props"]["action"]["with"] = Value::Array(with);
    ir["state"] = Value::Object(state);
    Ok(ir)
}

#[cfg(test)]
mod tests;
