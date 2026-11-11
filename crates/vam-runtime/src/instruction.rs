//! System instruction template loading and placeholder rendering.

use std::fs;
use std::path::Path;

use prost_types::Struct;
use vam_spec::v1::ToolSpec;

pub const DEFAULT_TEMPLATE: &str = include_str!("../templates/SYSTEM.md");
pub const DEFAULT_TEMPLATE_VERSION: u32 = 11;

const PLACEHOLDER_DISPLAY_NAME: &str = "{{agent.display_name}}";
const PLACEHOLDER_ROLE: &str = "{{agent.role}}";
const PLACEHOLDER_SYSTEM_INSTRUCTIONS: &str = "{{agent.system_instructions}}";
const PLACEHOLDER_TOOLS_INVENTORY: &str = "{{tools.inventory}}";
const PLACEHOLDER_SKILL_EXTENSIONS: &str = "{{skill.extensions}}";
const PLACEHOLDER_SKILLS_CATALOG: &str = "{{skills.catalog}}";
const PLACEHOLDER_MACHINE_FACTS: &str = "{{machine.facts}}";
const PLACEHOLDER_ENVIRONMENT_GUIDANCE: &str = "{{environment.guidance}}";
const PLACEHOLDER_PLAYBOOK_PROCEDURE: &str = "{{playbook.procedure}}";

const NO_LOCAL_TOOLS_GUIDANCE: &str = "This session has no file, shell or web tools. Do not write tool calls as text. Use only the tools listed for this session, and answer in plain language.";

const FILE_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob",
    "grep",
    "delete_file",
];

/// Parses `template_version` from YAML frontmatter (`---` delimited).
pub fn parse_template_version(content: &str) -> Option<u32> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let rest = trimmed.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("template_version:") {
            return val.trim().parse().ok();
        }
    }
    None
}

/// Returns the template body with YAML frontmatter removed.
pub fn template_body(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content;
    }
    let rest = trimmed.strip_prefix("---").unwrap_or(trimmed);
    if let Some(end) = rest.find("\n---") {
        let after = &rest[end + 4..];
        after.trim_start_matches('\n')
    } else {
        content
    }
}

/// Loads `workspace/.vam/SYSTEM.md`, falling back to the embedded default template.
pub fn load_template_source(workspace_root: &Path) -> String {
    let path = workspace_root.join(".vam/SYSTEM.md");
    fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_TEMPLATE.to_string())
}

/// Formats a bullet list of tool names and one-line descriptions for the prompt.
pub fn format_tools_inventory(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    tools
        .iter()
        .map(|t| {
            if is_mcp_host_tool(t) {
                // The full fenced description is in the tool definition the
                // model already receives; repeating it here would double the
                // prompt for every MCP tool.
                format!(
                    "- **{}**: (MCP tool; see its description in the tool definition)",
                    t.name
                )
            } else {
                format!("- **{}**: {}", t.name, t.description)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_mcp_host_tool(tool: &ToolSpec) -> bool {
    matches!(
        &tool.execution_provider,
        Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(host))
            if host.adapter_id.starts_with("mcp:")
    )
}

pub struct RenderContext<'a> {
    pub display_name: &'a str,
    pub role: &'a str,
    pub system_instructions: &'a str,
    pub tools_inventory: &'a str,
    pub skill_extensions: &'a str,
    pub skills_catalog: &'a str,
    pub machine_facts: &'a str,
    pub environment_guidance: &'a str,
    pub playbook_procedure: &'a str,
}

/// Replaces `{{dot.path}}` placeholders. Unknown or missing placeholders are left unchanged.
pub fn render_template(template: &str, ctx: &RenderContext<'_>) -> String {
    let body = template_body(template);
    let mut out = body.to_string();
    out = out.replace(PLACEHOLDER_DISPLAY_NAME, ctx.display_name);
    out = out.replace(PLACEHOLDER_ROLE, ctx.role);
    out = out.replace(PLACEHOLDER_SYSTEM_INSTRUCTIONS, ctx.system_instructions);
    out = out.replace(PLACEHOLDER_TOOLS_INVENTORY, ctx.tools_inventory);
    out = out.replace(PLACEHOLDER_SKILL_EXTENSIONS, ctx.skill_extensions);
    out = out.replace(PLACEHOLDER_SKILLS_CATALOG, ctx.skills_catalog);
    out = out.replace(PLACEHOLDER_MACHINE_FACTS, ctx.machine_facts);
    out = out.replace(PLACEHOLDER_ENVIRONMENT_GUIDANCE, ctx.environment_guidance);
    // Last, so a procedure that mentions other placeholders is not re-scanned.
    out = out.replace(PLACEHOLDER_PLAYBOOK_PROCEDURE, ctx.playbook_procedure);
    out
}

/// Operating-system line used when the image has no readable `/etc/os-release`.
const FALLBACK_OS_LINE: &str = "Minimal Linux (musl/busybox userland)";

/// Directories the guest tool `PATH` searches (see `exec.rs`), relative to the root.
const TOOL_DIRS: &[&str] = &["usr/local/bin", "usr/bin", "bin"];

/// Dynamically probes the guest VM environment to build the machine facts bullet list.
pub fn detect_machine_facts() -> String {
    machine_facts_at(Path::new("/"))
}

/// `PRETTY_NAME` from `<root>/etc/os-release`, if present and non-empty.
fn os_pretty_name(root: &Path) -> Option<String> {
    let text = fs::read_to_string(root.join("etc/os-release")).ok()?;
    text.lines().find_map(|line| {
        let value = line.trim().strip_prefix("PRETTY_NAME=")?;
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

fn tool_exists(root: &Path, name: &str) -> bool {
    TOOL_DIRS
        .iter()
        .any(|dir| root.join(dir).join(name).exists())
}

/// The facts for the image mounted at `root` (`/` in the guest; a temp tree in tests).
pub fn machine_facts_at(root: &Path) -> String {
    let os = os_pretty_name(root).unwrap_or_else(|| FALLBACK_OS_LINE.to_string());
    let mut facts = Vec::new();
    facts.push(format!("- **Operating System:** {os}."));

    let mut runtimes = Vec::new();
    if tool_exists(root, "bun") {
        runtimes.push("`bun` (TypeScript/JavaScript)");
    }
    if tool_exists(root, "node") {
        runtimes.push("`node` (JavaScript)");
    }
    if tool_exists(root, "python3") {
        runtimes.push("`python3` (Python 3)");
    }
    if tool_exists(root, "bash") {
        runtimes.push("`bash` (Bash Shell)");
    } else if tool_exists(root, "sh") {
        runtimes.push("`sh` (Standard Shell)");
    }

    if !runtimes.is_empty() {
        facts.push(format!(
            "- **Available Interpreters:** {}.",
            runtimes.join(", ")
        ));
    }

    let tools: Vec<&str> = ["git", "curl", "jq"]
        .into_iter()
        .filter(|name| tool_exists(root, name))
        .collect();
    if !tools.is_empty() {
        let list: Vec<String> = tools.iter().map(|t| format!("`{t}`")).collect();
        facts.push(format!("- **Command-line tools:** {}.", list.join(", ")));
    }

    facts.join("\n")
}

fn field_str(inputs: Option<&Struct>, key: &str) -> String {
    inputs
        .and_then(|s| s.fields.get(key))
        .and_then(|v| {
            if let Some(prost_types::value::Kind::StringValue(ref s)) = v.kind {
                Some(s.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn tool_present(tools: &[ToolSpec], name: &str) -> bool {
    tools.iter().any(|tool| tool.name == name)
}

fn has_file_tool(tools: &[ToolSpec]) -> bool {
    FILE_TOOL_NAMES.iter().any(|name| tool_present(tools, name))
}

/// Tool-dependent briefing. The static template has no conditionals, so sections
/// that name workspace, file, shell, or todo tools are filled from `tools`.
pub fn environment_guidance(tools: &[ToolSpec]) -> String {
    environment_guidance_with(tools, true)
}

/// `reminders` is the harness switch `context.reminders`: the three default
/// reminders (grep the skill index first, re-read a truncated path, write work
/// into the workspace).
pub fn environment_guidance_with(tools: &[ToolSpec], reminders: bool) -> String {
    let base = environment_guidance_base(tools, reminders);
    if tool_present(tools, "post_message") {
        format!("{base}\n\n{PROGRESS_GUIDANCE}")
    } else {
        base
    }
}

/// One line, only when `post_message` is in the tool list.
const PROGRESS_GUIDANCE: &str = "## Progress\n\nOn a long task, post a short progress message with `post_message` now and then, so the user can follow along.";

fn environment_guidance_base(tools: &[ToolSpec], reminders: bool) -> String {
    let has_file = has_file_tool(tools);
    let has_run = tool_present(tools, "run");
    let has_todo = tool_present(tools, "todo_write");
    let web = web_guidance(tools);
    if !has_file && !has_run && !has_todo {
        return match web {
            Some(web) => web,
            None => NO_LOCAL_TOOLS_GUIDANCE.to_string(),
        };
    }

    let mut sections = Vec::new();
    if has_file || has_run || has_todo {
        sections.push(environment_section(has_file, has_run, has_todo));
    }
    if let Some(habits) = working_habits_section(tools, has_run) {
        sections.push(habits);
    }
    if has_file {
        sections.push(DURABLE_VS_EPHEMERAL.to_string());
        sections.push(durable_work_product(tools));
    }
    if has_run {
        sections.push(MACHINE_FACTS.to_string());
        if tool_present(tools, "write_file") && tool_present(tools, "edit_file") {
            sections.push(WRITE_EXECUTE_FIX.to_string());
        }
        sections.push(COMPOSE_ON_DEMAND.to_string());
    }
    if let Some(act) = how_to_act_section(tools, has_file, has_run, reminders) {
        sections.push(act);
    }
    if let Some(web) = web {
        sections.push(web);
    }
    sections.join("\n\n")
}

/// Research guidance, only when `web_search` is in the tool list. Depends on tool
/// names alone, so it is byte-stable for a given catalog.
const WEB_GUIDANCE: &str = "## Researching the web\n\nSearch when the answer needs current or outside facts. Search more than once with different queries. Fetch the two to four best sources with `web_fetch` and read the saved file for detail. Cite each claim taken from a source as [S<n>], using ids from tool results (for example [S1] or [S1, S3]). Never invent an id. Say so when sources disagree or when nothing was found.";

fn web_guidance(tools: &[ToolSpec]) -> Option<String> {
    tool_present(tools, "web_search").then(|| WEB_GUIDANCE.to_string())
}

fn environment_section(has_file: bool, has_run: bool, has_todo: bool) -> String {
    let mut lines = Vec::new();
    if has_file {
        lines.push(
            "- **Workspace** (read/write): `/mnt/drive9/workspace` — your working directory for all file and shell operations.".to_string(),
        );
        lines.push(
            "- **Scratch** (ephemeral): `/work` — temporary files; lost on VM halt.".to_string(),
        );
        lines.push(
            "- **Skills** (read-only): `/mnt/drive9/skills/` — mounted skill manifests and tool scripts. Do not modify skill files.".to_string(),
        );
    } else if has_run {
        lines.push(
            "- **Scratch** (ephemeral): `/work` — temporary files; lost on VM halt.".to_string(),
        );
    }
    if has_todo {
        lines.push(
            "- **Todos**: `.vam/agent/todos.json` — use the `todo_write` primitive to update."
                .to_string(),
        );
    }
    let mut out = format!("## Environment\n\n{}", lines.join("\n"));
    if has_file {
        out.push_str(
            "\n\nDo **not** modify files under `/mnt/drive9/skills/` or `.vam/threads/` (host timeline metadata).",
        );
    }
    out
}

fn working_habits_section(tools: &[ToolSpec], has_run: bool) -> Option<String> {
    let mut bullets = Vec::new();
    if tool_present(tools, "read_file")
        && (tool_present(tools, "edit_file") || tool_present(tools, "write_file"))
    {
        let writer = if tool_present(tools, "edit_file") && tool_present(tools, "write_file") {
            "`edit_file` or `write_file`"
        } else if tool_present(tools, "edit_file") {
            "`edit_file`"
        } else {
            "`write_file`"
        };
        bullets.push(format!(
            "- **Read before editing.** `read_file` the current contents before {writer} changes a file. Do not edit from memory."
        ));
    }
    if tool_present(tools, "edit_file") {
        bullets.push(
            "- **Prefer surgical edits.** Change only the lines the task needs. Use `edit_file` on an existing file; do not rewrite the whole file when a small change will do.".to_string(),
        );
    }
    if has_run {
        bullets.push(
            "- **Reach the egress proxy for outbound HTTP.** The in-guest proxy listens at `http://127.0.0.1:3128`. Shell commands already have `HTTP_PROXY` and `HTTPS_PROXY` set to that address. `bun` and `node` do not: those runtimes ignore `NO_PROXY` and deadlock if `HTTP_PROXY` points at this loopback proxy, so those variables are cleared. From JavaScript, send the request to `http://127.0.0.1:3128` and set the `X-Target-Url` header to the absolute destination URL. Do not open a raw connection to the host network.".to_string(),
        );
    }
    if bullets.is_empty() {
        return None;
    }
    Some(format!("## Working habits\n\n{}", bullets.join("\n")))
}

fn durable_work_product(tools: &[ToolSpec]) -> String {
    let mut body = String::from(
        "## Durable work product\n\n\
Keep living state on disk instead of pasting large dumps into chat:\n\n\
- **`PLAN.md`** — your living plan: goals, status, and next steps. Update it as work progresses.\n\
- **`notes/`** — durable notes and research dumps (create the directory as needed).\n",
    );
    if tool_present(tools, "read_file") {
        body.push_str(
            "- **`.vam/host-artifacts/`** — large host tool payloads already written for you; `read_file` when a result says `artifact written to …`.\n",
        );
    } else {
        body.push_str(
            "- **`.vam/host-artifacts/`** — large host tool payloads already written for you when a result says `artifact written to …`.\n",
        );
    }
    body.push('\n');

    let full_close = tool_present(tools, "write_file")
        && tool_present(tools, "edit_file")
        && tool_present(tools, "read_file")
        && tool_present(tools, "glob");
    if full_close {
        body.push_str(
            "After a non-trivial research or planning step, `write_file` / `edit_file` the relevant file **before** answering the user. On follow-up runs, `read_file` / `glob` `PLAN.md` and `notes/` instead of relying on replayed chat alone.",
        );
        return body;
    }

    let mut sentences = Vec::new();
    if tool_present(tools, "write_file") && tool_present(tools, "edit_file") {
        sentences.push(
            "After a non-trivial research or planning step, `write_file` / `edit_file` the relevant file **before** answering the user.".to_string(),
        );
    } else if tool_present(tools, "write_file") {
        sentences.push(
            "After a non-trivial research or planning step, `write_file` the relevant file **before** answering the user.".to_string(),
        );
    } else if tool_present(tools, "edit_file") {
        sentences.push(
            "After a non-trivial research or planning step, `edit_file` the relevant file **before** answering the user.".to_string(),
        );
    }
    if tool_present(tools, "read_file") && tool_present(tools, "glob") {
        sentences.push(
            "On follow-up runs, `read_file` / `glob` `PLAN.md` and `notes/` instead of relying on replayed chat alone.".to_string(),
        );
    } else if tool_present(tools, "read_file") {
        sentences.push(
            "On follow-up runs, `read_file` `PLAN.md` and `notes/` instead of relying on replayed chat alone.".to_string(),
        );
    } else if tool_present(tools, "glob") {
        sentences.push(
            "On follow-up runs, `glob` `PLAN.md` and `notes/` instead of relying on replayed chat alone.".to_string(),
        );
    }
    if !sentences.is_empty() {
        body.push_str(&sentences.join(" "));
    }
    body
}

fn how_to_act_section(
    tools: &[ToolSpec],
    has_file: bool,
    has_run: bool,
    reminders: bool,
) -> Option<String> {
    let mut bullets = Vec::new();
    if has_file {
        bullets.push(
            "- Use the tools listed below to solve tasks. Never claim you lack filesystem access when primitives are available.".to_string(),
        );
    } else if has_run {
        bullets.push("- Use the tools listed below to solve tasks.".to_string());
    }
    if tool_present(tools, "load_skill") && (has_file || has_run) {
        bullets.push(
            "- **Load a skill** → `load_skill` with the catalog id **before** calling that skill's tools.".to_string(),
        );
    }
    if tool_present(tools, "read_file") && tool_present(tools, "grep") {
        bullets.push(
            "- **Read extra skill docs** → `read_file` or `grep` with absolute paths under `/mnt/drive9/skills/...` (not auto-injected into context).".to_string(),
        );
    } else if tool_present(tools, "read_file") {
        bullets.push(
            "- **Read extra skill docs** → `read_file` with absolute paths under `/mnt/drive9/skills/...` (not auto-injected into context).".to_string(),
        );
    } else if tool_present(tools, "grep") {
        bullets.push(
            "- **Read extra skill docs** → `grep` with absolute paths under `/mnt/drive9/skills/...` (not auto-injected into context).".to_string(),
        );
    }
    if reminders && tool_present(tools, "grep") && tool_present(tools, "glob") {
        bullets.push(
            "- **Grep the skill index first** → `grep` `INDEX.md` by table or document name under `/mnt/drive9/skills/<skill-id>/` before globbing a large docs tree.".to_string(),
        );
    } else if reminders && tool_present(tools, "grep") {
        bullets.push(
            "- **Grep the skill index first** → `grep` `INDEX.md` by table or document name under `/mnt/drive9/skills/<skill-id>/` before searching a large docs tree.".to_string(),
        );
    }
    if reminders && tool_present(tools, "read_file") {
        bullets.push(
            "- **Re-read a truncated path** → when output is truncated, `read_file` the exact `/mnt/drive9/skills/...` path stamped on the result.".to_string(),
        );
    }
    if tool_present(tools, "write_file") {
        if reminders {
            bullets.push(
            "- **Write work into the workspace** → generated SQL, notes, and other products go under `/mnt/drive9/workspace` with `write_file`; never the read-only skills mount.".to_string(),
            );
        }
        bullets.push(
            "- **Write a note or file** → `write_file` (creates parent directories; paths are workspace-relative with no leading `/`)".to_string(),
        );
    }
    if tool_present(tools, "glob") {
        bullets.push(
            "- **List or find files** → `glob` (optional `path` to scope a skill directory or workspace folder)".to_string(),
        );
    }
    if tool_present(tools, "grep") {
        bullets.push(
            "- **Search file contents** → `grep` (optional `path` to scope search)".to_string(),
        );
    }
    if tool_present(tools, "edit_file") {
        bullets.push("- **Edit existing files** → `edit_file`".to_string());
    }
    if has_run {
        bullets.push("- **Run shell commands** → `run`".to_string());
    }
    if tool_present(tools, "write_file") {
        bullets.push(
            "- Never answer a file-writing request without at least one successful `write_file`."
                .to_string(),
        );
    }
    if bullets.is_empty() {
        return None;
    }
    Some(format!("## How to act\n\n{}", bullets.join("\n")))
}

const DURABLE_VS_EPHEMERAL: &str = "\
## Durable vs ephemeral

Follow-up runs on the **same volume** reuse the Drive9 workspace — files you write under `/mnt/drive9/workspace` (for example `notes/hello.txt`) survive across runs.

These do **not** persist across runs:

- `/work`, `/tmp`, and the guest OS (including `/usr`) — each run gets a fresh microVM rootfs.
- Do not mutate `/usr` or other OS paths expecting changes to stick.

Conversation history for a thread is replayed from host metadata at `.vam/threads/` — you do not need to re-read prior turns from disk yourself.";

const MACHINE_FACTS: &str = "\
## Machine facts & available runtimes

The guest VM is a minimal, lightweight Linux system. The machine facts above name its operating system and the interpreters and tools it has.
Available interpreters you can run via the `run` primitive, when installed:
- **`bun`** — default fast JavaScript/TypeScript engine (includes standard Node modules, file system APIs).
- **`sh` / `bash`** — standard shell execution.
- **`python3`**, **`git`**, **`curl`**, **`jq`** — present on the standard base. Packages are installed when the image is built, never at run time, so `pip install` does not work; use what is installed.
- (Other compilers or runtimes such as `node` or `java` may be available depending on the base image.)";

const WRITE_EXECUTE_FIX: &str = "\
## The Write-Execute-Fix Iterative Loop

You have full capability to write and run code to solve problems. Do not guess—write a verification or execution script. Follow this iterative loop:
1. **Write/Edit:** Create or modify a script in `/mnt/drive9/workspace` using `write_file` or `edit_file`.
2. **Execute:** Run your script via the `run` tool (e.g. `run(command: \"bun run script.ts\")`).
3. **Debug:** Analyze stdout, stderr, and exit codes. A non-zero exit code is **expected, normal, and fully recoverable**. It is a helpful signal to guide your next step.
4. **Fix:** Use `edit_file` to correct the bug, and re-execute. Repeat until the task succeeds.";

const COMPOSE_ON_DEMAND: &str = "\
## Compose-On-Demand Capabilities

If the pre-built skill tools lack a specific action (for example, there is no high-level skill tool to list active sprints on a board, or get board metadata):
- **Do not give up.**
- **Compose your own solution:** Write a script or use general tools (like `http_get` if available) to fetch the required data directly from the API endpoint.
- Refer to any loaded skill instruction files (`/mnt/drive9/skills/<skill-name>/SKILLS.md` or similar) to understand endpoint formats and base paths.";

/// Builds the rendered system prompt from the template, run inputs, tool inventory, catalog, and skill extensions.
/// A playbook the host offers the model (run input `attached_playbooks`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct OfferedPlaybook {
    pub urn: String,
    pub name: String,
    pub when: String,
    #[serde(default)]
    pub default: bool,
}

/// The offered playbooks from run inputs, host order (default first).
pub fn attached_playbooks(inputs: Option<&Struct>) -> Vec<OfferedPlaybook> {
    let text = field_str(inputs, "attached_playbooks");
    if text.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

/// Prompt section naming the playbooks the model may start. Present only while
/// `start_playbook` is in the tool list, so it goes away once one is bound.
fn playbook_offer_section(inputs: Option<&Struct>, tools: &[ToolSpec]) -> String {
    if !tools.iter().any(|tool| tool.name == "start_playbook") {
        return String::new();
    }
    let offered = attached_playbooks(inputs);
    if offered.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n## Playbooks you can start\nA playbook is a step-by-step procedure for one kind of job. When the user's request matches one below, call start_playbook with its urn, and with inputs taken from the request if it takes any. Do not start one for a greeting or an unrelated request. The user may be asked to confirm.\n",
    );
    for playbook in &offered {
        let main = if playbook.default { " (main job)" } else { "" };
        out.push_str(&format!(
            "- {}{main}, urn {}: {}\n",
            playbook.name, playbook.urn, playbook.when
        ));
    }
    out
}

pub fn build_system_prompt(
    workspace_root: &Path,
    inputs: Option<&Struct>,
    tools: &[ToolSpec],
    skills_catalog: &str,
    skill_extensions: &str,
) -> String {
    build_system_prompt_with(
        workspace_root,
        inputs,
        tools,
        skills_catalog,
        skill_extensions,
        true,
    )
}

pub fn build_system_prompt_with(
    workspace_root: &Path,
    inputs: Option<&Struct>,
    tools: &[ToolSpec],
    skills_catalog: &str,
    skill_extensions: &str,
    reminders: bool,
) -> String {
    let template = load_template_source(workspace_root);
    let inventory = format_tools_inventory(tools);
    let facts = detect_machine_facts();
    let guidance = environment_guidance_with(tools, reminders);
    let ctx = RenderContext {
        display_name: &field_str(inputs, "agent_display_name"),
        role: &field_str(inputs, "agent_role"),
        system_instructions: &field_str(inputs, "system_instructions"),
        tools_inventory: &inventory,
        skill_extensions,
        skills_catalog,
        machine_facts: &facts,
        environment_guidance: &guidance,
        playbook_procedure: &field_str(inputs, "playbook_procedure"),
    };
    let mut prompt = render_template(&template, &ctx);
    prompt.push_str(&playbook_offer_section(inputs, tools));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn reminders_off_drops_exactly_the_three_reminders() {
        let tools = crate::primitives::builtin_tool_specs();
        let on = environment_guidance_with(&tools, true);
        assert_eq!(on, environment_guidance(&tools));
        let off = environment_guidance_with(&tools, false);
        for gone in [
            "Grep the skill index first",
            "Re-read a truncated path",
            "Write work into the workspace",
        ] {
            assert!(on.contains(gone), "{gone}");
            assert!(!off.contains(gone), "{gone}");
        }
        assert!(off.contains("Write a note or file"));
    }

    #[test]
    fn web_guidance_appears_only_with_web_search_and_is_byte_stable() {
        let plain = crate::primitives::builtin_tool_specs();
        assert!(!environment_guidance(&plain).contains("Researching the web"));
        let mut with_web = plain.clone();
        with_web.push(named_tool("web_search"));
        with_web.push(named_tool("web_fetch"));
        let first = environment_guidance(&with_web);
        assert!(first.contains("## Researching the web"));
        assert!(first.contains("[S<n>]"));
        assert_eq!(first, environment_guidance(&with_web));
        // Only web_fetch: no block.
        assert!(!environment_guidance(&[named_tool("web_fetch")]).contains("Researching"));
    }

    #[test]
    fn web_only_agent_gets_web_block_not_the_no_tools_text() {
        let tools = vec![named_tool("web_search"), named_tool("web_fetch")];
        let text = environment_guidance(&tools);
        assert!(text.contains("## Researching the web"));
        assert!(!text.contains("no file, shell or web tools"));
    }

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn parse_template_version_from_frontmatter() {
        assert_eq!(
            parse_template_version(DEFAULT_TEMPLATE),
            Some(DEFAULT_TEMPLATE_VERSION)
        );
        assert_eq!(parse_template_version("# no frontmatter\n"), None);
    }

    #[test]
    fn render_includes_role_instructions_and_inventory() {
        let tools = vec![ToolSpec {
            name: "read_file".to_string(),
            description: "Read a file from the workspace.".to_string(),
            input_schema_json: String::new(),
            output_schema_json: String::new(),
            side_effect: 0,
            timeout: None,
            execution_provider: None,
            required_secrets: vec![],
            ..Default::default()
        }];
        let inventory = format_tools_inventory(&tools);
        let ctx = RenderContext {
            display_name: "Test Agent",
            role: "Assistant",
            system_instructions: "Be helpful.",
            tools_inventory: &inventory,
            skill_extensions: "Use calculator for math.",
            skills_catalog: "- **Calculator** (`assistant-tools`): Math tools",
            machine_facts: "",
            environment_guidance: "",
            playbook_procedure: "",
        };
        let rendered = render_template(DEFAULT_TEMPLATE, &ctx);
        assert!(rendered.contains("Test Agent"));
        assert!(rendered.contains("Assistant"));
        assert!(rendered.contains("Be helpful."));
        assert!(rendered.contains("read_file"));
        assert!(rendered.contains("Use calculator for math."));
        assert!(rendered.contains("assistant-tools"));
        assert!(!rendered.contains("template_version"));
    }

    #[test]
    fn render_v5_includes_load_skill_guidance() {
        let tools = vec![ToolSpec {
            name: "load_skill".to_string(),
            description: "Load a skill.".to_string(),
            input_schema_json: String::new(),
            output_schema_json: String::new(),
            side_effect: 0,
            timeout: None,
            execution_provider: None,
            required_secrets: vec![],
            ..Default::default()
        }];
        let inventory = format_tools_inventory(&tools);
        let ctx = RenderContext {
            display_name: "Test Agent",
            role: "Bun Assistant",
            system_instructions: "Be helpful.",
            tools_inventory: &inventory,
            skill_extensions: "",
            skills_catalog: "- **Calculator** (`assistant-tools`): Math",
            machine_facts: "",
            environment_guidance: "",
            playbook_procedure: "",
        };
        let rendered = render_template(DEFAULT_TEMPLATE, &ctx);
        assert_eq!(
            parse_template_version(DEFAULT_TEMPLATE),
            Some(DEFAULT_TEMPLATE_VERSION)
        );
        assert!(rendered.contains("load_skill"));
        assert!(rendered.contains("Available skills"));
        assert!(rendered.contains("tool_error:"));
        assert!(!rendered.contains("- **write_note**"));
    }

    #[test]
    fn render_v6_includes_durable_work_product() {
        let guidance = environment_guidance(&crate::primitives::builtin_tool_specs());
        let rendered = render_template(
            DEFAULT_TEMPLATE,
            &RenderContext {
                display_name: "Test Agent",
                role: "Assistant",
                system_instructions: "",
                tools_inventory: "",
                skill_extensions: "",
                skills_catalog: "",
                machine_facts: "",
                environment_guidance: &guidance,
                playbook_procedure: "",
            },
        );
        assert!(rendered.contains("Durable work product"));
        assert!(rendered.contains("PLAN.md"));
        assert!(rendered.contains("notes/"));
    }

    #[test]
    fn missing_drive9_file_uses_embedded_fallback() {
        let _guard = test_guard();
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let loaded = load_template_source(&ws);
        assert_eq!(loaded, DEFAULT_TEMPLATE);
        let tools = crate::primitives::builtin_tool_specs();
        let prompt = build_system_prompt(&ws, None, &tools, "_No skills mounted._", "");
        assert!(prompt.contains("write_file"));
        assert!(prompt.contains("load_skill"));
    }

    #[test]
    fn inventory_line_count_matches_tool_count() {
        let tools = crate::primitives::builtin_tool_specs();
        let inventory = format_tools_inventory(&tools);
        let lines: Vec<&str> = inventory.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), tools.len());
        for tool in &tools {
            assert!(inventory.contains(&tool.name));
        }
    }

    #[test]
    fn render_v7_includes_machine_facts_and_iterate_loop() {
        let guidance = environment_guidance(&crate::primitives::builtin_tool_specs());
        let rendered = render_template(
            DEFAULT_TEMPLATE,
            &RenderContext {
                display_name: "Test Agent",
                role: "Assistant",
                system_instructions: "",
                tools_inventory: "",
                skill_extensions: "",
                skills_catalog: "",
                machine_facts: "- Test facts: bun present",
                environment_guidance: &guidance,
                playbook_procedure: "",
            },
        );
        assert!(rendered.contains("Machine facts & available runtimes"));
        assert!(rendered.contains("The Write-Execute-Fix Iterative Loop"));
        assert!(rendered.contains("Compose-On-Demand Capabilities"));
        // Confirm dynamic facts replaced placeholder
        assert!(!rendered.contains("{{machine.facts}}"));
    }

    #[test]
    fn render_v9_includes_large_doc_navigation_bullets() {
        let guidance = environment_guidance(&crate::primitives::builtin_tool_specs());
        let rendered = render_template(
            DEFAULT_TEMPLATE,
            &RenderContext {
                display_name: "Test Agent",
                role: "Assistant",
                system_instructions: "",
                tools_inventory: "",
                skill_extensions: "",
                skills_catalog: "",
                machine_facts: "",
                environment_guidance: &guidance,
                playbook_procedure: "",
            },
        );
        assert_eq!(
            parse_template_version(DEFAULT_TEMPLATE),
            Some(DEFAULT_TEMPLATE_VERSION)
        );
        assert_eq!(DEFAULT_TEMPLATE_VERSION, 11);
        assert!(rendered.contains("Read before editing"));
        assert!(rendered.contains("Prefer surgical edits"));
        assert!(rendered.contains("http://127.0.0.1:3128"));
        assert!(rendered.contains("Grep the skill index first"));
        assert!(rendered.contains("INDEX.md"));
        assert!(rendered.contains("Re-read a truncated path"));
        assert!(rendered.contains("/mnt/drive9/skills/"));
        assert!(rendered.contains("Write work into the workspace"));
        assert!(rendered.contains("/mnt/drive9/workspace"));
        assert!(!rendered.contains("{{machine.facts}}"));
        let index_hits = rendered.matches("INDEX.md").count();
        assert_eq!(index_hits, 1, "do not inject the index corpus");
    }

    #[test]
    fn render_v10_includes_playbook_procedure() {
        let rendered = render_template(
            DEFAULT_TEMPLATE,
            &RenderContext {
                display_name: "Test Agent",
                role: "Assistant",
                system_instructions: "Stay in role. {{playbook.procedure}}",
                tools_inventory: "",
                skill_extensions: "",
                skills_catalog: "",
                machine_facts: "",
                environment_guidance: "",
                playbook_procedure: "Search PROJ.\n{{agent.system_instructions}}",
            },
        );
        assert_eq!(DEFAULT_TEMPLATE_VERSION, 11);
        assert!(rendered.contains("Playbook procedure"));
        assert!(rendered.contains("Search PROJ."));
        assert!(!rendered.contains("{{playbook.procedure}}"));
        assert!(rendered.contains("{{agent.system_instructions}}"));
        assert!(rendered.contains("Stay in role."));
    }

    fn touch_exec(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
    }

    #[test]
    fn facts_for_the_standard_base_use_os_release_and_list_the_toolchain() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc")).unwrap();
        fs::write(
            root.path().join("etc/os-release"),
            "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nNAME=\"Debian GNU/Linux\"\n",
        )
        .unwrap();
        for rel in [
            "usr/local/bin/bun",
            "usr/bin/python3",
            "usr/bin/git",
            "usr/bin/curl",
            "usr/bin/jq",
            "usr/bin/bash",
        ] {
            touch_exec(root.path(), rel);
        }
        let facts = machine_facts_at(root.path());
        assert!(facts.contains("**Operating System:** Debian GNU/Linux 13 (trixie)."));
        assert!(!facts.contains("musl"));
        assert!(facts.contains("`bun`"));
        assert!(facts.contains("`python3` (Python 3)"));
        assert!(facts.contains("**Command-line tools:** `git`, `curl`, `jq`."));
    }

    #[test]
    fn facts_for_the_minimal_base_change_only_the_os_line() {
        let root = tempfile::tempdir().unwrap();
        touch_exec(root.path(), "usr/local/bin/bun");
        touch_exec(root.path(), "bin/sh");
        let facts = machine_facts_at(root.path());
        assert_eq!(
            facts,
            "- **Operating System:** Minimal Linux (musl/busybox userland).\n- **Available Interpreters:** `bun` (TypeScript/JavaScript), `sh` (Standard Shell)."
        );
    }

    #[test]
    fn facts_list_only_the_tools_that_are_present() {
        let root = tempfile::tempdir().unwrap();
        touch_exec(root.path(), "usr/bin/git");
        touch_exec(root.path(), "usr/bin/jq");
        let facts = machine_facts_at(root.path());
        assert!(facts.contains("**Command-line tools:** `git`, `jq`."));
        assert!(!facts.contains("`curl`"));
        assert!(!facts.contains("python3"));
    }

    #[test]
    fn os_release_without_a_pretty_name_falls_back() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc")).unwrap();
        fs::write(
            root.path().join("etc/os-release"),
            "NAME=Foo\nPRETTY_NAME=\n",
        )
        .unwrap();
        let facts = machine_facts_at(root.path());
        assert!(facts.contains("Minimal Linux (musl/busybox userland)."));
        // single quotes are accepted too
        fs::write(
            root.path().join("etc/os-release"),
            "PRETTY_NAME='Alpine Linux v3.20'\n",
        )
        .unwrap();
        assert!(machine_facts_at(root.path()).contains("Alpine Linux v3.20."));
    }

    #[test]
    fn test_detect_machine_facts_is_not_empty() {
        let facts = detect_machine_facts();
        assert!(facts.contains("Operating System"));
        assert!(facts.contains("-"));
    }

    fn named_tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: format!("{name} tool"),
            input_schema_json: String::new(),
            output_schema_json: String::new(),
            side_effect: 0,
            timeout: None,
            execution_provider: None,
            required_secrets: vec![],
            ..Default::default()
        }
    }

    /// `run_step` and the word "running" are not the shell tool `run`.
    fn mentions_ident(prompt: &str, ident: &str) -> bool {
        prompt
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word == ident)
    }

    fn prompt_for(tools: &[ToolSpec]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        build_system_prompt(&ws, None, tools, "", "")
    }

    #[test]
    fn playbook_control_tools_omit_file_shell_guidance() {
        let prompt = prompt_for(&[
            named_tool("step_complete"),
            named_tool("run_step"),
            named_tool("check_playbook_finish"),
        ]);
        assert!(prompt.contains(NO_LOCAL_TOOLS_GUIDANCE));
        assert!(!prompt.contains("glob"));
        assert!(!prompt.contains("read_file"));
        assert!(!prompt.contains("/mnt/drive9/workspace"));
        assert!(!mentions_ident(&prompt, "run"));
        assert!(!prompt.contains("`run`"));
    }

    #[test]
    fn builtin_tools_include_file_guidance() {
        let prompt = prompt_for(&crate::primitives::builtin_tool_specs());
        assert!(prompt.contains("/mnt/drive9/workspace"));
        assert!(prompt.contains("read_file"));
        assert!(prompt.contains("glob"));
        assert!(prompt.contains("Durable work product"));
        assert!(prompt.contains("Read before editing"));
        assert!(mentions_ident(&prompt, "run"));
        assert!(!prompt.contains(NO_LOCAL_TOOLS_GUIDANCE));
    }

    #[test]
    fn run_without_file_tools_omits_workspace_guidance() {
        let prompt = prompt_for(&[named_tool("run")]);
        assert!(prompt.contains("Machine facts & available runtimes"));
        assert!(prompt.contains("`run`"));
        assert!(!prompt.contains("/mnt/drive9/workspace"));
        assert!(!prompt.contains("read_file"));
        assert!(!prompt.contains("glob"));
        assert!(!prompt.contains(NO_LOCAL_TOOLS_GUIDANCE));
    }

    #[test]
    fn file_tools_without_run_omit_shell_guidance() {
        let prompt = prompt_for(&[
            named_tool("read_file"),
            named_tool("write_file"),
            named_tool("edit_file"),
            named_tool("glob"),
            named_tool("grep"),
        ]);
        assert!(prompt.contains("/mnt/drive9/workspace"));
        assert!(prompt.contains("read_file"));
        assert!(prompt.contains("glob"));
        assert!(!prompt.contains("Machine facts & available runtimes"));
        assert!(!prompt.contains("The Write-Execute-Fix Iterative Loop"));
        assert!(!prompt.contains("`run`"));
        assert!(!prompt.contains("Run shell commands"));
    }

    #[test]
    fn override_template_without_environment_placeholder_still_renders() {
        let _guard = test_guard();
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(ws.join(".vam")).unwrap();
        std::fs::write(
            ws.join(".vam/SYSTEM.md"),
            "---\ntemplate_version: 4\n---\n\n# {{agent.display_name}}\n\nLegacy body stays.\n\n{{tools.inventory}}\n\n{{mystery.slot}}\n",
        )
        .unwrap();
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "agent_display_name".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue("Legacy Agent".into())),
            },
        );
        let inputs = prost_types::Struct { fields };
        let tools = vec![
            named_tool("step_complete"),
            named_tool("run_step"),
            named_tool("check_playbook_finish"),
        ];
        let prompt = build_system_prompt(&ws, Some(&inputs), &tools, "", "");
        assert!(prompt.contains("Legacy Agent"));
        assert!(prompt.contains("Legacy body stays."));
        assert!(prompt.contains("**step_complete**"));
        assert!(prompt.contains("{{mystery.slot}}"));
        assert!(!prompt.contains("{{environment.guidance}}"));
        assert!(!prompt.contains(NO_LOCAL_TOOLS_GUIDANCE));
    }
}
