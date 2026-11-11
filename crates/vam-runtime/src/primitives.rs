use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use glob::Pattern;
use prost_types::Struct;
use regex::Regex;
use vam_spec::v1::{SideEffectClass, ToolSpec};
use vam_spec::validation::reserved_builtin_tool_names;
use walkdir::WalkDir;

use crate::exec::SubprocessToolResult;
use crate::skills::SKILLS_MOUNT;

pub const BUILTIN_ABI_VERSION: &str = "v1";
/// Guest-visible prefix stamped on skills-mount `read_file` / `grep` output.
pub const SOURCE_STAMP_PREFIX: &str = "Source: ";
const MAX_READ_BYTES: usize = 256 * 1024;
const MAX_GLOB_RESULTS: usize = 200;
const MAX_GREP_RESULTS: usize = 200;
const TODO_FILE: &str = ".vam/agent/todos.json";

pub fn is_builtin(tool_name: &str) -> bool {
    reserved_builtin_tool_names().contains(&tool_name)
}

pub fn builtin_digest(tool_name: &str) -> String {
    vam_spec::validation::builtin_tool_digest(tool_name)
}

pub fn workspace_root() -> PathBuf {
    std::env::var("VAM_WORKSPACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(crate::landlock::DEFAULT_WORKSPACE_ROOT))
}

fn get_string(args: &Struct, key: &str) -> Option<String> {
    args.fields.get(key).and_then(|v| {
        if let Some(prost_types::value::Kind::StringValue(s)) = &v.kind {
            Some(s.clone())
        } else {
            None
        }
    })
}

fn get_i64(args: &Struct, key: &str) -> Option<i64> {
    args.fields.get(key).and_then(|v| {
        if let Some(prost_types::value::Kind::NumberValue(n)) = &v.kind {
            Some(*n as i64)
        } else {
            None
        }
    })
}

fn get_bool(args: &Struct, key: &str) -> bool {
    args.fields
        .get(key)
        .and_then(|v| {
            if let Some(prost_types::value::Kind::BoolValue(b)) = &v.kind {
                Some(*b)
            } else {
                None
            }
        })
        .unwrap_or(false)
}

fn normalize_rel_path(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if path.contains('\0') {
        return Err("path contains null byte".to_string());
    }
    let p = Path::new(path);
    for component in p.components() {
        match component {
            Component::ParentDir => {
                return Err("path traversal ('..') is not allowed".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute paths outside the workspace jail are not allowed".to_string());
            }
            _ => {}
        }
    }
    Ok(p.to_path_buf())
}

pub(crate) fn skills_mount_root() -> PathBuf {
    std::env::var("VAM_SKILLS_MOUNT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(SKILLS_MOUNT))
}

fn is_skills_mount_path(canonical: &Path) -> bool {
    let skills = skills_mount_root();
    let skills_canonical = skills.canonicalize().unwrap_or(skills);
    canonical.starts_with(&skills_canonical)
}

/// Map a canonical on-disk skills path to `/mnt/drive9/skills/<skill-id>/...`.
/// Never returns a host filesystem path.
fn guest_visible_skills_path(canonical: &Path) -> Option<String> {
    let skills_root = skills_mount_root();
    let skills_canonical = skills_root
        .canonicalize()
        .unwrap_or_else(|_| skills_root.clone());
    let rel = canonical.strip_prefix(&skills_canonical).ok()?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if rel_str.is_empty() {
        Some(SKILLS_MOUNT.to_string())
    } else {
        Some(format!("{SKILLS_MOUNT}/{rel_str}"))
    }
}

fn stamp_source(body: String, guest_path: &str) -> String {
    if body.is_empty() {
        format!("{SOURCE_STAMP_PREFIX}{guest_path}")
    } else {
        format!("{SOURCE_STAMP_PREFIX}{guest_path}\n{body}")
    }
}

fn display_search_path(path: &Path, root_canonical: &Path) -> String {
    if let Some(guest) = guest_visible_skills_path(path) {
        return guest;
    }
    path.strip_prefix(root_canonical)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_protected_write_path(rel: &Path) -> bool {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if rel_str == ".vam/workspace.json"
        || rel_str == ".vam/SYSTEM.md"
        || rel_str.starts_with(".vam/threads/")
    {
        return true;
    }
    false
}

fn verify_inside_jail(canonical: &Path, root_canonical: &Path, path: &str) -> Result<(), String> {
    if !canonical.starts_with(root_canonical) {
        return Err(format!("path '{}' resolves outside workspace jail", path));
    }
    if is_skills_mount_path(canonical) {
        return Err("access to /mnt/drive9/skills is forbidden from workspace tools".to_string());
    }
    Ok(())
}

/// Resolve a read path: workspace-relative jail paths, or absolute read-only skills mount paths.
pub fn resolve_read_path(path: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        let skills_root = skills_mount_root();
        let skills_canonical = skills_root
            .canonicalize()
            .unwrap_or_else(|_| skills_root.clone());
        let canonical = candidate
            .canonicalize()
            .map_err(|e| format!("failed to resolve path '{}': {}", path, e))?;
        if canonical.starts_with(&skills_canonical) {
            if !canonical.is_file() {
                return Err(format!("failed to resolve path '{}': file not found", path));
            }
            return Ok(canonical);
        }
        return Err("absolute paths outside the workspace jail are not allowed".to_string());
    }
    resolve_jailed_path(path, false)
}

/// Base directory for glob/grep: optional `path` (workspace-relative or skills mount), else workspace root.
fn search_root_from_args(args: &Struct) -> Result<PathBuf, String> {
    if let Some(path) = get_string(args, "path") {
        let p = Path::new(&path);
        if p.is_absolute() {
            let skills_root = skills_mount_root();
            let skills_canonical = skills_root
                .canonicalize()
                .unwrap_or_else(|_| skills_root.clone());
            let canonical = p
                .canonicalize()
                .map_err(|e| format!("failed to resolve path '{}': {}", path, e))?;
            if !canonical.starts_with(&skills_canonical) {
                return Err(format!(
                    "path '{}' is outside the allowed skills mount",
                    path
                ));
            }
            if !canonical.is_dir() {
                return Err(format!("path '{}' is not a directory", path));
            }
            return Ok(canonical);
        }
        let resolved = resolve_jailed_path(&path, false)?;
        if !resolved.is_dir() {
            return Err(format!("path '{}' is not a directory", path));
        }
        return Ok(resolved);
    }
    let root = workspace_root();
    Ok(root.canonicalize().unwrap_or(root))
}

/// Resolve a user-supplied path against the workspace jail.
pub fn resolve_jailed_path(path: &str, for_write: bool) -> Result<PathBuf, String> {
    let rel = normalize_rel_path(path)?;
    if for_write && is_protected_write_path(&rel) {
        return Err(format!(
            "writes to '{}' are forbidden (host timeline metadata)",
            rel.display()
        ));
    }

    let root = workspace_root();
    let candidate = root.join(&rel);
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());

    if candidate.exists() {
        let canonical = candidate
            .canonicalize()
            .map_err(|e| format!("failed to resolve path '{}': {}", path, e))?;
        verify_inside_jail(&canonical, &root_canonical, path)?;
        return Ok(canonical);
    }

    if for_write {
        let parent = candidate
            .parent()
            .ok_or_else(|| format!("failed to resolve path '{}': no parent directory", path))?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create parent directories: {}", e))?;

        let canonical_parent = parent
            .canonicalize()
            .map_err(|e| format!("failed to resolve path '{}': {}", path, e))?;
        verify_inside_jail(&canonical_parent, &root_canonical, path)?;

        let file_name = candidate
            .file_name()
            .ok_or_else(|| format!("failed to resolve path '{}': no file name", path))?;
        let resolved = canonical_parent.join(file_name);
        verify_inside_jail(&resolved, &root_canonical, path)?;
        return Ok(resolved);
    }

    Err(format!("failed to resolve path '{}': file not found", path))
}

pub fn execute_builtin(
    tool_name: &str,
    args: Option<&Struct>,
) -> Result<SubprocessToolResult, String> {
    let args = args.cloned().unwrap_or_default();
    let output = match tool_name {
        "read_file" => read_file(&args)?,
        "write_file" => write_file(&args)?,
        "edit_file" => edit_file(&args)?,
        "delete_file" => delete_file(&args)?,
        "glob" => glob_search(&args)?,
        "grep" => grep_search(&args)?,
        "todo_write" => todo_write(&args)?,
        other => return Err(format!("unknown builtin tool '{}'", other)),
    };

    Ok(SubprocessToolResult {
        stdout: output.into_bytes(),
        stderr: Vec::new(),
        system_logs: String::new(),
        exit_code: 0,
        error: None,
        resource_usage: None,
    })
}

fn read_file(args: &Struct) -> Result<String, String> {
    let path = get_string(args, "path")
        .or_else(|| get_string(args, "filepath"))
        .ok_or_else(|| "read_file requires 'path'".to_string())?;
    let offset = get_i64(args, "offset").unwrap_or(1).max(1) as usize;
    let limit = get_i64(args, "limit").map(|l| l.max(1) as usize);

    let resolved = resolve_read_path(&path)?;
    let guest_source = guest_visible_skills_path(&resolved);
    let content =
        fs::read_to_string(&resolved).map_err(|e| format!("failed to read '{}': {}", path, e))?;

    let lines: Vec<&str> = content.lines().collect();
    let start_idx = offset.saturating_sub(1);
    let mut out = if start_idx >= lines.len() {
        String::new()
    } else {
        let end_idx = limit
            .map(|l| (start_idx + l).min(lines.len()))
            .unwrap_or(lines.len());

        let mut body = lines[start_idx..end_idx].join("\n");
        if end_idx < lines.len() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&format!(
                "\n[Truncated: showing lines {}-{} of {} total]",
                start_idx + 1,
                end_idx,
                lines.len()
            ));
        }
        body
    };

    if out.len() > MAX_READ_BYTES {
        out.truncate(MAX_READ_BYTES);
        out.push_str("\n[Truncated: file content exceeded size limit]");
    }

    if let Some(guest) = guest_source {
        out = stamp_source(out, &guest);
    }

    Ok(out)
}

fn write_file(args: &Struct) -> Result<String, String> {
    let path = get_string(args, "path")
        .or_else(|| get_string(args, "filepath"))
        .ok_or_else(|| "write_file requires 'path'".to_string())?;
    let content = get_string(args, "content").unwrap_or_default();

    let resolved = resolve_jailed_path(&path, true)?;
    if let Some(parent) = resolved.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create parent directories: {}", e))?;
    }
    fs::write(&resolved, &content).map_err(|e| format!("failed to write '{}': {}", path, e))?;
    Ok(format!("Wrote {} bytes to {}", content.len(), path))
}

fn edit_file(args: &Struct) -> Result<String, String> {
    let path = get_string(args, "path")
        .or_else(|| get_string(args, "filepath"))
        .ok_or_else(|| "edit_file requires 'path'".to_string())?;
    let old_string = get_string(args, "old_string")
        .ok_or_else(|| "edit_file requires 'old_string'".to_string())?;
    let new_string = get_string(args, "new_string").unwrap_or_default();
    let replace_all = get_bool(args, "replace_all");

    let resolved = resolve_jailed_path(&path, true)?;
    let content =
        fs::read_to_string(&resolved).map_err(|e| format!("failed to read '{}': {}", path, e))?;

    let count = content.matches(&old_string).count();
    if count == 0 {
        return Err(format!(
            "old_string not found in '{}' (unique match required)",
            path
        ));
    }
    if !replace_all && count > 1 {
        return Err(format!(
            "old_string appears {} times in '{}'; set replace_all=true or provide a more unique old_string",
            count, path
        ));
    }

    let updated = if replace_all {
        content.replace(&old_string, &new_string)
    } else {
        content.replacen(&old_string, &new_string, 1)
    };

    fs::write(&resolved, &updated).map_err(|e| format!("failed to write '{}': {}", path, e))?;

    let replaced = if replace_all { count } else { 1 };
    Ok(format!("Replaced {} occurrence(s) in {}", replaced, path))
}

fn delete_file(args: &Struct) -> Result<String, String> {
    let path = get_string(args, "path")
        .or_else(|| get_string(args, "filepath"))
        .ok_or_else(|| "delete_file requires 'path'".to_string())?;

    let resolved = resolve_jailed_path(&path, true)?;
    if resolved.is_dir() {
        return Err(format!(
            "'{}' is a directory; delete_file only removes files",
            path
        ));
    }
    fs::remove_file(&resolved).map_err(|e| format!("failed to delete '{}': {}", path, e))?;
    Ok(format!("Deleted {}", path))
}

fn glob_search(args: &Struct) -> Result<String, String> {
    let pattern_str = get_string(args, "pattern")
        .or_else(|| get_string(args, "glob_pattern"))
        .ok_or_else(|| "glob requires 'pattern'".to_string())?;

    let pattern = Pattern::new(&pattern_str)
        .map_err(|e| format!("invalid glob pattern '{}': {}", pattern_str, e))?;

    let root_canonical = search_root_from_args(args)?;

    let mut matches: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in WalkDir::new(&root_canonical)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path == root_canonical {
            continue;
        }
        if is_skills_mount_path(path) && !path.starts_with(&root_canonical) {
            continue;
        }
        let rel = path
            .strip_prefix(&root_canonical)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if pattern.matches(&rel)
            || pattern.matches(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
            )
        {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            matches.push((mtime, path.to_path_buf()));
        }
    }

    matches.sort_by_key(|m| std::cmp::Reverse(m.0));
    matches.truncate(MAX_GLOB_RESULTS);

    let lines: Vec<String> = matches
        .iter()
        .map(|(_, p)| {
            p.strip_prefix(&root_canonical)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    let mut out = lines.join("\n");
    if matches.len() >= MAX_GLOB_RESULTS {
        out.push_str(&format!(
            "\n[Truncated: showing newest {} matches]",
            MAX_GLOB_RESULTS
        ));
    }
    Ok(out)
}

fn grep_search(args: &Struct) -> Result<String, String> {
    let pattern_str =
        get_string(args, "pattern").ok_or_else(|| "grep requires 'pattern'".to_string())?;
    let re =
        Regex::new(&pattern_str).map_err(|e| format!("invalid regex '{}': {}", pattern_str, e))?;

    let root_canonical = search_root_from_args(args)?;

    let mut results: Vec<String> = Vec::new();
    'walk: for entry in WalkDir::new(&root_canonical)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path == root_canonical || path.is_dir() {
            continue;
        }
        if is_skills_mount_path(path) && !path.starts_with(&root_canonical) {
            continue;
        }
        let rel = path
            .strip_prefix(&root_canonical)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with(".vam/threads/") {
            continue;
        }

        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };

        let display = display_search_path(path, &root_canonical);
        for (line_no, line) in content.lines().enumerate() {
            if re.is_match(line) {
                results.push(format!("{}:{}:{}", display, line_no + 1, line));
                if results.len() >= MAX_GREP_RESULTS {
                    break 'walk;
                }
            }
        }
    }

    let mut out = results.join("\n");
    if results.len() >= MAX_GREP_RESULTS {
        out.push_str(&format!(
            "\n[Truncated: showing first {} matches]",
            MAX_GREP_RESULTS
        ));
    }
    if let Some(guest_root) = guest_visible_skills_path(&root_canonical) {
        out = stamp_source(out, &guest_root);
    }
    Ok(out)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct TodoItem {
    id: String,
    content: String,
    status: String,
}

fn todo_write(args: &Struct) -> Result<String, String> {
    let todos_val = args.fields.get("todos").ok_or_else(|| {
        "todo_write requires 'todos' array with objects {id, content, status}".to_string()
    })?;

    let items = parse_todo_items(todos_val)?;
    let todo_path = resolve_jailed_path(TODO_FILE, true)?;
    if let Some(parent) = todo_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create todo directory: {}", e))?;
    }

    let mut existing: HashMap<String, TodoItem> = HashMap::new();
    if todo_path.is_file() {
        if let Ok(content) = fs::read_to_string(&todo_path) {
            if let Ok(parsed) = serde_json::from_str::<Vec<TodoItem>>(&content) {
                for item in parsed {
                    existing.insert(item.id.clone(), item);
                }
            }
        }
    }

    for item in items {
        existing.insert(item.id.clone(), item);
    }

    let mut merged: Vec<TodoItem> = existing.into_values().collect();
    merged.sort_by(|a, b| a.id.cmp(&b.id));

    let json = serde_json::to_string_pretty(&merged)
        .map_err(|e| format!("failed to serialize todos: {}", e))?;
    fs::write(&todo_path, json.as_bytes())
        .map_err(|e| format!("failed to write todos file: {}", e))?;

    Ok(format!(
        "Merged {} todo item(s) into {}",
        merged.len(),
        TODO_FILE
    ))
}

fn parse_todo_items(value: &prost_types::Value) -> Result<Vec<TodoItem>, String> {
    let list = match &value.kind {
        Some(prost_types::value::Kind::ListValue(l)) => &l.values,
        _ => {
            return Err("'todos' must be an array".to_string());
        }
    };

    let mut items = Vec::new();
    for entry in list {
        let fields = match &entry.kind {
            Some(prost_types::value::Kind::StructValue(s)) => &s.fields,
            _ => return Err("each todo must be an object".to_string()),
        };
        let id = fields
            .get("id")
            .and_then(|v| {
                if let Some(prost_types::value::Kind::StringValue(s)) = &v.kind {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "todo item requires non-empty 'id'".to_string())?;
        let content = fields
            .get("content")
            .and_then(|v| {
                if let Some(prost_types::value::Kind::StringValue(s)) = &v.kind {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let status = fields
            .get("status")
            .and_then(|v| {
                if let Some(prost_types::value::Kind::StringValue(s)) = &v.kind {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "pending".to_string());
        items.push(TodoItem {
            id,
            content,
            status,
        });
    }
    Ok(items)
}

pub fn builtin_tool_specs() -> Vec<ToolSpec> {
    reserved_builtin_tool_names()
        .iter()
        .map(|name| {
            let (description, schema, side_effect) = match *name {
                "read_file" => (
                    "Read a file from the workspace. Optional offset (1-based line) and limit.",
                    r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]}"#,
                    SideEffectClass::ReadOnly as i32,
                ),
                "write_file" => (
                    "Create or overwrite a file in the workspace. Creates parent directories.",
                    r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}"#,
                    SideEffectClass::MutableLocal as i32,
                ),
                "edit_file" => (
                    "Replace a unique old_string with new_string in a workspace file.",
                    r#"{"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_string","new_string"]}"#,
                    SideEffectClass::MutableLocal as i32,
                ),
                "delete_file" => (
                    "Delete a file under the workspace jail.",
                    r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#,
                    SideEffectClass::MutableLocal as i32,
                ),
                "glob" => (
                    "Find files matching a glob pattern (newest first, capped). Optional path scopes search to a workspace directory or skills mount.",
                    r#"{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"Optional directory: workspace-relative or absolute /mnt/drive9/skills/..."}},"required":["pattern"]}"#,
                    SideEffectClass::ReadOnly as i32,
                ),
                "grep" => (
                    "Search files with a regex pattern. Optional path scopes search to a workspace directory or skills mount.",
                    r#"{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"Optional directory: workspace-relative or absolute /mnt/drive9/skills/..."}},"required":["pattern"]}"#,
                    SideEffectClass::ReadOnly as i32,
                ),
                "load_skill" => (
                    "Load a skill by catalog id (directory name under /mnt/drive9/skills). Injects SKILLS.md instructions and registers that skill's tools.",
                    r#"{"type":"object","properties":{"skill":{"type":"string","description":"Skill directory name, e.g. assistant-tools"}},"required":["skill"]}"#,
                    SideEffectClass::ReadOnly as i32,
                ),
                "run" => (
                    "Run a shell command in the workspace directory via sh.",
                    r#"{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}"#,
                    SideEffectClass::MutableLocal as i32,
                ),
                "todo_write" => (
                    "Merge todos by id into .vam/agent/todos.json.",
                    r#"{"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string"},"status":{"type":"string"}},"required":["id","content","status"]}}},"required":["todos"]}"#,
                    SideEffectClass::MutableLocal as i32,
                ),
                _ => unreachable!(),
            };

            ToolSpec {
                name: (*name).to_string(),
                description: description.to_string(),
                input_schema_json: schema.to_string(),
                execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::Builtin(
                    vam_spec::v1::BuiltinExecSpec {
                        version: BUILTIN_ABI_VERSION.to_string(),
                    },
                )),
                required_secrets: vec![],
                side_effect,
                output_schema_json: String::new(),
                timeout: None,
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::Value;

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::skills::skills_mount_test_guard()
    }

    fn setup_workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        fs::create_dir_all(&ws).unwrap();
        std::env::set_var("VAM_WORKSPACE_ROOT", &ws);
        (dir, ws)
    }

    fn string_field(key: &str, val: &str) -> (String, Value) {
        (
            key.to_string(),
            Value {
                kind: Some(prost_types::value::Kind::StringValue(val.to_string())),
            },
        )
    }

    #[test]
    fn test_path_jail_rejects_traversal() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        assert!(resolve_jailed_path("../etc/passwd", false).is_err());
        assert!(resolve_jailed_path("foo/../../secret", false).is_err());
        assert!(resolve_jailed_path("../escape.txt", true).is_err());
    }

    #[test]
    fn test_write_file_creates_new_file() {
        let _guard = test_guard();
        let (_dir, ws) = setup_workspace();

        let mut args = Struct::default();
        args.fields
            .insert("path".into(), string_field("path", "notes/hello.txt").1);
        args.fields
            .insert("content".into(), string_field("content", "hello world").1);

        let res = execute_builtin("write_file", Some(&args)).unwrap();
        let out = String::from_utf8_lossy(&res.stdout);
        assert!(out.contains("Wrote"));
        assert!(out.contains("notes/hello.txt"));

        let file_path = ws.join("notes/hello.txt");
        assert!(file_path.is_file());
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello world");
    }

    #[test]
    fn test_todo_write_when_file_missing() {
        let _guard = test_guard();
        let (_dir, ws) = setup_workspace();

        let mut args = Struct::default();
        args.fields.insert(
            "todos".into(),
            Value {
                kind: Some(prost_types::value::Kind::ListValue(
                    prost_types::ListValue {
                        values: vec![Value {
                            kind: Some(prost_types::value::Kind::StructValue(Struct {
                                fields: [
                                    string_field("id", "1"),
                                    string_field("content", "first task"),
                                    string_field("status", "pending"),
                                ]
                                .into_iter()
                                .collect(),
                            })),
                        }],
                    },
                )),
            },
        );

        execute_builtin("todo_write", Some(&args)).unwrap();
        let todo_path = ws.join(".vam/agent/todos.json");
        assert!(todo_path.is_file());
        let stored: Vec<TodoItem> =
            serde_json::from_str(&fs::read_to_string(&todo_path).unwrap()).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].content, "first task");
    }

    #[test]
    fn test_path_jail_rejects_thread_writes() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        assert!(resolve_jailed_path(".vam/threads/t1/timeline.jsonl", true).is_err());
        assert!(resolve_jailed_path(".vam/workspace.json", true).is_err());
        assert!(resolve_jailed_path(".vam/SYSTEM.md", true).is_err());
    }

    #[test]
    fn test_edit_file_unique_and_replace_all() {
        let _guard = test_guard();
        let (_dir, ws) = setup_workspace();
        let file = ws.join("sample.txt");
        fs::write(&file, "alpha beta alpha\n").unwrap();

        let mut args = Struct::default();
        args.fields
            .insert("path".into(), string_field("path", "sample.txt").1);
        args.fields
            .insert("old_string".into(), string_field("old_string", "alpha").1);
        args.fields
            .insert("new_string".into(), string_field("new_string", "gamma").1);
        assert!(execute_builtin("edit_file", Some(&args)).is_err());

        args.fields.insert(
            "replace_all".into(),
            Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        let res = execute_builtin("edit_file", Some(&args)).unwrap();
        assert!(String::from_utf8_lossy(&res.stdout).contains("Replaced 2"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "gamma beta gamma\n");
    }

    #[test]
    fn test_glob_and_grep_workspace_only() {
        let _guard = test_guard();
        let (_dir, ws) = setup_workspace();
        fs::write(ws.join("a.rs"), "fn main() {}\n").unwrap();
        fs::write(ws.join("b.txt"), "hello world\n").unwrap();

        let mut glob_args = Struct::default();
        glob_args
            .fields
            .insert("pattern".into(), string_field("pattern", "*.rs").1);
        let glob_out = execute_builtin("glob", Some(&glob_args)).unwrap();
        assert!(String::from_utf8_lossy(&glob_out.stdout).contains("a.rs"));

        let mut grep_args = Struct::default();
        grep_args
            .fields
            .insert("pattern".into(), string_field("pattern", "fn main").1);
        let grep_out = execute_builtin("grep", Some(&grep_args)).unwrap();
        assert!(String::from_utf8_lossy(&grep_out.stdout).contains("a.rs:1:"));
    }

    #[test]
    fn test_todo_write_merge_by_id() {
        let _guard = test_guard();
        let (_dir, ws) = setup_workspace();
        let agent_dir = ws.join(".vam/agent");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("todos.json"),
            r#"[{"id":"1","content":"old","status":"pending"}]"#,
        )
        .unwrap();

        let mut args = Struct::default();
        args.fields.insert(
            "todos".into(),
            Value {
                kind: Some(prost_types::value::Kind::ListValue(
                    prost_types::ListValue {
                        values: vec![Value {
                            kind: Some(prost_types::value::Kind::StructValue(Struct {
                                fields: [
                                    string_field("id", "1"),
                                    string_field("content", "updated"),
                                    string_field("status", "completed"),
                                ]
                                .into_iter()
                                .collect(),
                            })),
                        }],
                    },
                )),
            },
        );

        execute_builtin("todo_write", Some(&args)).unwrap();
        let stored: Vec<TodoItem> =
            serde_json::from_str(&fs::read_to_string(agent_dir.join("todos.json")).unwrap())
                .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].content, "updated");
        assert_eq!(stored[0].status, "completed");
    }

    #[test]
    fn test_read_file_allows_skills_mount() {
        let _guard = test_guard();
        let (_dir, ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        let skill_path = skills_dir.path().join("db-tools");
        std::fs::create_dir_all(&skill_path).unwrap();
        let schema = skill_path.join("SCHEMA.md");
        std::fs::write(&schema, "# Schema\n\ntable facts\n").unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());

        let abs = format!("{}/db-tools/SCHEMA.md", skills_dir.path().display());
        let mut args = Struct::default();
        args.fields
            .insert("path".into(), string_field("path", &abs).1);
        let res = execute_builtin("read_file", Some(&args)).unwrap();
        let out = String::from_utf8_lossy(&res.stdout);
        assert!(out.contains("table facts"));
        assert!(out.contains("Source: /mnt/drive9/skills/db-tools/SCHEMA.md"));
        assert!(
            !out.contains(&skills_dir.path().display().to_string()),
            "must not leak host skills-mount path"
        );
        let _ = ws;
    }

    #[test]
    fn test_read_file_line_limit_keeps_skills_stamp() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        let skill_path = skills_dir.path().join("db-tools");
        std::fs::create_dir_all(&skill_path).unwrap();
        std::fs::write(skill_path.join("SCHEMA.md"), "l1\nl2\nl3\nl4\n").unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());

        let abs = format!("{}/db-tools/SCHEMA.md", skills_dir.path().display());
        let mut args = Struct::default();
        args.fields
            .insert("path".into(), string_field("path", &abs).1);
        args.fields.insert(
            "limit".into(),
            Value {
                kind: Some(prost_types::value::Kind::NumberValue(2.0)),
            },
        );
        let out =
            String::from_utf8_lossy(&execute_builtin("read_file", Some(&args)).unwrap().stdout)
                .into_owned();
        assert!(out.starts_with("Source: /mnt/drive9/skills/db-tools/SCHEMA.md"));
        assert!(out.contains("l1"));
        assert!(out.contains("[Truncated: showing lines 1-2 of 4 total]"));
        assert!(!out.contains("l3"));
    }

    #[test]
    fn test_read_file_byte_truncate_keeps_skills_stamp() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        let skill_path = skills_dir.path().join("db-tools");
        std::fs::create_dir_all(&skill_path).unwrap();
        let huge = "x".repeat(MAX_READ_BYTES + 64);
        std::fs::write(skill_path.join("BIG.md"), &huge).unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());

        let abs = format!("{}/db-tools/BIG.md", skills_dir.path().display());
        let mut args = Struct::default();
        args.fields
            .insert("path".into(), string_field("path", &abs).1);
        let out =
            String::from_utf8_lossy(&execute_builtin("read_file", Some(&args)).unwrap().stdout)
                .into_owned();
        assert!(out.starts_with("Source: /mnt/drive9/skills/db-tools/BIG.md"));
        assert!(out.contains("[Truncated: file content exceeded size limit]"));
        assert!(!out.contains(skills_dir.path().to_str().unwrap()));
    }

    #[test]
    fn test_grep_skills_emits_guest_paths() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        let skill_path = skills_dir.path().join("db-tools").join("docs");
        std::fs::create_dir_all(&skill_path).unwrap();
        std::fs::write(skill_path.join("SCHEMA.md"), "table facts live here\n").unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());

        let mut grep_args = Struct::default();
        grep_args
            .fields
            .insert("pattern".into(), string_field("pattern", "table facts").1);
        let abs = format!("{}/db-tools", skills_dir.path().display());
        grep_args
            .fields
            .insert("path".into(), string_field("path", &abs).1);
        let out =
            String::from_utf8_lossy(&execute_builtin("grep", Some(&grep_args)).unwrap().stdout)
                .into_owned();
        assert!(out.contains("Source: /mnt/drive9/skills/db-tools"));
        assert!(out.contains("/mnt/drive9/skills/db-tools/docs/SCHEMA.md:1:table facts live here"));
        assert!(!out.contains(skills_dir.path().to_str().unwrap()));
    }

    #[test]
    fn test_read_file_rejects_paths_outside_jail_and_skills() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());

        let mut args = Struct::default();
        args.fields
            .insert("path".into(), string_field("path", "/etc/passwd").1);
        assert!(execute_builtin("read_file", Some(&args)).is_err());

        args.fields.insert(
            "path".into(),
            string_field("path", "/tmp/vam-not-a-skill.txt").1,
        );
        assert!(execute_builtin("read_file", Some(&args)).is_err());
    }

    #[test]
    fn test_grep_rejects_path_outside_skills_mount() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());

        let mut grep_args = Struct::default();
        grep_args
            .fields
            .insert("pattern".into(), string_field("pattern", "root").1);
        grep_args
            .fields
            .insert("path".into(), string_field("path", "/etc").1);
        assert!(execute_builtin("grep", Some(&grep_args)).is_err());
    }

    #[test]
    fn test_index_md_readable_and_grep_resolves_guest_path() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        let skill = skills_dir.path().join("oracle-sql");
        fs::create_dir_all(skill.join("docs/tables")).unwrap();
        let index = crate::skill_index::generate_index((0..250).map(|i| {
            (
                format!("table_{i:04}"),
                format!("docs/tables/table_{i:04}.md"),
            )
        }));
        fs::write(skill.join("INDEX.md"), &index).unwrap();
        fs::write(
            skill.join("docs/tables/table_0042.md"),
            "# table_0042\n\ncolumns\n",
        )
        .unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());

        let index_abs = format!("{}/oracle-sql/INDEX.md", skills_dir.path().display());
        let mut read_args = Struct::default();
        read_args
            .fields
            .insert("path".into(), string_field("path", &index_abs).1);
        let read_out = String::from_utf8_lossy(
            &execute_builtin("read_file", Some(&read_args))
                .unwrap()
                .stdout,
        )
        .into_owned();
        assert!(read_out.contains("Source: /mnt/drive9/skills/oracle-sql/INDEX.md"));
        assert!(read_out.contains("table_0042"));

        let mut grep_args = Struct::default();
        grep_args.fields.insert(
            "pattern".into(),
            string_field("pattern", "^table_0042\\t").1,
        );
        grep_args.fields.insert(
            "path".into(),
            string_field(
                "path",
                &format!("{}/oracle-sql", skills_dir.path().display()),
            )
            .1,
        );
        let grep_out =
            String::from_utf8_lossy(&execute_builtin("grep", Some(&grep_args)).unwrap().stdout)
                .into_owned();
        assert!(grep_out.contains("/mnt/drive9/skills/oracle-sql/INDEX.md"));
        assert!(grep_out.contains("docs/tables/table_0042.md"));

        let parsed = crate::skill_index::parse_index(&index).unwrap();
        let hit = crate::skill_index::lookup(&parsed, "table_0042").unwrap();
        assert_eq!(
            crate::skill_index::guest_doc_path("oracle-sql", &hit.relative_path),
            "/mnt/drive9/skills/oracle-sql/docs/tables/table_0042.md"
        );
    }

    #[test]
    fn test_write_file_forbidden_under_skills_mount() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());
        let abs = format!("{}/evil.txt", skills_dir.path().display());
        let mut args = Struct::default();
        args.fields
            .insert("path".into(), string_field("path", &abs).1);
        args.fields
            .insert("content".into(), string_field("content", "nope").1);
        assert!(execute_builtin("write_file", Some(&args)).is_err());
    }

    #[test]
    fn test_glob_scoped_to_skills_path() {
        let _guard = test_guard();
        let (_dir, _ws) = setup_workspace();
        let skills_dir = tempfile::tempdir().unwrap();
        let skill_path = skills_dir.path().join("db-tools");
        std::fs::create_dir_all(&skill_path).unwrap();
        std::fs::write(skill_path.join("SCHEMA.md"), "schema\n").unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", skills_dir.path());

        let mut glob_args = Struct::default();
        glob_args
            .fields
            .insert("pattern".into(), string_field("pattern", "*.md").1);
        let abs = format!("{}/db-tools", skills_dir.path().display());
        glob_args
            .fields
            .insert("path".into(), string_field("path", &abs).1);
        let glob_out = execute_builtin("glob", Some(&glob_args)).unwrap();
        assert!(String::from_utf8_lossy(&glob_out.stdout).contains("SCHEMA.md"));
    }

    #[test]
    fn test_builtin_digest_constant() {
        assert_eq!(
            builtin_digest("read_file"),
            vam_spec::validation::builtin_tool_digest("read_file")
        );
    }
}
