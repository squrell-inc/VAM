// ==============================================================================
// VAM Cedar Security Policy: HSP files-ts guest invoke
// URN: urn:vam:policy:local.vam.dev/vam/hsp-files-ts
// ==============================================================================

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
        Tool::"files.put",
        Tool::"files.list",
        Tool::"files.get"
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
    context.path like "/mnt/drive9/workspace/*"
};
