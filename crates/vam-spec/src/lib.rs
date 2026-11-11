pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/vam.v1.rs"));
}

pub mod args_digest;
pub mod call_check;
pub mod compaction;
pub mod connector;
pub mod connector_listing;
pub mod encoding;
pub mod extends;
pub mod harness;
pub mod jig;
pub mod marketplace;
pub mod origin;
pub mod outbox;
pub mod package;
pub mod permissions;
pub mod playbook;
pub mod playbook_inputs;
pub mod receipt;
pub mod runtime_requirement;
pub mod schema_format;
pub mod skill_cards;
pub mod skill_egress;
pub mod skill_entrypoint;
pub mod skill_frontmatter;
pub mod text_tool_calls;
pub mod thread_context;
pub mod validation;

pub use extends::{merge_agent_chain, AgentMergeError, MergedAgent, MAX_EXTENDS_DEPTH};
pub use playbook::required_skill_refs;
pub use playbook_inputs::{
    parse_playbook_inputs, playbook_run_output_json, validate_playbook_inputs,
    validate_playbook_run_output, validate_playbook_step_output, PlaybookInputError,
};

pub use skill_entrypoint::{
    resolve_manifest_relative_entrypoint, validate_manifest_relative_entrypoint,
    SkillEntrypointError,
};

pub use args_digest::{canonical_args_digest, canonical_args_json, ArgsDigestError};

pub use permissions::{
    decision_strictness, host_default_strictness, match_host, mcp_server_id,
    tool_default_strictness, validate_agent_permissions, EffectiveDefaults, HostPattern,
    McpServerIdError, Specificity, Strictness, Target, TargetParseError, ToolSelector,
};

pub use validation::{
    hosted_adapter_id, is_local_skill_registry, mcp_adapter_id, parse_hosted_adapter_id,
    parse_mcp_adapter_id, parse_skill_urn,
};

pub use origin::{
    default_namespace, Origin, RegistryDomainError, RegistryDomains, DEFAULT_MARKETPLACE_DOMAIN,
    LOCAL_REGISTRY_DOMAIN,
};
