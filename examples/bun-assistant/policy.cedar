// ==============================================================================
// VAM Cedar Security Policy: Bun Assistant
// URN: urn:vam:policy:local.vam.dev/vam/assistant-egress-only
// ==============================================================================
// Sample on disk under examples/. Not copied into the product catalog at vamd boot.

permit (
    principal,
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"read_file",
        Tool::"write_file",
        Tool::"edit_file",
        Tool::"delete_file",
        Tool::"glob",
        Tool::"grep",
        Tool::"run",
        Tool::"todo_write",
        Tool::"load_skill",
        Tool::"calculator"
    ].contains(resource)
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
