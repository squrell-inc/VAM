// ==============================================================================
// VAM Cedar Security Policy: Jira Assistant
// URN: urn:vam:policy:local.vam.dev/vam/jira-egress-only
// ==============================================================================
// Sample on disk under examples/. Not copied into the product catalog at vamd boot.

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"jira_search_issues",
        Tool::"jira_add_comment",
        Tool::"http_get",
        Tool::"read_file",
        Tool::"glob",
        Tool::"grep",
        Tool::"load_skill"
    ].contains(resource)
};

// Mutating Jira tools require host HITL approval (two-pass Suspend at execute time).
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"jira_create_issue",
        Tool::"jira_transition_issue",
        Tool::"http_write"
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
