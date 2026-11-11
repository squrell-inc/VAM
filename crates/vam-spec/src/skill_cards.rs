//! Offline check of the cards a skill ships for its tools (`ToolSpec.ui`): `jigs/<name>.json`
//! in the package, checked against the tool's schemas. vamd calls it at publish and when it
//! seeds a session; `vam catalog validate` calls it offline. Same code, same messages.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::jig::{check_ask_card, check_card, JigError};
use crate::v1::{SkillManifest, ToolSpec};

/// Largest card file.
pub const MAX_CARD_FILE_BYTES: u64 = 64 * 1024;

/// One checked card: the IR with `jig` and `inputSchema` set, and the schema it was checked against
/// (the tool's output schema for `show`, its input schema for `ask`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    pub skill: String,
    pub jig: String,
    pub ir: Value,
    pub schema: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCards {
    /// The tool declares a `show` card, whether or not it passed the check.
    pub declares_show: bool,
    pub show: Option<ToolCard>,
    pub ask: Option<ToolCard>,
}

/// The last segment of a skill URN.
pub fn skill_name(manifest: &SkillManifest) -> String {
    manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.rsplit('/').next().unwrap_or_default().to_string())
        .unwrap_or_default()
}

fn problems(errors: &[JigError]) -> String {
    let shown: Vec<String> = errors.iter().take(3).map(|e| e.to_string()).collect();
    let more = if errors.len() > 3 {
        format!(" (and {} more)", errors.len() - 3)
    } else {
        String::new()
    };
    format!("{}{more}", shown.join("; "))
}

fn load_one(
    root: &Path,
    skill: &str,
    urn: &str,
    tool: &ToolSpec,
    name: &str,
    schema_json: &str,
    ask: bool,
) -> Result<ToolCard, String> {
    let file = format!("jigs/{name}.json");
    let fail = |why: String| format!("tool '{}': card {file}: {why}", tool.name);
    let path = root.join(&file);
    let meta = fs::symlink_metadata(&path)
        .map_err(|_| fail("the package has no such file".to_string()))?;
    if !meta.is_file() {
        return Err(fail("not a regular file".to_string()));
    }
    if meta.len() > MAX_CARD_FILE_BYTES {
        return Err(fail(format!("larger than {MAX_CARD_FILE_BYTES} bytes")));
    }
    let raw = fs::read_to_string(&path).map_err(|e| fail(format!("unreadable: {e}")))?;
    let mut ir: Value = serde_json::from_str(&raw).map_err(|e| fail(format!("not JSON: {e}")))?;
    if !ir.is_object() {
        return Err(fail("not a card (the IR is one JSON object)".to_string()));
    }
    let schema: Value = if schema_json.trim().is_empty() {
        json!({"type": "object"})
    } else {
        serde_json::from_str(schema_json).map_err(|e| {
            format!(
                "tool '{}': {} is not JSON: {e}",
                tool.name,
                if ask {
                    "inputSchemaJson"
                } else {
                    "outputSchemaJson"
                }
            )
        })?
    };
    let checked = if ask {
        check_ask_card(&ir, &schema)
    } else {
        check_card(&ir, &schema)
    };
    checked.map_err(|errs| fail(problems(&errs)))?;
    let jig = format!("{skill}.{name}");
    if let Some(obj) = ir.as_object_mut() {
        obj.insert("inputSchema".into(), schema.clone());
        obj.insert("jig".into(), json!(jig));
    }
    Ok(ToolCard {
        skill: urn.to_string(),
        jig,
        ir,
        schema,
    })
}

/// Every tool's cards, checked. The first problem is the error.
pub fn check_package_cards(
    root: &Path,
    manifest: &SkillManifest,
) -> Result<HashMap<String, ToolCards>, String> {
    let skill = skill_name(manifest);
    let urn = manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.clone())
        .unwrap_or_default();
    let mut out = HashMap::new();
    for tool in &manifest.tools {
        let Some(ui) = tool.ui.as_ref() else { continue };
        let mut cards = ToolCards {
            declares_show: !ui.show.is_empty(),
            ..Default::default()
        };
        if !ui.show.is_empty() {
            cards.show = Some(load_one(
                root,
                &skill,
                &urn,
                tool,
                &ui.show,
                &tool.output_schema_json,
                false,
            )?);
        }
        if !ui.ask.is_empty() {
            cards.ask = Some(load_one(
                root,
                &skill,
                &urn,
                tool,
                &ui.ask,
                &tool.input_schema_json,
                true,
            )?);
        }
        if cards.show.is_some() || cards.ask.is_some() {
            out.insert(tool.name.clone(), cards);
        }
    }
    Ok(out)
}
