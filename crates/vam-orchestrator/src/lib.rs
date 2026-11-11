//! Durable host-owned workflow orchestration and session state for VAM.
//!
//! This crate contains the orchestrator logic (`vamd`), database state management,
//! systemd daemon supervision loops, and session state engines.

// A panic in the daemon is a defect (IT-2.7): request paths return typed errors, startup exits
// with a one-line message. Tests may still unwrap.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod allowed_models;
pub mod api;
pub mod brokers;
pub mod caller_context;
pub mod catalog;
pub mod catalog_freeze;
pub mod catalog_publish;
pub mod catalog_resolve;
pub mod catalog_suggest;
pub mod catalog_tier;
pub mod connection_egress;
pub mod connector_park;
pub mod connectors;
pub mod contacts;
pub mod credentials;
pub mod daemon;
pub mod driver;
pub mod effect_class;
pub mod effect_key;
pub mod effect_path;
pub mod effect_reconcile;
pub mod effect_store;
pub mod egress;
pub mod error;
pub mod events;
pub mod evidence;
pub mod first_key;
pub mod gateway;
pub mod golden;
pub mod grants;
pub mod guest_stream;
pub mod hitl;
pub mod host_tools;
pub mod host_tools_fixtures;
pub mod host_tools_http;
pub mod hsp;
pub mod hsp_echo;
pub mod hsp_echo_effects;
pub mod hsp_hooks;
pub mod hsp_registry;
pub mod hsp_store;
pub mod inbox;
pub mod inbox_store;
pub mod isolation;
pub mod keys;
pub mod manager;
pub mod marketplace;
pub mod mcp_client;
pub(crate) mod mcp_oauth_refresh;
pub mod mcp_registry;
pub mod metrics_scrape;
pub mod node_base;
pub mod node_notify;
pub mod notification_prefs;
pub mod notifications;
pub mod notifications_tool;
pub mod notify_delivery;
pub mod notify_gateway;
pub mod outbox;
pub mod outcomes;
pub mod owner;
#[cfg(test)]
mod paging_store_tests;
pub mod password;
pub mod pep_capture;
pub mod permissions_migrate;
pub mod permissions_view;
pub mod playbook_activation;
pub mod playbook_flight;
pub mod playbook_test;
pub mod policy_legacy;
pub mod preflight;
pub mod receipt;
pub mod reconcile;
pub mod registry;
pub mod resume;
pub mod resume_decisions;
pub mod resume_store;
pub mod run_checkpoint;
pub mod run_context;
pub mod run_facts;
pub mod secret_requirements;
pub mod secrets;
pub mod security;
pub mod session;
pub mod session_end;
pub mod session_targets;
pub mod session_tokens;
pub mod signing;
pub mod skill_cards;
pub mod skill_package_admit;
pub mod skill_package_resolve;
pub mod skill_packages;
pub mod spaces;
pub mod ttl;
pub use vam_security::ssrf_guard;
pub mod thread_context;
pub mod thread_recorder;
pub mod timing;
pub mod tls;
pub mod trajectory;
pub mod turn_commit;
pub mod users;
pub mod web_admission;
pub mod web_sources;
pub mod workflow;

#[cfg(feature = "process-guest")]
pub mod process_mode;

/// `GET /v1/node` `guestMode`: `"vm"` on every normal node, `"process"` only on
/// a node started in process-guest mode (feature build plus `VAMD_PROCESS_GUEST_BIN`).
pub fn guest_mode() -> &'static str {
    #[cfg(feature = "process-guest")]
    if process_mode::is_active() {
        return "process";
    }
    "vm"
}

pub type VmmFactory = std::sync::Arc<dyn Fn(&str, u32) -> Box<dyn vam_core::Vmm> + Send + Sync>;

pub use caller_context::{
    assert_identity_satisfied, identity_requirement_is_subset, resolve_secrets_for_caller,
    stamp_caller_context,
};
pub use error::{
    ApprovalRecord, AttemptKey, AttemptState, OrchestratorError, OutboxEvent, StoredApproval,
};
pub use guest_stream::GuestStreamReceiver;
pub use hitl::{HitlApprover, REVIEWER_CEDAR_TYPE, REVIEWER_PRINCIPAL_PREFIX};
pub use hsp::{HspClient, HspEndpoint};
pub use hsp_hooks::{
    HookTimeouts, HspHookDispatcher, OnCompleteDispatchResult, OnRequestDispatchResult,
    OnResponseDispatchResult, EVENT_HOOK_COMPLETED, EVENT_HOOK_DEGRADED, EVENT_HOOK_FAILED,
};
pub use hsp_registry::{assert_invoke_egress, assert_trust_transport, HspRegistry};
pub use hsp_store::{seed_hsp_skills, HspBrokerApproval, HspBrokerToolSnapshot};
pub use keys::{
    AuthSource, BROKERS_ADMIN_SCOPE, KEYS_ADMIN_SCOPE, PUBLISH_GRANT_SCOPE, USERS_ADMIN_SCOPE,
};
pub use owner::{
    owner_user_id_for_authenticated_key, owner_user_id_for_key, BOOTSTRAP_KEY_ID,
    BOOTSTRAP_PRINCIPAL,
};
pub use password::{
    apply_admin_password_reset, apply_headless_admin_bootstrap, hash_password, verify_password,
    PasswordError, BOOTSTRAP_ADMIN_ENV, BOOTSTRAP_ADMIN_PASSWORD_ENV, RESET_PASSWORD_ENV,
    RESET_PASSWORD_USER_ENV,
};
pub use secret_requirements::{MissingRequirement, RequirementScope, SatisfiedScope};
pub use secrets::{HostSecretMetadata, SecretKind, SecretScope, SecretsVault, ORG_OWNER_USER_ID};
pub use security::{
    derive_hitl_overlay, derive_playbook_overlay, resolve_policy_for_urn, resolve_session_policy,
    PolicyResolutionKind, ResolvedSessionPolicy, SessionPolicyMode, SessionSecurityContext,
    DEFAULT_BUILTIN_FALLBACK_POLICY, EVENT_POLICY_RESOLUTION, PLAYBOOK_FINISH_CHECK_OVERLAY,
    PLAYBOOK_RUN_STEP_OVERLAY, PLAYBOOK_STEP_COMPLETE_OVERLAY,
};
pub use session::{ToolCatalogCache, ToolCatalogEntry};
pub use session_tokens::{
    IssuedSession, ACCESS_TOKEN_PREFIX, DEFAULT_ACCESS_TTL_SECS, DEFAULT_REFRESH_TTL_SECS,
    ENV_ACCESS_TTL, ENV_REFRESH_TTL, REFRESH_TOKEN_PREFIX,
};
pub use skill_packages::{
    is_owner_namespaced_blob_relpath, skill_blob_filename, skill_blob_relpath, skill_catalog_root,
};
pub use users::{
    UserRecord, UserRole, LOCAL_USERNAME, LOCAL_USER_ID, OPERATOR_USERNAME, OPERATOR_USER_ID,
};
pub use workflow::SqliteWorkflowStore;
