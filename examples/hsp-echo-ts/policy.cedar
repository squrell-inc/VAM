// ==============================================================================
// VAM Cedar Security Policy: HSP echo-ts guest invoke
// URN: urn:vam:policy:local.vam.dev/vam/hsp-echo-ts
// ==============================================================================

permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"load_skill"
);

permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"echo.ping"
);
