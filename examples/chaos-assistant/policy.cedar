// ==============================================================================
// VAM Cedar Security Policy: Chaos & Boundary Testing Assistant
// URN: urn:vam:policy:local.vam.dev/vam/chaos-isolated
// ==============================================================================
// Sample on disk under examples/. Not copied into the product catalog at vamd boot.

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/chaos-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"probe_sandbox"
);

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/chaos-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"check_env"
);

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/chaos-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"sleep_hang"
);

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/chaos-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"read_file"
);

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/chaos-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"glob"
);

permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/chaos-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"load_skill"
);

forbid (
    principal,
    action == Action::"ExecuteTool",
    resource
)
when {
    context has side_effect &&
    context.side_effect == 3 // SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL
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
