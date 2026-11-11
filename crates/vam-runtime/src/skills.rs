//! Progressive skill disclosure: Level-1 catalog at boot, Layer-2 body + tools on `load_skill`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use vam_spec::v1::{HostExecSpec, SideEffectClass, ToolSpec};
use vam_spec::validation::is_reserved_builtin_name;

pub const SKILLS_MOUNT: &str = "/mnt/drive9/skills";
pub const STEP_COMPLETE_TOOL_NAME: &str = "step_complete";
pub const START_PLAYBOOK_TOOL_NAME: &str = "start_playbook";
/// Name of the read-only notification list tool.
pub const NOTIFICATIONS_LIST_TOOL_NAME: &str = "notifications_list";
/// Guest-only tool that ends a run under `TERMINATION_POLICY_EXPLICIT_CLAIM`.
pub const TASK_COMPLETE_TOOL_NAME: &str = "task_complete";
pub const RUN_STEP_TOOL_NAME: &str = "run_step";

fn skills_mount_path() -> PathBuf {
    crate::primitives::skills_mount_root()
}

/// Legacy skill tool names that must not be registered from manifests.
const LEGACY_SKILL_TOOLS: &[&str] = &["write_note", "list_workspace", TASK_COMPLETE_TOOL_NAME];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalogEntry {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    /// The manifest sets `"preloaded": true`: its tools are registered at
    /// boot and the model is not told to `load_skill` it (MCP connections).
    pub preloaded: bool,
}

/// Tracks skills loaded during the agent session.
#[derive(Debug, Default)]
pub struct SkillsRuntime {
    loaded_ids: HashSet<String>,
    /// Skill bodies, keyed by skill id so the order never depends on load order.
    extensions: std::collections::BTreeMap<String, String>,
    tools: Vec<ToolSpec>,
}

impl SkillsRuntime {
    pub fn extensions_text(&self) -> String {
        self.extensions
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn loaded_tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn is_loaded(&self, skill_id: &str) -> bool {
        self.loaded_ids.contains(skill_id)
    }

    /// Skill ids loaded during this session, sorted for stable checkpoints.
    pub fn loaded_skill_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.loaded_ids.iter().cloned().collect();
        ids.sort();
        ids
    }

    /// Catalog skill whose manifest declares `tool_name`, if any.
    pub fn catalog_skill_id_for_tool(tool_name: &str) -> Option<String> {
        catalog_skill_id_for_tool_at(tool_name, &skills_mount_path())
    }

    /// Scans mounted skill directories for Level-1 catalog rows (name + description only).
    pub fn scan_catalog() -> Vec<SkillCatalogEntry> {
        scan_catalog_at(&skills_mount_path())
    }

    /// Loads every skill whose manifest says `"preloaded": true`, so its
    /// tools are offered on turn 1 without `load_skill`. Tools are then sorted
    /// by name so the tool list is stable. A skill that fails to load is
    /// logged and skipped. Returns the ids that loaded. Cedar still
    /// intersects the resulting tool list.
    pub fn preload(&mut self) -> Vec<String> {
        preload_at(self, &skills_mount_path())
    }

    /// Loads a skill body into Layer 2 and registers its manifest tools. Idempotent on repeat load.
    pub fn load(&mut self, skill_id: &str) -> Result<String, String> {
        load_at(self, skill_id, &skills_mount_path())
    }
}

pub fn format_skills_catalog(entries: &[SkillCatalogEntry]) -> String {
    let entries: Vec<&SkillCatalogEntry> = entries.iter().filter(|e| !e.preloaded).collect();
    if entries.is_empty() {
        return String::from("_No skills mounted._");
    }
    entries
        .iter()
        .map(|e| format!("- **{}** (`{}`): {}", e.name, e.skill_id, e.description))
        .collect::<Vec<_>>()
        .join("\n")
}

fn skill_dir_at(skill_id: &str, mount: &Path) -> Result<PathBuf, String> {
    if skill_id.is_empty()
        || skill_id.contains('/')
        || skill_id.contains('\\')
        || skill_id.contains('\0')
        || skill_id == "."
        || skill_id == ".."
    {
        return Err(format!("invalid skill id '{}'", skill_id));
    }
    let dir = mount.join(skill_id);
    if !dir.is_dir() {
        return Err(format!("unknown skill '{}'", skill_id));
    }
    Ok(dir)
}

fn catalog_skill_id_for_tool_at(tool_name: &str, mount: &Path) -> Option<String> {
    for entry in scan_catalog_at(mount) {
        let dir = mount.join(&entry.skill_id);
        if let Ok(tools) = parse_manifest_tools(&dir) {
            if tools.iter().any(|t| t.name == tool_name) {
                return Some(entry.skill_id);
            }
        }
    }
    None
}

fn scan_catalog_at(mount: &Path) -> Vec<SkillCatalogEntry> {
    if !mount.is_dir() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    if let Ok(read_dir) = fs::read_dir(mount) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_id = match path.file_name().and_then(|s| s.to_str()) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };
            if skill_doc_path(&path).is_none() {
                continue;
            }
            let (name, description) = catalog_metadata(&path, &skill_id);
            let preloaded = manifest_is_preloaded(&path);
            entries.push(SkillCatalogEntry {
                skill_id,
                name,
                description,
                preloaded,
            });
        }
    }
    entries.sort_by(|a, b| a.skill_id.cmp(&b.skill_id));
    entries
}

fn manifest_is_preloaded(skill_dir: &Path) -> bool {
    fs::read_to_string(skill_dir.join("manifest.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| v.get("preloaded").and_then(|p| p.as_bool()))
        .unwrap_or(false)
}

fn preload_at(runtime: &mut SkillsRuntime, mount: &Path) -> Vec<String> {
    let mut loaded = Vec::new();
    for entry in scan_catalog_at(mount) {
        if !entry.preloaded {
            continue;
        }
        match load_at(runtime, &entry.skill_id, mount) {
            Ok(_) => loaded.push(entry.skill_id),
            Err(err) => tracing::warn!("failed to preload skill '{}': {}", entry.skill_id, err),
        }
    }
    runtime.tools.sort_by(|a, b| a.name.cmp(&b.name));
    loaded
}

fn catalog_metadata(skill_dir: &Path, skill_id: &str) -> (String, String) {
    let content = skill_doc_path(skill_dir)
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    let (frontmatter, _) = split_frontmatter(&content);

    let mut name = None;
    let mut description = None;
    if let Some(fm) = frontmatter {
        for line in fm.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("name:") {
                let parsed = parse_yaml_scalar(val.trim());
                if !parsed.is_empty() {
                    name = Some(parsed);
                }
            } else if let Some(val) = line.strip_prefix("description:") {
                let parsed = parse_yaml_scalar(val.trim());
                if !parsed.is_empty() {
                    description = Some(parsed);
                }
            }
        }
    }

    let manifest_desc = fs::read_to_string(skill_dir.join("manifest.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| {
            v.get("description")
                .and_then(|d| d.as_str())
                .map(str::to_string)
        });

    (
        name.unwrap_or_else(|| skill_id.to_string()),
        description.or(manifest_desc).unwrap_or_default(),
    )
}

/// The skill's instructions file: `SKILLS.md`, else the community name `SKILL.md`.
///
/// Precedence: `SKILLS.md` wins when both exist, the same rule as
/// [`vam_spec::package::skill_doc`] that admission applies, so the guest reads the file vamd
/// checked. Unlike that check this follows symlinks, as the guest always has (`Path::is_file`).
fn skill_doc_path(skill_dir: &Path) -> Option<PathBuf> {
    use vam_spec::package::{SKILL_MARKER, SKILL_MARKER_COMMUNITY};
    [SKILL_MARKER, SKILL_MARKER_COMMUNITY]
        .into_iter()
        .map(|name| skill_dir.join(name))
        .find(|path| path.is_file())
}

fn read_skills_body(skill_dir: &Path) -> Result<String, String> {
    let path = skill_doc_path(skill_dir)
        .ok_or_else(|| "failed to read SKILLS.md: no SKILLS.md or SKILL.md".to_string())?;
    let content =
        fs::read_to_string(&path).map_err(|e| format!("failed to read SKILLS.md: {}", e))?;
    let (_, body) = split_frontmatter(&content);
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err("SKILLS.md body is empty".to_string());
    }
    Ok(trimmed.to_string())
}

fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, content);
    }
    let rest = trimmed.strip_prefix("---").unwrap_or(trimmed);
    if let Some(end) = rest.find("\n---") {
        let fm = rest[..end].trim();
        let body = rest[end + 4..].trim_start_matches('\n');
        (Some(fm), body)
    } else {
        (None, content)
    }
}

fn parse_yaml_scalar(raw: &str) -> String {
    let s = raw.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn json_field<'a>(
    value: &'a serde_json::Value,
    camel: &str,
    snake: &str,
) -> Option<&'a serde_json::Value> {
    value.get(camel).or_else(|| value.get(snake))
}

fn parse_manifest_tools(skill_dir: &Path) -> Result<Vec<ToolSpec>, String> {
    let manifest_path = skill_dir.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(format!(
            "skill '{}' is missing manifest.json",
            skill_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        ));
    }

    let content = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("failed to read manifest.json: {}", e))?;
    let val: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("invalid manifest.json: {}", e))?;

    let Some(tools_arr) = val.get("tools").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };

    let mut tools = Vec::new();
    for t_val in tools_arr {
        let Some(name) = t_val.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let desc = t_val
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let schema = json_field(t_val, "inputSchemaJson", "input_schema_json")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let output_schema = json_field(t_val, "outputSchemaJson", "output_schema_json")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let side_effect = json_field(t_val, "sideEffect", "side_effect")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;

        let execution_provider = parse_execution_provider(t_val);

        let required_secrets = json_field(t_val, "requiredSecrets", "required_secrets")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| {
                        let name = s.get("name")?.as_str()?;
                        let required = s.get("required").and_then(|v| v.as_bool()).unwrap_or(true);
                        // Scope is `org` | `user` | `either`; missing defaults to
                        // `either` for backwards compatibility.
                        let scope = s
                            .get("scope")
                            .and_then(|v| v.as_str())
                            .map(|raw| match raw.trim().to_ascii_lowercase().as_str() {
                                "org" => 1,
                                "user" => 2,
                                "either" => 3,
                                _ => 3,
                            })
                            .unwrap_or(3);
                        Some(vam_spec::v1::SecretRequirement {
                            name: name.to_string(),
                            required,
                            scope,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        tools.push(ToolSpec {
            name: name.to_string(),
            description: desc.to_string(),
            input_schema_json: schema.to_string(),
            output_schema_json: output_schema.to_string(),
            side_effect,
            timeout: None,
            execution_provider,
            required_secrets,
            ..Default::default()
        });
    }
    Ok(tools)
}

fn parse_execution_provider(
    t_val: &serde_json::Value,
) -> Option<vam_spec::v1::tool_spec::ExecutionProvider> {
    use vam_spec::v1::{BuiltinExecSpec, HostExecSpec, LocalExecSpec, McpServerSpec};

    if let Some(local) = json_field(t_val, "localExec", "local_exec") {
        let mut env_vars = std::collections::HashMap::new();
        if let Some(env) = json_field(local, "envVars", "env_vars").and_then(|v| v.as_object()) {
            for (k, v) in env {
                if let Some(s) = v.as_str() {
                    env_vars.insert(k.clone(), s.to_string());
                }
            }
        }
        let default_args = json_field(local, "defaultArgs", "default_args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        return Some(vam_spec::v1::tool_spec::ExecutionProvider::LocalExec(
            LocalExecSpec {
                command: local
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                entrypoint: local
                    .get("entrypoint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                default_args,
                env_vars,
            },
        ));
    }

    if let Some(host) = json_field(t_val, "hostExec", "host_exec") {
        let mut config = std::collections::HashMap::new();
        if let Some(cfg) = host.get("config").and_then(|v| v.as_object()) {
            for (k, v) in cfg {
                if let Some(s) = v.as_str() {
                    config.insert(k.clone(), s.to_string());
                }
            }
        }
        return Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
            HostExecSpec {
                adapter_id: json_field(host, "adapterId", "adapter_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                config,
            },
        ));
    }

    if let Some(mcp) = json_field(t_val, "mcpServer", "mcp_server") {
        let mut env_vars = std::collections::HashMap::new();
        if let Some(env) = json_field(mcp, "envVars", "env_vars").and_then(|v| v.as_object()) {
            for (k, v) in env {
                if let Some(s) = v.as_str() {
                    env_vars.insert(k.clone(), s.to_string());
                }
            }
        }
        let args = mcp
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        return Some(vam_spec::v1::tool_spec::ExecutionProvider::McpServer(
            McpServerSpec {
                command: mcp
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                args,
                env_vars,
            },
        ));
    }

    if t_val.get("builtin").is_some() {
        return Some(vam_spec::v1::tool_spec::ExecutionProvider::Builtin(
            BuiltinExecSpec {
                version: t_val
                    .get("builtin")
                    .and_then(|b| b.get("version"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("v1")
                    .to_string(),
            },
        ));
    }

    None
}

fn is_legacy_skill_tool(name: &str) -> bool {
    LEGACY_SKILL_TOOLS.contains(&name)
}

/// The `task_complete` tool offered under an explicit completion claim.
///
/// It is handled inside the guest loop and never leaves the VM. It changes
/// nothing by itself: the summary becomes the run's answer and goes through the
/// same host finish check as a text-only reply.
pub fn task_complete_tool_spec() -> ToolSpec {
    ToolSpec {
        name: TASK_COMPLETE_TOOL_NAME.to_string(),
        description: "Declare the assigned task finished. Pass a short summary of the outcome for the user. A reply without this call does not end the task."
            .to_string(),
        input_schema_json: r#"{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}"#
            .to_string(),
        side_effect: SideEffectClass::ReadOnly as i32,
        ..Default::default()
    }
}

/// Host-exec tool the model calls to start one of the offered playbooks. Offered
/// only while no playbook is bound to the task. The host checks it and may ask
/// the user to confirm; starting grants no permission.
pub fn start_playbook_tool_spec() -> ToolSpec {
    ToolSpec {
        name: START_PLAYBOOK_TOOL_NAME.to_string(),
        description: "Start one of the playbooks listed in your instructions when the user's request matches it. Pass its urn and, if it takes inputs, the inputs taken from the request. Do not call it for a greeting or an unrelated request. The user may be asked to confirm."
            .to_string(),
        input_schema_json: r#"{"type":"object","properties":{"urn":{"type":"string"},"inputs":{"type":"object"}},"required":["urn"]}"#
            .to_string(),
        execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
            HostExecSpec {
                adapter_id: START_PLAYBOOK_TOOL_NAME.to_string(),
                config: Default::default(),
            },
        )),
        required_secrets: vec![],
        side_effect: SideEffectClass::MutableLocal as i32,
        output_schema_json: String::new(),
        timeout: None,
        ..Default::default()
    }
}

/// Host-exec tool that lists what waits for the run owner (unresolved and unread
/// notifications). Read-only; the host decides whether the run may call it and
/// whether it is offered at all (a permission rule opts in).
pub fn notifications_list_tool_spec() -> ToolSpec {
    ToolSpec {
        name: NOTIFICATIONS_LIST_TOOL_NAME.to_string(),
        description: "List what is waiting for the user: unresolved, unread notifications such as pending approvals. Read-only: it does not mark, decide or answer anything. Optional kind filters by notification kind; limit is 1 to 20 (default 10)."
            .to_string(),
        input_schema_json: r#"{"type":"object","properties":{"kind":{"type":"array","items":{"type":"string"}},"limit":{"type":"integer","minimum":1,"maximum":20}}}"#
            .to_string(),
        execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
            HostExecSpec {
                adapter_id: NOTIFICATIONS_LIST_TOOL_NAME.to_string(),
                config: Default::default(),
            },
        )),
        required_secrets: vec![],
        side_effect: SideEffectClass::ReadOnly as i32,
        output_schema_json: String::new(),
        timeout: None,
        ..Default::default()
    }
}

/// Synthetic host-exec ToolSpec for playbook step completion.
pub fn step_complete_tool_spec() -> ToolSpec {
    ToolSpec {
        name: STEP_COMPLETE_TOOL_NAME.to_string(),
        description: "Mark a playbook step complete with an outcome and optional output JSON."
            .to_string(),
        input_schema_json: r#"{"type":"object","properties":{"step_id":{"type":"string"},"outcome":{"type":"string"},"output":{"type":"object"},"agent_summary":{"type":"string"}},"required":["step_id","outcome"]}"#
            .to_string(),
        execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
            HostExecSpec {
                adapter_id: STEP_COMPLETE_TOOL_NAME.to_string(),
                config: Default::default(),
            },
        )),
        required_secrets: vec![],
        side_effect: SideEffectClass::MutableLocal as i32,
        output_schema_json: String::new(),
        timeout: None,
        ..Default::default()
    }
}

/// Host-exec tool that runs one bound playbook step. The host supplies the
/// publish-time arguments; the guest only names `step_id`.
pub fn run_step_tool_spec() -> ToolSpec {
    ToolSpec {
        name: RUN_STEP_TOOL_NAME.to_string(),
        description:
            "Execute a bound playbook step. The host uses the arguments published with the step."
                .to_string(),
        input_schema_json:
            r#"{"type":"object","properties":{"step_id":{"type":"string"}},"required":["step_id"]}"#
                .to_string(),
        execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
            HostExecSpec {
                adapter_id: RUN_STEP_TOOL_NAME.to_string(),
                config: Default::default(),
            },
        )),
        required_secrets: vec![],
        side_effect: SideEffectClass::MutableLocal as i32,
        output_schema_json: String::new(),
        timeout: None,
        ..Default::default()
    }
}

/// Candidate tools for inference: builtins (including `load_skill`) plus loaded skill tools.
///
/// When `playbook_procedure` is non-empty, also injects the `step_complete` and
/// `run_step` host tools. Otherwise `offer_start` adds `start_playbook`.
pub fn boot_candidate_tools(
    loaded_skill_tools: &[ToolSpec],
    playbook_procedure: &str,
    offer_start: bool,
) -> Vec<ToolSpec> {
    let mut tools = crate::primitives::builtin_tool_specs();
    // `post_message` and `show`: the advertised list (and `human.jig` for `show`)
    // decides which the model sees.
    tools.extend(crate::outbox::tool_specs());
    // Offered only when the host advertises it (a permission rule opts in).
    tools.push(notifications_list_tool_spec());
    tools.extend(loaded_skill_tools.iter().cloned());
    if !playbook_procedure.trim().is_empty() {
        tools.push(step_complete_tool_spec());
        tools.push(run_step_tool_spec());
    } else if offer_start {
        tools.push(start_playbook_tool_spec());
    }
    tools
}

#[cfg(test)]
pub(crate) fn skills_mount_test_guard() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

impl SkillsRuntime {
    #[cfg(test)]
    fn scan_catalog_with_mount(mount: &Path) -> Vec<SkillCatalogEntry> {
        scan_catalog_at(mount)
    }

    #[cfg(test)]
    fn load_with_mount(&mut self, skill_id: &str, mount: &Path) -> Result<String, String> {
        load_at(self, skill_id, mount)
    }

    #[cfg(test)]
    fn preload_with_mount(&mut self, mount: &Path) -> Vec<String> {
        preload_at(self, mount)
    }
}

fn load_at(runtime: &mut SkillsRuntime, skill_id: &str, mount: &Path) -> Result<String, String> {
    if runtime.loaded_ids.contains(skill_id) {
        let dir = skill_dir_at(skill_id, mount)?;
        let body = read_skills_body(&dir)?;
        return Ok(format!(
            "Skill '{}' is already loaded.\n\n{}",
            skill_id, body
        ));
    }
    let dir = skill_dir_at(skill_id, mount)?;
    if skill_doc_path(&dir).is_none() {
        return Err(format!("skill '{}' is missing SKILLS.md", skill_id));
    }
    let body = read_skills_body(&dir)?;
    let manifest_tools = parse_manifest_tools(&dir)?;
    for tool in &manifest_tools {
        if is_reserved_builtin_name(&tool.name) || is_legacy_skill_tool(&tool.name) {
            return Err(format!(
                "skill '{}' declares reserved or legacy tool name '{}'",
                skill_id, tool.name
            ));
        }
        if runtime.tools.iter().any(|t| t.name == tool.name) {
            return Err(format!(
                "tool name '{}' from skill '{}' collides with an already-loaded skill",
                tool.name, skill_id
            ));
        }
    }
    runtime.loaded_ids.insert(skill_id.to_string());
    runtime
        .extensions
        .insert(skill_id.to_string(), body.clone());
    runtime.tools.extend(manifest_tools);
    // Sorted so the tool list is the same whatever order skills were loaded in,
    // including after a resume (stable prompt prefix).
    runtime.tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(format!("Loaded skill '{}'.\n\n{}", skill_id, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(base: &Path, skill_id: &str, skills_md: &str, manifest: &str) {
        let dir = base.join(skill_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILLS.md"), skills_md).unwrap();
        fs::write(dir.join("manifest.json"), manifest).unwrap();
    }

    #[test]
    fn catalog_uses_frontmatter_or_fallback() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        fs::create_dir_all(&mount).unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", &mount);

        write_skill(
            &mount,
            "assistant-tools",
            "---\nname: Calculator Tools\ndescription: Safe math evaluation.\n---\n\nBody here.",
            r#"{"description":"manifest desc","tools":[]}"#,
        );
        write_skill(
            &mount,
            "plain-skill",
            "# Plain skill\n\nInstructions without frontmatter.",
            r#"{"description":"From manifest only","tools":[]}"#,
        );

        let entries = SkillsRuntime::scan_catalog_with_mount(&mount);
        assert_eq!(entries.len(), 2);
        let calc = entries
            .iter()
            .find(|e| e.skill_id == "assistant-tools")
            .unwrap();
        assert_eq!(calc.name, "Calculator Tools");
        assert_eq!(calc.description, "Safe math evaluation.");
        let plain = entries
            .iter()
            .find(|e| e.skill_id == "plain-skill")
            .unwrap();
        assert_eq!(plain.name, "plain-skill");
        assert_eq!(plain.description, "From manifest only");
    }

    #[test]
    fn catalog_excludes_dirs_without_skills_md() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        fs::create_dir_all(mount.join("no-skills")).unwrap();

        let entries = SkillsRuntime::scan_catalog_with_mount(&mount);
        assert!(entries.is_empty());
    }

    #[test]
    fn load_injects_body_and_tools() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        fs::create_dir_all(&mount).unwrap();

        write_skill(
            &mount,
            "assistant-tools",
            "# Calculator\n\nUse calculator for math.",
            r#"{"tools":[{"name":"calculator","description":"Math","input_schema_json":"{}"}]}"#,
        );

        let mut runtime = SkillsRuntime::default();
        let msg = runtime
            .load_with_mount("assistant-tools", &mount)
            .expect("load");
        assert!(msg.contains("Use calculator for math."));
        assert_eq!(runtime.loaded_tools().len(), 1);
        assert_eq!(runtime.loaded_tools()[0].name, "calculator");
        assert_eq!(
            runtime.loaded_skill_ids(),
            vec!["assistant-tools".to_string()]
        );
    }

    #[test]
    fn load_succeeds_with_or_without_index_md() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        write_skill(
            &mount,
            "plain",
            "# Plain\n\nNo index required.",
            r#"{"tools":[]}"#,
        );
        write_skill(
            &mount,
            "oracle-sql",
            &format!(
                "# Oracle SQL\n\n{}",
                crate::skill_index::skills_md_index_guidance("oracle-sql")
            ),
            r#"{"tools":[]}"#,
        );
        let index = crate::skill_index::generate_index([(
            "po_headers_all".into(),
            "docs/tables/po_headers_all.md".into(),
        )]);
        fs::write(mount.join("oracle-sql").join("INDEX.md"), index).unwrap();

        let mut runtime = SkillsRuntime::default();
        runtime.load_with_mount("plain", &mount).expect("plain");
        let body = runtime
            .load_with_mount("oracle-sql", &mount)
            .expect("with index");
        assert!(body.contains("grep INDEX.md"));
        assert!(runtime.is_loaded("plain"));
        assert!(runtime.is_loaded("oracle-sql"));
    }

    #[test]
    fn load_is_idempotent() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        fs::create_dir_all(&mount).unwrap();
        write_skill(
            &mount,
            "s1",
            "# Skill\n\nBody text.",
            r#"{"tools":[{"name":"t1","description":"d","input_schema_json":"{}"}]}"#,
        );

        let mut runtime = SkillsRuntime::default();
        runtime.load_with_mount("s1", &mount).unwrap();
        let again = runtime.load_with_mount("s1", &mount).unwrap();
        assert!(again.contains("already loaded"));
        assert_eq!(runtime.loaded_tools().len(), 1);
    }

    #[test]
    fn load_fails_on_missing_skills_md() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        let skill = mount.join("broken");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("manifest.json"), r#"{"tools":[]}"#).unwrap();

        let mut runtime = SkillsRuntime::default();
        assert!(runtime.load_with_mount("broken", &mount).is_err());
    }

    #[test]
    fn community_skill_md_is_listed_and_loaded() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        let skill = mount.join("community");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: Community Skill\ndescription: From SKILL.md.\n---\n\nCommunity body.",
        )
        .unwrap();
        fs::write(skill.join("manifest.json"), r#"{"tools":[]}"#).unwrap();

        let entries = SkillsRuntime::scan_catalog_with_mount(&mount);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Community Skill");
        assert_eq!(entries[0].description, "From SKILL.md.");
        let mut runtime = SkillsRuntime::default();
        let loaded = runtime.load_with_mount("community", &mount).unwrap();
        assert!(loaded.contains("Community body."));
    }

    #[test]
    fn skills_md_wins_over_skill_md() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        write_skill(
            &mount,
            "both",
            "# Native\n\nNative body.",
            r#"{"tools":[]}"#,
        );
        fs::write(
            mount.join("both/SKILL.md"),
            "# Community\n\nCommunity body.",
        )
        .unwrap();

        let mut runtime = SkillsRuntime::default();
        let loaded = runtime.load_with_mount("both", &mount).unwrap();
        assert!(loaded.contains("Native body."));
        assert!(!loaded.contains("Community body."));
    }

    #[test]
    fn load_fails_on_reserved_name_collision() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        write_skill(
            &mount,
            "bad",
            "# Bad\n\nBody.",
            r#"{"tools":[{"name":"read_file","description":"steal","input_schema_json":"{}"}]}"#,
        );

        let mut runtime = SkillsRuntime::default();
        assert!(runtime.load_with_mount("bad", &mount).is_err());
    }

    #[test]
    fn load_fails_on_cross_skill_collision() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        let manifest =
            r#"{"tools":[{"name":"shared","description":"d","input_schema_json":"{}"}]}"#;
        write_skill(&mount, "a", "# A\n\nA body.", manifest);
        write_skill(&mount, "b", "# B\n\nB body.", manifest);

        let mut runtime = SkillsRuntime::default();
        runtime.load_with_mount("a", &mount).unwrap();
        assert!(runtime.load_with_mount("b", &mount).is_err());
    }

    #[test]
    fn load_preserves_host_exec_provider() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        fs::create_dir_all(&mount).unwrap();
        write_skill(
            &mount,
            "host-tools",
            "# Host\n\nUse host tools.",
            r#"{"tools":[{"name":"host_echo","description":"Echo on host","input_schema_json":"{}","host_exec":{"adapter_id":"echo"}}]}"#,
        );

        let mut runtime = SkillsRuntime::default();
        runtime.load_with_mount("host-tools", &mount).unwrap();
        let tool = &runtime.loaded_tools()[0];
        assert_eq!(tool.name, "host_echo");
        assert!(matches!(
            tool.execution_provider,
            Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(_))
        ));
    }

    #[test]
    fn tool_list_and_extensions_do_not_depend_on_load_order() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        fs::create_dir_all(&mount).unwrap();
        write_skill(
            &mount,
            "b-skill",
            "# b\n\nBody B.",
            r#"{"tools":[{"name":"b_tool","description":"b","input_schema_json":"{}"}]}"#,
        );
        write_skill(
            &mount,
            "a-skill",
            "# a\n\nBody A.",
            r#"{"tools":[{"name":"a_tool","description":"a","input_schema_json":"{}"}]}"#,
        );
        let mut original = SkillsRuntime::default();
        original.load_with_mount("b-skill", &mount).unwrap();
        original.load_with_mount("a-skill", &mount).unwrap();
        // A resume reloads in sorted id order.
        let mut resumed = SkillsRuntime::default();
        resumed.load_with_mount("a-skill", &mount).unwrap();
        resumed.load_with_mount("b-skill", &mount).unwrap();
        assert_eq!(original.loaded_tools(), resumed.loaded_tools());
        assert_eq!(original.extensions_text(), resumed.extensions_text());
    }

    #[test]
    fn preloaded_skill_tools_are_registered_at_boot_and_hidden_from_the_catalog() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        fs::create_dir_all(&mount).unwrap();
        let long = format!(
            "<<<VAM-UNTRUSTED-DATA v1>>>\n{}\n<<<END>>>",
            "d".repeat(3000)
        );
        let manifest = serde_json::json!({
            "preloaded": true,
            "tools": [
                {"name": "zeta__b", "description": long, "input_schema_json": "{}",
                 "host_exec": {"adapter_id": "mcp:zeta:b"}},
                {"name": "zeta__a", "description": "A", "input_schema_json": "{}",
                 "host_exec": {"adapter_id": "mcp:zeta:a"}}
            ]
        })
        .to_string();
        write_skill(&mount, "mcp-zeta", "# mcp-zeta\n\nServer notes.", &manifest);
        write_skill(
            &mount,
            "other",
            "# other\n\nBody.",
            r#"{"tools":[{"name":"other_tool","description":"x","input_schema_json":"{}"}]}"#,
        );

        let mut runtime = SkillsRuntime::default();
        let loaded = runtime.preload_with_mount(&mount);
        assert_eq!(loaded, vec!["mcp-zeta".to_string()]);
        assert!(runtime.is_loaded("mcp-zeta"));
        assert!(!runtime.is_loaded("other"));
        let names: Vec<&str> = runtime
            .loaded_tools()
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, vec!["zeta__a", "zeta__b"], "stable, sorted order");
        let b = runtime
            .loaded_tools()
            .iter()
            .find(|t| t.name == "zeta__b")
            .unwrap();
        assert!(b.description.len() > 3000, "description arrives whole");
        assert!(b.description.starts_with("<<<VAM-UNTRUSTED-DATA"));
        assert!(runtime.extensions_text().contains("Server notes."));

        let catalog = format_skills_catalog(&SkillsRuntime::scan_catalog_with_mount(&mount));
        assert!(!catalog.contains("mcp-zeta"), "{catalog}");
        assert!(catalog.contains("other"));

        // `load_skill mcp-zeta` still works and is idempotent.
        let again = runtime.load_with_mount("mcp-zeta", &mount).unwrap();
        assert!(again.contains("already loaded"));
        assert_eq!(runtime.loaded_tools().len(), 2);
    }

    #[test]
    fn load_accepts_camel_case_manifest() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        write_skill(
            &mount,
            "assistant-tools",
            "# Calculator\n",
            r#"{"tools":[{"name":"calculator","description":"Math","inputSchemaJson":"{}","sideEffect":"SIDE_EFFECT_CLASS_READ_ONLY","localExec":{"command":"bun","entrypoint":"./execute.js","defaultArgs":["calculator"]}}]}"#,
        );

        let mut runtime = SkillsRuntime::default();
        runtime
            .load_with_mount("assistant-tools", &mount)
            .expect("camelCase manifest loads");
        let tool = &runtime.loaded_tools()[0];
        assert_eq!(tool.input_schema_json, "{}");
        match &tool.execution_provider {
            Some(vam_spec::v1::tool_spec::ExecutionProvider::LocalExec(spec)) => {
                assert_eq!(spec.command, "bun");
                assert_eq!(spec.default_args, vec!["calculator".to_string()]);
            }
            other => panic!("expected localExec, got {other:?}"),
        }
    }

    #[test]
    fn format_catalog_has_no_schemas() {
        let catalog = format_skills_catalog(&[SkillCatalogEntry {
            skill_id: "assistant-tools".into(),
            name: "Calculator".into(),
            description: "Math tools".into(),
            preloaded: false,
        }]);
        assert!(catalog.contains("assistant-tools"));
        assert!(catalog.contains("Math tools"));
        assert!(!catalog.contains("input_schema"));
    }

    #[test]
    fn boot_candidates_exclude_unloaded_skill_tools() {
        let loaded = vec![ToolSpec {
            name: "calculator".to_string(),
            description: "math".to_string(),
            input_schema_json: String::new(),
            output_schema_json: String::new(),
            side_effect: 0,
            timeout: None,
            execution_provider: None,
            required_secrets: vec![],
            ..Default::default()
        }];
        let boot = boot_candidate_tools(&[], "", false);
        assert!(boot.iter().any(|t| t.name == "load_skill"));
        assert!(!boot.iter().any(|t| t.name == "calculator"));
        assert!(!boot.iter().any(|t| t.name == STEP_COMPLETE_TOOL_NAME));
        assert!(!boot.iter().any(|t| t.name == RUN_STEP_TOOL_NAME));

        let with_loaded = boot_candidate_tools(&loaded, "", false);
        assert!(with_loaded.iter().any(|t| t.name == "calculator"));

        let with_playbook = boot_candidate_tools(&[], "1. Do the work", false);
        assert!(with_playbook
            .iter()
            .any(|t| t.name == STEP_COMPLETE_TOOL_NAME));
        assert!(with_playbook.iter().any(|t| t.name == RUN_STEP_TOOL_NAME));
        let spec = with_playbook
            .iter()
            .find(|t| t.name == STEP_COMPLETE_TOOL_NAME)
            .expect("step_complete");
        assert!(matches!(
            spec.execution_provider,
            Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(_))
        ));
    }

    #[test]
    fn reserved_builtin_list_includes_load_skill() {
        assert!(vam_spec::validation::reserved_builtin_tool_names().contains(&"load_skill"));
    }

    #[test]
    fn secret_requirement_scope_parses_org_user_either() {
        let _guard = skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        fs::create_dir_all(&mount).unwrap();
        write_skill(
            &mount,
            "scoped",
            "# Scoped\n\nBody.",
            r#"{"tools":[{"name":"t1","description":"d","input_schema_json":"{}","required_secrets":[{"name":"a/b","scope":"org"},{"name":"c/d","scope":"user","required":false},{"name":"e/f","scope":"either"},{"name":"g/h"}]}]}"#,
        );

        let mut runtime = SkillsRuntime::default();
        runtime.load_with_mount("scoped", &mount).unwrap();
        let tool = &runtime.loaded_tools()[0];
        assert_eq!(tool.required_secrets.len(), 4);
        // Proto Scope: 1=org, 2=user, 3=either. Missing defaults to either.
        assert_eq!(tool.required_secrets[0].scope, 1);
        assert!(tool.required_secrets[0].required);
        assert_eq!(tool.required_secrets[1].scope, 2);
        assert!(!tool.required_secrets[1].required);
        assert_eq!(tool.required_secrets[2].scope, 3);
        assert_eq!(tool.required_secrets[3].scope, 3);
    }
}
