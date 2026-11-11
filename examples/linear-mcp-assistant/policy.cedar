// ==============================================================================
// VAM Cedar Security Policy: Linear MCP Assistant
// URN: urn:vam:policy:local.vam.dev/vam/linear-mcp-assistant
// ==============================================================================
// Sample on disk under examples/. Not copied into the product catalog at vamd boot.
// Curated allowlist only — expand the .contains lists when you want more of the
// ~59 pinned linear__* tools. There is no Tool::"linear__*" wildcard.

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/linear-mcp-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"load_skill",
        Tool::"read_file",
        Tool::"glob",
        Tool::"grep",
        Tool::"write_file",
        Tool::"todo_write",
        Tool::"linear__get_issue",
        Tool::"linear__list_issues"
    ].contains(resource)
};

// Mutating Linear tools require host HITL approval (default side_effect 3).
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/linear-mcp-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"linear__create_issue"
    ].contains(resource) &&
    context has hitl_approved &&
    context.hitl_approved == true
};

permit (
    principal,
    action == Action::"ReadFile",
    resource
)
when {
    context has path &&
    (context.path like "/mnt/drive9/workspace/*" ||
     context.path like "/mnt/drive9/skills/*")
};

permit (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path &&
    context.path like "/mnt/drive9/workspace/*" &&
    !(context.path like "/mnt/drive9/skills/*") &&
    context has file_size_bytes &&
    context.file_size_bytes <= 10485760
};

forbid (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path && context.path like "/mnt/drive9/skills/*"
};
