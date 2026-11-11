// ==============================================================================
// VAM Cedar Security Policy: SQLite Memory & Code Analyst Assistant
// URN: urn:vam:policy:local.vam.dev/vam/sqlite-memory-only
// ==============================================================================
// Sample on disk under examples/. Not copied into the product catalog at vamd boot.

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/sqlite-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"init_db",
        Tool::"insert_fact",
        Tool::"insert_code_note",
        Tool::"read_file",
        Tool::"write_file",
        Tool::"edit_file",
        Tool::"glob",
        Tool::"todo_write",
        Tool::"run",
        Tool::"load_skill"
    ].contains(resource)
};

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/sqlite-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"query_db"
);

forbid (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/sqlite-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"query_db"
)
when {
    context has query &&
    (
        !(context.query like "*SELECT*") ||
        context.query like "*DROP*" ||
        context.query like "*DELETE*" ||
        context.query like "*TRUNCATE*" ||
        context.query like "*ALTER*"
    )
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
