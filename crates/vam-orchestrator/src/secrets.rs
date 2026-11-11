use std::path::{Path, PathBuf};
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use rusqlite::{params, OptionalExtension};
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use vam_inference::SecretLookup;
use vam_network::SecretProvider;
use vam_spec::v1::{InjectionSite, InjectionSiteType, SecretBinding};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

pub const MASTER_KEY_ENV: &str = "VAMD_SECRETS_MASTER_KEY";
pub const DEFAULT_MASTER_KEY_PATH: &str = "/var/lib/vam/secrets.key";
pub const IMPORT_ENV_SECRETS: &str = "VAMD_IMPORT_ENV_SECRETS";

pub const BINDING_OPENAI: &str = "openai/api-key";
pub const BINDING_ANTHROPIC: &str = "anthropic/api-key";
pub const BINDING_OPENROUTER: &str = "openrouter/api-key";
pub const BINDING_VERCEL: &str = "vercel/api-key";
pub const BINDING_JIRA_BASE_URL: &str = "jira/base-url";
pub const BINDING_JIRA_AUTH_HEADER: &str = "jira/auth-header";
pub const BINDING_HRMS_API_URL: &str = "hrms/api-url";
pub const BINDING_HRMS_API_KEY: &str = "hrms/api-key";

/// Guest-visible sentinels. Must match guest skill executors (Jira / HRMS).
pub const PLACEHOLDER_JIRA_BASE_URL: &str = "vamsec_jira_base_url";
pub const PLACEHOLDER_JIRA_AUTH_TOKEN: &str = "vamsec_jira_auth_token";
pub const PLACEHOLDER_HRMS_API_URL: &str = "vamsec_hrms_api_url";
pub const PLACEHOLDER_HRMS_API_KEY: &str = "vamsec_hrms_api_key";

pub const WELL_KNOWN_INFERENCE: &[(&str, &str)] = &[
    (BINDING_OPENAI, "OPENAI_API_KEY"),
    (BINDING_ANTHROPIC, "ANTHROPIC_API_KEY"),
    (BINDING_OPENROUTER, "OPENROUTER_API_KEY"),
    (BINDING_VERCEL, "AI_GATEWAY_API_KEY"),
];

/// Guest placeholders that do not follow the vault-id convention
/// (`hrms/api-key` → `vamsec_hrms_api_key`). Other egress rows still attach.
pub struct WellKnownEgressBinding {
    pub binding_id: &'static str,
    pub placeholder: &'static str,
    pub inject_type: i32,
    pub inject_key: &'static str,
    pub default_destinations: &'static [&'static str],
    pub env_vars: &'static [&'static str],
}

pub const WELL_KNOWN_EGRESS: &[WellKnownEgressBinding] = &[
    WellKnownEgressBinding {
        binding_id: BINDING_JIRA_BASE_URL,
        placeholder: PLACEHOLDER_JIRA_BASE_URL,
        inject_type: InjectionSiteType::InjectionSiteQueryParam as i32,
        inject_key: "",
        default_destinations: &["*.atlassian.net", "*.jira.com"],
        env_vars: &["JIRA_BASE_URL"],
    },
    WellKnownEgressBinding {
        binding_id: BINDING_JIRA_AUTH_HEADER,
        placeholder: PLACEHOLDER_JIRA_AUTH_TOKEN,
        inject_type: InjectionSiteType::InjectionSiteHeader as i32,
        inject_key: "Authorization",
        default_destinations: &["*.atlassian.net", "*.jira.com"],
        env_vars: &["JIRA_AUTH_HEADER", "JIRA_API_TOKEN"],
    },
];

/// Hostname from a vault URL secret (`https://acme.atlassian.net` or `acme.atlassian.net/path`).
pub fn host_from_url_secret(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let host = without_scheme
        .split('/')
        .next()?
        .split(':')
        .next()?
        .trim()
        .trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() || host.contains(char::is_whitespace) {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Is every host the pinned `destination` can match inside the broker's
/// approved `domains`? Domains follow the HSP rule (the domain itself and its
/// subdomains). A destination is an exact host or a `*.base` pattern (strict
/// subdomains of `base`); a bare `*` is never inside a domain list.
pub fn destination_within_domains(destination: &str, domains: &[String]) -> bool {
    let destination = destination.trim().to_ascii_lowercase();
    let base = destination.strip_prefix("*.").unwrap_or(&destination);
    if base.is_empty() || base.contains('*') {
        return false;
    }
    domains.iter().any(|domain| {
        let domain = domain.trim().to_ascii_lowercase();
        !domain.is_empty()
            && !domain.contains('*')
            && (base == domain || base.ends_with(&format!(".{domain}")))
    })
}

/// May a secret pinned to `destinations` be sent to `host`? Same rule as the
/// egress gate: an exact host, a `*.base` pattern (strict subdomains) or `*`.
/// An empty list allows nothing.
pub fn destination_allows_host(destinations: &[String], host: &str) -> bool {
    let host = host.trim().trim_end_matches('.');
    destinations
        .iter()
        .any(|pattern| pattern == "*" || vam_network::policy::match_domain(pattern.trim(), host))
}

pub fn broker_allowed_domains(bindings: &[SecretBinding]) -> Vec<String> {
    let mut domains = Vec::new();
    for binding in bindings {
        for dest in &binding.allowed_destinations {
            if !domains.iter().any(|existing: &String| existing == dest) {
                domains.push(dest.clone());
            }
        }
    }
    domains
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    Inference,
    Egress,
    /// Credentials of an MCP connection (`{conn}/auth-header`,
    /// `{conn}/header/{name}`, `{conn}/oauth/*`). Written by `mcp_registry`
    /// and, since CN-1.4, by one second writer: `connectors.rs`, for a REST
    /// connector's `connector/{id}/oauth/access-token` and `.../refresh-token`
    /// (Microsoft 365). Those rows are written only after the id token's tenant
    /// equals the admin's consent tenant, and read only by `vamd` to mint
    /// access tokens. The host reads them to call the server. They are
    /// never a guest egress binding (no `vamsec_` placeholder, no allowlist
    /// entry) and no public secrets API can write this kind: `from_proto`
    /// refuses kind 3, and `put_secret_at` refuses to rewrite an existing
    /// `connection` row as another kind.
    Connection,
}

impl SecretKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inference => "inference",
            Self::Egress => "egress",
            Self::Connection => "connection",
        }
    }

    pub fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "inference" => Ok(Self::Inference),
            "egress" => Ok(Self::Egress),
            "connection" => Ok(Self::Connection),
            _ => Err(OrchestratorError::InvalidContract(format!(
                "invalid secret kind '{}'; expected inference or egress",
                value
            ))),
        }
    }

    /// Kind a caller of the public secrets API may write. `connection` (3) is
    /// reserved for MCP connection credentials and is refused here.
    pub fn from_proto(value: i32) -> Result<Self, OrchestratorError> {
        match value {
            1 => Ok(Self::Inference),
            2 => Ok(Self::Egress),
            3 => Err(OrchestratorError::InvalidContract(
                "secret kind connection is reserved for MCP connection credentials".into(),
            )),
            _ => Err(OrchestratorError::InvalidContract(
                "secret kind is required (inference or egress)".into(),
            )),
        }
    }

    pub fn to_proto(self) -> i32 {
        match self {
            Self::Inference => 1,
            Self::Egress => 2,
            Self::Connection => 3,
        }
    }
}

/// Vault partition. Org is shared across the node; user is per `owner_user_id`.
///
/// There is **no** implicit user→org fallback. A missing user-scope row is
/// an error even when an org-scope secret exists for the same `binding_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretScope {
    Org,
    User,
}

impl SecretScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Org => "org",
            Self::User => "user",
        }
    }

    pub fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "org" => Ok(Self::Org),
            "user" => Ok(Self::User),
            _ => Err(OrchestratorError::InvalidContract(format!(
                "invalid secret scope '{value}'; expected org or user"
            ))),
        }
    }

    /// Proto `SCOPE_UNSPECIFIED` (0) is `None` so callers can apply a role default.
    pub fn from_proto(value: i32) -> Result<Option<Self>, OrchestratorError> {
        match value {
            0 => Ok(None),
            1 => Ok(Some(Self::Org)),
            2 => Ok(Some(Self::User)),
            _ => Err(OrchestratorError::InvalidContract(
                "secret scope must be org or user".into(),
            )),
        }
    }

    pub fn to_proto(self) -> i32 {
        match self {
            Self::Org => 1,
            Self::User => 2,
        }
    }
}

/// Empty owner for org-scope rows. User-scope rows use `users.user_id`.
pub const ORG_OWNER_USER_ID: &str = "";

/// Owner column stored in the vault key `(scope, owner_user_id, binding_id)`.
pub fn vault_owner_user_id(scope: SecretScope, owner_user_id: &str) -> &str {
    match scope {
        SecretScope::Org => ORG_OWNER_USER_ID,
        SecretScope::User => crate::owner::resolved_owner(owner_user_id),
    }
}

/// Operator / org_admin default to org; everyone else defaults to user.
pub fn default_secret_scope(role: crate::users::UserRole) -> SecretScope {
    if role.can_write_org_secrets() {
        SecretScope::Org
    } else {
        SecretScope::User
    }
}

/// Resolve a requested (or unspecified) write/read scope. Org writes are
/// admin/operator only; reads of org metadata are allowed for any caller.
pub fn resolve_secret_scope(
    requested: Option<SecretScope>,
    role: crate::users::UserRole,
) -> SecretScope {
    requested.unwrap_or_else(|| default_secret_scope(role))
}

/// Org-scope mutations require operator or org_admin (decision 14).
pub fn authorize_org_secret_write(
    scope: SecretScope,
    role: crate::users::UserRole,
) -> Result<(), OrchestratorError> {
    if scope == SecretScope::Org && !role.can_write_org_secrets() {
        return Err(OrchestratorError::PermissionDenied(
            "org-scope secret writes require operator or org_admin".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct HostSecretMetadata {
    pub binding_id: String,
    pub kind: SecretKind,
    pub fingerprint: String,
    pub allowed_destinations: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    /// Empty for org-scope. User-scope is the owning `users.user_id`.
    pub owner_user_id: String,
    pub scope: SecretScope,
}

#[derive(Clone)]
pub struct SecretsVault {
    master_key: [u8; 32],
    store: Arc<SqliteWorkflowStore>,
}

impl std::fmt::Debug for SecretsVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsVault").finish_non_exhaustive()
    }
}

impl SecretsVault {
    pub fn new(store: Arc<SqliteWorkflowStore>, master_key: [u8; 32]) -> Self {
        Self { master_key, store }
    }

    pub fn for_tests(store: Arc<SqliteWorkflowStore>) -> Arc<Self> {
        Arc::new(Self::new(store, [0u8; 32]))
    }

    /// Keep the vault's store alive while a host-side OAuth refresh completes
    /// on a blocking worker, even if the invoking guest call is cancelled.
    pub(crate) fn workflow_store(&self) -> Arc<SqliteWorkflowStore> {
        Arc::clone(&self.store)
    }

    pub fn initialize(
        store: Arc<SqliteWorkflowStore>,
        db_path: &Path,
    ) -> Result<Self, OrchestratorError> {
        let master_key = load_or_create_master_key(db_path)?;
        Ok(Self::new(store, master_key))
    }

    pub fn fingerprint(value: &str) -> String {
        let hash = Sha256::digest(value.as_bytes());
        format!("{:x}", hash)[..8].to_string()
    }

    pub fn put_secret(
        &self,
        binding_id: &str,
        value: &str,
        kind: SecretKind,
        allowed_destinations: &[String],
        idempotency_key: Option<&str>,
    ) -> Result<HostSecretMetadata, OrchestratorError> {
        self.put_secret_at(
            binding_id,
            value,
            kind,
            allowed_destinations,
            idempotency_key,
            SecretScope::Org,
            ORG_OWNER_USER_ID,
        )
    }

    pub fn put_secret_with_owner(
        &self,
        binding_id: &str,
        value: &str,
        kind: SecretKind,
        allowed_destinations: &[String],
        idempotency_key: Option<&str>,
        owner_user_id: &str,
    ) -> Result<HostSecretMetadata, OrchestratorError> {
        self.put_secret_at(
            binding_id,
            value,
            kind,
            allowed_destinations,
            idempotency_key,
            SecretScope::User,
            owner_user_id,
        )
    }

    #[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
    pub fn put_secret_at(
        &self,
        binding_id: &str,
        value: &str,
        kind: SecretKind,
        allowed_destinations: &[String],
        idempotency_key: Option<&str>,
        scope: SecretScope,
        owner_user_id: &str,
    ) -> Result<HostSecretMetadata, OrchestratorError> {
        if binding_id.trim().is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "binding_id is required".into(),
            ));
        }
        if value.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "value is required".into(),
            ));
        }
        let owner = vault_owner_user_id(scope, owner_user_id);
        if scope == SecretScope::User && owner.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "user-scope secrets require owner_user_id".into(),
            ));
        }

        // A connection credential keeps its kind. Without this, a write of the
        // same binding id as kind=egress would turn the MCP token back into a
        // guest egress binding.
        if kind != SecretKind::Connection {
            if let Ok(existing) = self
                .store
                .get_host_secret_metadata(scope, owner, binding_id)
            {
                if existing.kind == SecretKind::Connection {
                    return Err(OrchestratorError::InvalidContract(format!(
                        "secret '{binding_id}' belongs to an MCP connection and cannot be rewritten as another kind"
                    )));
                }
            }
        }

        if let Some(idem) = idempotency_key.filter(|k| !k.is_empty()) {
            if let Some(existing) = self
                .store
                .find_host_secret_by_idempotency(scope, owner, idem)?
            {
                return Ok(existing);
            }
        }

        let (nonce, ciphertext) = encrypt_value(&self.master_key, value.as_bytes())?;
        let fingerprint = Self::fingerprint(value);
        self.store.put_host_secret(
            binding_id,
            kind,
            &nonce,
            &ciphertext,
            &fingerprint,
            allowed_destinations,
            idempotency_key,
            scope,
            owner,
        )
    }

    pub fn list_metadata(&self) -> Result<Vec<HostSecretMetadata>, OrchestratorError> {
        self.store.list_host_secrets()
    }

    /// Org-scope rows plus the caller's user-scope rows. Other users' vaults
    /// are not listed.
    pub fn list_visible_metadata(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<HostSecretMetadata>, OrchestratorError> {
        self.store
            .list_host_secrets_visible(crate::owner::resolved_owner(owner_user_id))
    }

    pub fn list_org_metadata(&self) -> Result<Vec<HostSecretMetadata>, OrchestratorError> {
        self.store
            .list_host_secrets_at(SecretScope::Org, ORG_OWNER_USER_ID)
    }

    pub fn get_metadata(&self, binding_id: &str) -> Result<HostSecretMetadata, OrchestratorError> {
        self.get_metadata_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)
    }

    pub fn get_metadata_at(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<HostSecretMetadata, OrchestratorError> {
        self.store.get_host_secret_metadata(
            scope,
            vault_owner_user_id(scope, owner_user_id),
            binding_id,
        )
    }

    pub fn delete_secret(&self, binding_id: &str) -> Result<HostSecretMetadata, OrchestratorError> {
        self.delete_secret_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)
    }

    pub fn delete_secret_at(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<HostSecretMetadata, OrchestratorError> {
        self.store
            .delete_host_secret(scope, vault_owner_user_id(scope, owner_user_id), binding_id)
    }

    /// Broker / host-tool decrypt. Org-scope only (in-process adapters and
    /// Port 54 egress). HSP invoke uses [`Self::decrypt_secret_for_caller`].
    pub fn decrypt_secret(&self, binding_id: &str) -> Result<SecretString, OrchestratorError> {
        self.decrypt_secret_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)
    }

    /// Caller-scoped invoke decrypt: this user's user-scope row if present,
    /// else org-scope. Never another user's user-scope row. A dedicated
    /// user-scope lookup ([`Self::decrypt_secret_at`]) still has no
    /// implicit user→org fallback.
    pub fn decrypt_secret_for_caller(
        &self,
        caller_user_id: &str,
        binding_id: &str,
    ) -> Result<SecretString, OrchestratorError> {
        match self.decrypt_secret_at(SecretScope::User, caller_user_id, binding_id) {
            Ok(value) => Ok(value),
            Err(OrchestratorError::NotFound(_)) => {
                self.decrypt_secret_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)
            }
            Err(err) => Err(err),
        }
    }

    /// Decrypt `binding_id` for an HSP broker call made by `caller_user_id`
    /// (user row first, then org, like [`Self::decrypt_secret_for_caller`]),
    /// but only when the broker may hold it:
    ///
    /// - a `connection` row (MCP credential) is never handed to a broker;
    /// - every destination the row is pinned to must lie inside the broker's
    ///   approved `egress_domains` (a row with no pinned destination has no
    ///   host it could be sent to, so it passes).
    ///
    /// A refusal is [`OrchestratorError::PermissionDenied`] and happens before
    /// any decryption, so the value is never read for a broker that may not
    /// have it.
    pub fn decrypt_secret_for_broker(
        &self,
        caller_user_id: &str,
        binding_id: &str,
        egress_domains: &[String],
    ) -> Result<SecretString, OrchestratorError> {
        let (scope, meta) = self.locate_for_caller(caller_user_id, binding_id)?;
        if meta.kind == SecretKind::Connection {
            return Err(OrchestratorError::PermissionDenied(format!(
                "secret '{binding_id}' is an MCP connection credential and is never given to a broker"
            )));
        }
        if let Some(outside) = meta
            .allowed_destinations
            .iter()
            .find(|dest| !destination_within_domains(dest, egress_domains))
        {
            return Err(OrchestratorError::PermissionDenied(format!(
                "secret '{binding_id}' is pinned to '{outside}', outside the broker's approved egress domains"
            )));
        }
        self.decrypt_secret_at(scope, caller_user_id, binding_id)
    }

    /// The row a caller's call resolves to: the caller's own user row, else the
    /// org row (the order of [`Self::decrypt_secret_for_caller`]).
    fn locate_for_caller(
        &self,
        caller_user_id: &str,
        binding_id: &str,
    ) -> Result<(SecretScope, HostSecretMetadata), OrchestratorError> {
        match self.get_metadata_at(SecretScope::User, caller_user_id, binding_id) {
            Ok(meta) => Ok((SecretScope::User, meta)),
            Err(OrchestratorError::NotFound(_)) => Ok((
                SecretScope::Org,
                self.get_metadata_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)?,
            )),
            Err(err) => Err(err),
        }
    }

    /// Decrypt a secret that a host tool adapter will put in an outbound
    /// request, and return the hosts the row is pinned to. An MCP connection
    /// credential is refused ([`OrchestratorError::PermissionDenied`]) before
    /// any decryption. The caller must then check the destination with
    /// [`destination_allows_host`].
    pub fn decrypt_secret_for_host_call(
        &self,
        caller_user_id: &str,
        binding_id: &str,
    ) -> Result<(SecretString, Vec<String>), OrchestratorError> {
        // An empty caller reads the org row only, as the HTTP adapter always has.
        let (scope, meta) = if caller_user_id.is_empty() {
            (
                SecretScope::Org,
                self.get_metadata_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)?,
            )
        } else {
            self.locate_for_caller(caller_user_id, binding_id)?
        };
        if meta.kind == SecretKind::Connection {
            return Err(OrchestratorError::PermissionDenied(format!(
                "secret '{binding_id}' is an MCP connection credential and cannot be used by a tool"
            )));
        }
        let value = self.decrypt_secret_at(scope, caller_user_id, binding_id)?;
        Ok((value, meta.allowed_destinations))
    }

    /// Org-scope decrypt for a host component that takes a vault ref from node
    /// settings (the notification adapters' Slack tokens). It refuses a
    /// `connection` row, so an operator cannot name an MCP credential as one of
    /// those tokens and have it sent to a broker or a Slack API. Refusal is
    /// [`OrchestratorError::PermissionDenied`], before any decryption.
    pub fn decrypt_secret_not_connection(
        &self,
        binding_id: &str,
    ) -> Result<SecretString, OrchestratorError> {
        let meta = self.get_metadata_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)?;
        if meta.kind == SecretKind::Connection {
            return Err(OrchestratorError::PermissionDenied(format!(
                "secret '{binding_id}' is an MCP connection credential and cannot be used here"
            )));
        }
        self.decrypt_secret_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)
    }

    pub fn decrypt_secret_at(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<SecretString, OrchestratorError> {
        let owner = vault_owner_user_id(scope, owner_user_id);
        let (nonce, ciphertext) = self
            .store
            .load_host_secret_ciphertext(scope, owner, binding_id)?;
        let plaintext = decrypt_value(&self.master_key, &nonce, &ciphertext)
            .map_err(|_| OrchestratorError::SecretDecryptFailed(binding_id.to_string()))?;
        self.store
            .touch_host_secret_used(scope, owner, binding_id)?;
        Ok(SecretString::new(plaintext))
    }

    pub fn has_binding(&self, binding_id: &str) -> Result<bool, OrchestratorError> {
        self.has_binding_at(SecretScope::Org, ORG_OWNER_USER_ID, binding_id)
    }

    pub fn has_binding_at(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<bool, OrchestratorError> {
        self.store
            .host_secret_exists(scope, vault_owner_user_id(scope, owner_user_id), binding_id)
    }

    /// Operator-only daemon path. Seeds **org-scope** well-known bindings from
    /// process env when `VAMD_IMPORT_ENV_SECRETS` is set. A user-scope row
    /// with the same `binding_id` does not skip the org import.
    pub fn import_env_secrets_if_enabled(&self) -> Result<(), OrchestratorError> {
        let enabled = std::env::var(IMPORT_ENV_SECRETS)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !enabled {
            return Ok(());
        }
        self.import_well_known_from_env(std::env::vars())
    }

    fn import_well_known_from_env<I, K, V>(&self, vars: I) -> Result<(), OrchestratorError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let env: std::collections::HashMap<String, String> = vars
            .into_iter()
            .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string()))
            .collect();
        for (binding_id, env_var) in WELL_KNOWN_INFERENCE {
            if self.has_binding(binding_id)? {
                continue;
            }
            if let Some(value) = env.get(*env_var) {
                if !value.is_empty() {
                    self.put_secret(binding_id, value, SecretKind::Inference, &[], None)?;
                }
            }
        }
        for known in WELL_KNOWN_EGRESS {
            if self.has_binding(known.binding_id)? {
                continue;
            }
            for env_var in known.env_vars {
                if let Some(value) = env.get(*env_var) {
                    if !value.is_empty() {
                        let destinations: Vec<String> = known
                            .default_destinations
                            .iter()
                            .map(|s| s.to_string())
                            .collect();
                        self.put_secret(
                            known.binding_id,
                            value,
                            SecretKind::Egress,
                            &destinations,
                            None,
                        )?;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Builds Port 54 SecretBindings for **org-scope** egress vault rows.
    ///
    /// HSP invoke injects caller-scoped `resolved_secrets` separately.
    /// Well-known IDs (Jira) supply guest placeholders and inject sites.
    /// Any other `kind=egress` secret is still attached so its
    /// `allowed_destinations` reach the session allowlist without a core constant.
    /// URL values can fill destinations when the metadata list is empty.
    pub fn session_egress_bindings(&self) -> Result<Vec<SecretBinding>, OrchestratorError> {
        let egress: Vec<HostSecretMetadata> = self
            .list_org_metadata()?
            .into_iter()
            .filter(|meta| meta.kind == SecretKind::Egress)
            .collect();

        let mut bindings = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for known in WELL_KNOWN_EGRESS {
            let Some(meta) = egress.iter().find(|m| m.binding_id == known.binding_id) else {
                continue;
            };
            seen.insert(meta.binding_id.clone());
            bindings.push(self.binding_from_meta(meta, Some(known))?);
        }

        for meta in &egress {
            if seen.contains(&meta.binding_id) {
                continue;
            }
            bindings.push(self.binding_from_meta(meta, None)?);
        }

        Ok(bindings)
    }

    /// Like [`Self::session_egress_bindings`] but merges in the caller's
    /// user-scope egress rows. User-scope rows **win** over org-scope rows on
    /// the same `binding_id` (user overrides org). Used by the egress broker
    /// at session boot so the per-user network allow-list is correct.
    pub fn session_egress_bindings_for_caller(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<SecretBinding>, OrchestratorError> {
        // Collect org-scope egress rows
        let org_egress: Vec<HostSecretMetadata> = self
            .list_org_metadata()?
            .into_iter()
            .filter(|meta| meta.kind == SecretKind::Egress)
            .collect();

        // Collect user-scope egress rows for this owner
        let user_egress: Vec<HostSecretMetadata> = self
            .store
            .list_host_secrets_at(
                SecretScope::User,
                crate::owner::resolved_owner(owner_user_id),
            )?
            .into_iter()
            .filter(|meta| meta.kind == SecretKind::Egress)
            .collect();

        // Merge: user-scope wins on binding_id conflict, then org fills the rest
        let mut seen = std::collections::HashSet::new();
        let mut merged: Vec<&HostSecretMetadata> = Vec::new();
        for meta in user_egress.iter().chain(org_egress.iter()) {
            if seen.insert(meta.binding_id.clone()) {
                merged.push(meta);
            }
        }

        let mut bindings = Vec::new();
        let mut bound = std::collections::HashSet::new();

        for known in WELL_KNOWN_EGRESS {
            let Some(meta) = merged.iter().find(|m| m.binding_id == known.binding_id) else {
                continue;
            };
            bound.insert(meta.binding_id.clone());
            bindings.push(self.binding_from_meta(meta, Some(known))?);
        }
        for meta in &merged {
            if !bound.contains(&meta.binding_id) {
                bindings.push(self.binding_from_meta(meta, None)?);
            }
        }

        Ok(bindings)
    }

    fn binding_from_meta(
        &self,
        meta: &HostSecretMetadata,
        known: Option<&WellKnownEgressBinding>,
    ) -> Result<SecretBinding, OrchestratorError> {
        let (placeholder, inject_type, inject_key, defaults) = if let Some(known) = known {
            (
                known.placeholder.to_string(),
                known.inject_type,
                known.inject_key.to_string(),
                known.default_destinations,
            )
        } else {
            let (inject_type, inject_key) = convention_inject_site(&meta.binding_id);
            (
                convention_placeholder(&meta.binding_id),
                inject_type,
                inject_key,
                &[] as &[&str],
            )
        };

        let destinations = if !meta.allowed_destinations.is_empty() {
            meta.allowed_destinations.clone()
        } else if inject_type == InjectionSiteType::InjectionSiteQueryParam as i32 {
            self.decrypt_secret(&meta.binding_id)
                .ok()
                .and_then(|secret| host_from_url_secret(secret.expose_secret()))
                .map(|host| vec![host])
                .unwrap_or_else(|| defaults.iter().map(|s| s.to_string()).collect())
        } else {
            defaults.iter().map(|s| s.to_string()).collect()
        };

        Ok(SecretBinding {
            binding_id: meta.binding_id.clone(),
            placeholder,
            secret_ref: meta.binding_id.clone(),
            allowed_destinations: destinations,
            inject_at: Some(InjectionSite {
                r#type: inject_type,
                key: inject_key,
            }),
        })
    }
}

/// Guest placeholder derived from vault id: `hrms/api-key` → `vamsec_hrms_api_key`.
fn convention_placeholder(binding_id: &str) -> String {
    format!("vamsec_{}", binding_id.replace(['/', '-'], "_"))
}

fn convention_inject_site(binding_id: &str) -> (i32, String) {
    let id = binding_id.to_ascii_lowercase();
    if id.ends_with("api-url") || id.ends_with("base-url") || id.ends_with("/url") {
        (
            InjectionSiteType::InjectionSiteQueryParam as i32,
            String::new(),
        )
    } else if id.contains("api-key") {
        (
            InjectionSiteType::InjectionSiteHeader as i32,
            "x-api-key".to_string(),
        )
    } else {
        (
            InjectionSiteType::InjectionSiteHeader as i32,
            "Authorization".to_string(),
        )
    }
}

impl SqliteWorkflowStore {
    pub fn initialize_host_secrets_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS host_secrets (
                scope TEXT NOT NULL CHECK(scope IN ('org', 'user')),
                owner_user_id TEXT NOT NULL,
                binding_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                nonce BLOB NOT NULL,
                ciphertext BLOB NOT NULL,
                fingerprint TEXT NOT NULL,
                allowed_destinations_json TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_used_at INTEGER,
                idempotency_key TEXT,
                PRIMARY KEY (scope, owner_user_id, binding_id),
                CHECK (
                    (scope = 'org' AND owner_user_id = '')
                    OR (scope = 'user' AND owner_user_id != '')
                )
            );
            CREATE INDEX IF NOT EXISTS idx_host_secrets_kind ON host_secrets(kind);
            ",
        )?;
        Ok(())
    }

    /// Migrate legacy `PRIMARY KEY (binding_id)` rows to
    /// `(scope, owner_user_id, binding_id)` and fold idempotency uniqueness
    /// into that key. Existing rows become org-scope (empty owner). Idempotent.
    pub fn initialize_vault_key(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        if host_secrets_needs_vault_key_rebuild(&connection)? {
            rebuild_host_secrets_vault_key(&connection)?;
        }
        crate::owner::drop_global_idempotency_unique(&connection, "host_secrets")?;
        connection.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_host_secrets_kind ON host_secrets(kind);
            CREATE INDEX IF NOT EXISTS idx_host_secrets_owner_user_id ON host_secrets(owner_user_id);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_host_secrets_scope_owner_idempotency
                ON host_secrets(scope, owner_user_id, idempotency_key)
                WHERE idempotency_key IS NOT NULL;
            ",
        )?;
        Ok(())
    }

    /// Mark credentials that earlier builds stored as `egress` for an existing
    /// MCP connection as `connection`, so they stop being guest egress
    /// bindings. A row matches when it sits in the connection's own vault
    /// partition and its id is one the connection writes: `{id}/auth-header`,
    /// `{id}/header/*`, `{id}/oauth/access-token`, `{id}/oauth/refresh-token`
    /// or `{id}/oauth/client-secret`. Idempotent; runs on every start, after
    /// the connections table exists. Returns the number of rows changed.
    pub fn initialize_connection_secret_kinds(&self) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE host_secrets SET kind = 'connection'
             WHERE kind = 'egress'
               AND EXISTS (
                 SELECT 1 FROM mcp_connections m
                 WHERE m.scope = host_secrets.scope
                   AND m.owner_user_id = host_secrets.owner_user_id
                   AND (
                     host_secrets.binding_id IN (
                       m.connection_id || '/auth-header',
                       m.connection_id || '/oauth/access-token',
                       m.connection_id || '/oauth/refresh-token',
                       m.connection_id || '/oauth/client-secret'
                     )
                     OR substr(host_secrets.binding_id, 1, length(m.connection_id) + 8)
                        = m.connection_id || '/header/'
                   )
               )",
            [],
        )?;
        Ok(changed)
    }

    #[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
    pub fn put_host_secret(
        &self,
        binding_id: &str,
        kind: SecretKind,
        nonce: &[u8],
        ciphertext: &[u8],
        fingerprint: &str,
        allowed_destinations: &[String],
        idempotency_key: Option<&str>,
        scope: SecretScope,
        owner_user_id: &str,
    ) -> Result<HostSecretMetadata, OrchestratorError> {
        let destinations_json = serde_json::to_string(allowed_destinations)
            .map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let now = unix_timestamp();
        let owner = vault_owner_user_id(scope, owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO host_secrets (
                scope, owner_user_id, binding_id, kind, nonce, ciphertext, fingerprint,
                allowed_destinations_json, created_at, updated_at, idempotency_key
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)
             ON CONFLICT(scope, owner_user_id, binding_id) DO UPDATE SET
                kind = excluded.kind,
                nonce = excluded.nonce,
                ciphertext = excluded.ciphertext,
                fingerprint = excluded.fingerprint,
                allowed_destinations_json = excluded.allowed_destinations_json,
                updated_at = excluded.updated_at,
                idempotency_key = COALESCE(excluded.idempotency_key, host_secrets.idempotency_key)",
            params![
                scope.as_str(),
                owner,
                binding_id,
                kind.as_str(),
                nonce,
                ciphertext,
                fingerprint,
                destinations_json,
                now,
                idempotency_key,
            ],
        )?;
        drop(connection);
        self.get_host_secret_metadata(scope, owner, binding_id)
    }

    pub fn list_host_secrets(&self) -> Result<Vec<HostSecretMetadata>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT binding_id, kind, fingerprint, allowed_destinations_json,
                    created_at, updated_at, last_used_at, owner_user_id, scope
             FROM host_secrets ORDER BY scope ASC, owner_user_id ASC, binding_id ASC",
        )?;
        let rows = stmt.query_map([], parse_metadata_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    pub fn list_host_secrets_visible(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<HostSecretMetadata>, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT binding_id, kind, fingerprint, allowed_destinations_json,
                    created_at, updated_at, last_used_at, owner_user_id, scope
             FROM host_secrets
             WHERE scope = 'org'
                OR (scope = 'user' AND owner_user_id = ?1)
             ORDER BY scope ASC, owner_user_id ASC, binding_id ASC",
        )?;
        let rows = stmt.query_map([owner], parse_metadata_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    pub fn list_host_secrets_at(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
    ) -> Result<Vec<HostSecretMetadata>, OrchestratorError> {
        let owner = vault_owner_user_id(scope, owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT binding_id, kind, fingerprint, allowed_destinations_json,
                    created_at, updated_at, last_used_at, owner_user_id, scope
             FROM host_secrets
             WHERE scope = ?1 AND owner_user_id = ?2
             ORDER BY binding_id ASC",
        )?;
        let rows = stmt.query_map(params![scope.as_str(), owner], parse_metadata_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    pub fn get_host_secret_metadata(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<HostSecretMetadata, OrchestratorError> {
        let owner = vault_owner_user_id(scope, owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT binding_id, kind, fingerprint, allowed_destinations_json,
                        created_at, updated_at, last_used_at, owner_user_id, scope
                 FROM host_secrets
                 WHERE scope = ?1 AND owner_user_id = ?2 AND binding_id = ?3",
                params![scope.as_str(), owner, binding_id],
                parse_metadata_row,
            )
            .map_err(|e| secret_not_found(e, scope, owner, binding_id))
    }

    pub fn delete_host_secret(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<HostSecretMetadata, OrchestratorError> {
        let owner = vault_owner_user_id(scope, owner_user_id);
        let meta = self.get_host_secret_metadata(scope, owner, binding_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "DELETE FROM host_secrets
             WHERE scope = ?1 AND owner_user_id = ?2 AND binding_id = ?3",
            params![scope.as_str(), owner, binding_id],
        )?;
        if changed == 0 {
            return Err(secret_missing(scope, owner, binding_id));
        }
        Ok(meta)
    }

    pub fn load_host_secret_ciphertext(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<(Vec<u8>, Vec<u8>), OrchestratorError> {
        let owner = vault_owner_user_id(scope, owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT nonce, ciphertext FROM host_secrets
                 WHERE scope = ?1 AND owner_user_id = ?2 AND binding_id = ?3",
                params![scope.as_str(), owner, binding_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| secret_not_found(e, scope, owner, binding_id))
    }

    pub fn touch_host_secret_used(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<(), OrchestratorError> {
        let owner = vault_owner_user_id(scope, owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE host_secrets SET last_used_at = ?1
             WHERE scope = ?2 AND owner_user_id = ?3 AND binding_id = ?4",
            params![unix_timestamp(), scope.as_str(), owner, binding_id],
        )?;
        Ok(())
    }

    pub fn host_secret_exists(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<bool, OrchestratorError> {
        let owner = vault_owner_user_id(scope, owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM host_secrets
             WHERE scope = ?1 AND owner_user_id = ?2 AND binding_id = ?3",
            params![scope.as_str(), owner, binding_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn find_host_secret_by_idempotency(
        &self,
        scope: SecretScope,
        owner_user_id: &str,
        key: &str,
    ) -> Result<Option<HostSecretMetadata>, OrchestratorError> {
        let owner = vault_owner_user_id(scope, owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT binding_id, kind, fingerprint, allowed_destinations_json,
                        created_at, updated_at, last_used_at, owner_user_id, scope
                 FROM host_secrets
                 WHERE scope = ?1 AND owner_user_id = ?2 AND idempotency_key = ?3",
                params![scope.as_str(), owner, key],
                parse_metadata_row,
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    pub fn host_secrets_has_vault_key(&self) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(!host_secrets_needs_vault_key_rebuild(&connection)?)
    }
}

/// Vault-backed secret lookup for the inference broker (Port 53).
pub struct VaultSecretLookup {
    vault: Arc<SecretsVault>,
    /// Session owner user id. When set, resolves user-scope rows first (then
    /// org-scope fallback). When unset, resolves org-scope only.
    caller_user_id: Option<String>,
    audit_target: Option<(Arc<SqliteWorkflowStore>, String)>,
}

impl std::fmt::Debug for VaultSecretLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultSecretLookup")
            .field("vault", &self.vault)
            .finish_non_exhaustive()
    }
}

impl VaultSecretLookup {
    pub fn new(vault: Arc<SecretsVault>) -> Self {
        Self {
            vault,
            caller_user_id: None,
            audit_target: None,
        }
    }

    /// Set the session owner so the lookup resolves user-scope secrets first.
    pub fn with_caller(mut self, user_id: String) -> Self {
        if !user_id.trim().is_empty() {
            self.caller_user_id = Some(user_id);
        }
        self
    }

    pub fn with_audit(mut self, store: Arc<SqliteWorkflowStore>, session_id: String) -> Self {
        self.audit_target = Some((store, session_id));
        self
    }
}

impl SecretLookup for VaultSecretLookup {
    fn get_secret(&self, secret_ref: &str) -> Option<SecretString> {
        let val = match &self.caller_user_id {
            Some(uid) => self.vault.decrypt_secret_for_caller(uid, secret_ref).ok(),
            None => self.vault.decrypt_secret(secret_ref).ok(),
        };
        if val.is_some() {
            if let Some((ref store, ref session_id)) = self.audit_target {
                if let Ok(payload) =
                    crate::events::secret_accessed_json(session_id, 0, secret_ref, "inference")
                {
                    let _ = store.append_outbox_event(
                        session_id,
                        crate::events::TYPE_SECRET_ACCESSED,
                        &payload,
                    );
                }
            }
        }
        val
    }
}

/// Vault-backed provider for the egress broker (Port 54).
pub struct VaultSecretProvider {
    vault: Arc<SecretsVault>,
    /// Session owner user id. When set, resolves user-scope rows first (then
    /// org-scope fallback). When unset, resolves org-scope only.
    caller_user_id: Option<String>,
    audit_target: Option<(Arc<SqliteWorkflowStore>, String)>,
}

impl std::fmt::Debug for VaultSecretProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultSecretProvider")
            .field("vault", &self.vault)
            .finish_non_exhaustive()
    }
}

impl VaultSecretProvider {
    pub fn new(vault: Arc<SecretsVault>) -> Self {
        Self {
            vault,
            caller_user_id: None,
            audit_target: None,
        }
    }

    /// Set the session owner so the provider resolves user-scope secrets first.
    pub fn with_caller(mut self, user_id: String) -> Self {
        if !user_id.trim().is_empty() {
            self.caller_user_id = Some(user_id);
        }
        self
    }

    pub fn with_audit(mut self, store: Arc<SqliteWorkflowStore>, session_id: String) -> Self {
        self.audit_target = Some((store, session_id));
        self
    }
}

impl SecretProvider for VaultSecretProvider {
    fn get_secret(&self, secret_ref: &str) -> Result<SecretString, vam_network::VamNetworkError> {
        let val = match &self.caller_user_id {
            Some(uid) => self
                .vault
                .decrypt_secret_for_caller(uid, secret_ref)
                .map_err(|_| vam_network::VamNetworkError::SecretNotFound {
                    key: secret_ref.to_string(),
                })?,
            None => self.vault.decrypt_secret(secret_ref).map_err(|_| {
                vam_network::VamNetworkError::SecretNotFound {
                    key: secret_ref.to_string(),
                }
            })?,
        };
        if let Some((ref store, ref session_id)) = self.audit_target {
            if let Ok(payload) =
                crate::events::secret_accessed_json(session_id, 0, secret_ref, "egress")
            {
                let _ = store.append_outbox_event(
                    session_id,
                    crate::events::TYPE_SECRET_ACCESSED,
                    &payload,
                );
            }
        }
        Ok(val)
    }
}

pub fn load_or_create_master_key(db_path: &Path) -> Result<[u8; 32], OrchestratorError> {
    if let Ok(hex_key) = std::env::var(MASTER_KEY_ENV) {
        return parse_master_key_hex(&hex_key);
    }

    let key_path = master_key_path_for_db(db_path);
    if key_path.exists() {
        let bytes = std::fs::read(&key_path)?;
        if bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
        return Err(OrchestratorError::SecretsMasterKey(format!(
            "invalid master key file at {:?}; expected 32 bytes",
            key_path
        )));
    }

    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);

    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_master_key_file(&key_path, &key)?;
    eprintln!(
        "Generated secrets master key at {:?} (0600). Set {} to override.",
        key_path, MASTER_KEY_ENV
    );
    Ok(key)
}

fn master_key_path_for_db(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(|p| p.join("secrets.key"))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MASTER_KEY_PATH))
}

fn write_master_key_file(path: &Path, key: &[u8; 32]) -> Result<(), OrchestratorError> {
    std::fs::write(path, key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn parse_master_key_hex(hex_key: &str) -> Result<[u8; 32], OrchestratorError> {
    let trimmed = hex_key.trim();
    let bytes = hex::decode(trimmed).map_err(|e| {
        OrchestratorError::SecretsMasterKey(format!(
            "{} must be 32-byte hex: {}",
            MASTER_KEY_ENV, e
        ))
    })?;
    if bytes.len() != 32 {
        return Err(OrchestratorError::SecretsMasterKey(format!(
            "{} must decode to exactly 32 bytes",
            MASTER_KEY_ENV
        )));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn encrypt_value(
    master_key: &[u8; 32],
    plaintext: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), OrchestratorError> {
    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|e| OrchestratorError::SecretsMasterKey(e.to_string()))?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| OrchestratorError::SecretsMasterKey(format!("encrypt failed: {}", e)))?;
    Ok((nonce_bytes.to_vec(), ciphertext))
}

fn decrypt_value(
    master_key: &[u8; 32],
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<String, OrchestratorError> {
    if nonce.len() != 12 {
        return Err(OrchestratorError::SecretDecryptFailed(
            "invalid nonce".into(),
        ));
    }
    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|e| OrchestratorError::SecretsMasterKey(e.to_string()))?;
    let nonce = Nonce::from_slice(nonce);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| OrchestratorError::SecretDecryptFailed("decrypt failed".into()))?;
    String::from_utf8(plaintext)
        .map_err(|_| OrchestratorError::SecretDecryptFailed("invalid utf-8".into()))
}

fn parse_metadata_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostSecretMetadata> {
    let kind_str: String = row.get(1)?;
    let destinations_json: String = row.get(3)?;
    let allowed_destinations: Vec<String> =
        serde_json::from_str(&destinations_json).unwrap_or_default();
    let owner_user_id: Option<String> = row.get(7)?;
    let scope_str: String = row.get(8)?;
    let scope = SecretScope::parse(&scope_str).unwrap_or(SecretScope::Org);
    let owner = owner_user_id.unwrap_or_default();
    Ok(HostSecretMetadata {
        binding_id: row.get(0)?,
        kind: SecretKind::parse(&kind_str).unwrap_or(SecretKind::Inference),
        fingerprint: row.get(2)?,
        allowed_destinations,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        last_used_at: row.get(6)?,
        owner_user_id: match scope {
            SecretScope::Org => ORG_OWNER_USER_ID.to_string(),
            SecretScope::User => {
                if owner.is_empty() {
                    crate::users::LOCAL_USER_ID.to_string()
                } else {
                    owner
                }
            }
        },
        scope,
    })
}

fn secret_missing(scope: SecretScope, owner: &str, binding_id: &str) -> OrchestratorError {
    let _ = (scope, owner);
    OrchestratorError::NotFound(format!("secret '{binding_id}' not found"))
}

fn secret_not_found(
    err: rusqlite::Error,
    scope: SecretScope,
    owner: &str,
    binding_id: &str,
) -> OrchestratorError {
    if err == rusqlite::Error::QueryReturnedNoRows {
        secret_missing(scope, owner, binding_id)
    } else {
        OrchestratorError::Database(err)
    }
}

fn host_secrets_needs_vault_key_rebuild(
    connection: &rusqlite::Connection,
) -> Result<bool, OrchestratorError> {
    if !table_has_column(connection, "host_secrets", "scope")? {
        return Ok(true);
    }
    host_secrets_pk_is_binding_id_only(connection)
}

fn table_has_column(
    connection: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, OrchestratorError> {
    let mut stmt = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(OrchestratorError::Database)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(OrchestratorError::Database)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(OrchestratorError::Database)?;
    Ok(names.iter().any(|n| n == column))
}

fn host_secrets_pk_is_binding_id_only(
    connection: &rusqlite::Connection,
) -> Result<bool, OrchestratorError> {
    let mut stmt = connection
        .prepare("PRAGMA table_info(host_secrets)")
        .map_err(OrchestratorError::Database)?;
    let cols = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
        })
        .map_err(OrchestratorError::Database)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(OrchestratorError::Database)?;
    let pk: Vec<String> = cols
        .into_iter()
        .filter(|(_, pk)| *pk > 0)
        .map(|(name, _)| name)
        .collect();
    Ok(pk.len() == 1 && pk[0] == "binding_id")
}

fn rebuild_host_secrets_vault_key(
    connection: &rusqlite::Connection,
) -> Result<(), OrchestratorError> {
    connection
        .execute("PRAGMA foreign_keys = OFF", [])
        .map_err(OrchestratorError::Database)?;
    let rebuilt = (|| -> Result<(), rusqlite::Error> {
        connection.execute_batch(
            "
            CREATE TABLE host_secrets__vault_new (
                scope TEXT NOT NULL CHECK(scope IN ('org', 'user')),
                owner_user_id TEXT NOT NULL,
                binding_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                nonce BLOB NOT NULL,
                ciphertext BLOB NOT NULL,
                fingerprint TEXT NOT NULL,
                allowed_destinations_json TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_used_at INTEGER,
                idempotency_key TEXT,
                PRIMARY KEY (scope, owner_user_id, binding_id),
                CHECK (
                    (scope = 'org' AND owner_user_id = '')
                    OR (scope = 'user' AND owner_user_id != '')
                )
            );
            INSERT INTO host_secrets__vault_new (
                scope, owner_user_id, binding_id, kind, nonce, ciphertext, fingerprint,
                allowed_destinations_json, created_at, updated_at, last_used_at,
                idempotency_key
            )
            SELECT
                'org',
                '',
                binding_id, kind, nonce, ciphertext, fingerprint,
                allowed_destinations_json, created_at, updated_at, last_used_at,
                idempotency_key
            FROM host_secrets;
            DROP TABLE host_secrets;
            ALTER TABLE host_secrets__vault_new RENAME TO host_secrets;
            ",
        )?;
        Ok(())
    })();
    connection
        .execute("PRAGMA foreign_keys = ON", [])
        .map_err(OrchestratorError::Database)?;
    rebuilt.map_err(OrchestratorError::Database)
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

mod hex {
    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        if !s.len().is_multiple_of(2) {
            return Err("odd length".into());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [7u8; 32];
        let (nonce, ciphertext) = encrypt_value(&key, b"sk-test-secret").unwrap();
        let plain = decrypt_value(&key, &nonce, &ciphertext).unwrap();
        assert_eq!(plain, "sk-test-secret");
    }

    #[test]
    fn fingerprint_is_eight_hex_chars() {
        let fp = SecretsVault::fingerprint("my-secret-value");
        assert_eq!(fp.len(), 8);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn vault_put_list_no_plaintext_in_metadata() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store);
        let meta = vault
            .put_secret(
                BINDING_OPENAI,
                "sk-openai-test",
                SecretKind::Inference,
                &[],
                Some("idem-1"),
            )
            .unwrap();
        assert_eq!(
            meta.fingerprint,
            SecretsVault::fingerprint("sk-openai-test")
        );
        assert!(!meta.fingerprint.contains("sk-openai"));
        assert_eq!(meta.scope, SecretScope::Org);
        assert_eq!(meta.owner_user_id, ORG_OWNER_USER_ID);
        let listed = vault.list_metadata().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].binding_id, BINDING_OPENAI);
        assert_eq!(listed[0].scope, SecretScope::Org);
    }

    #[test]
    fn vault_secret_lookup_returns_decrypted_value() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = Arc::new(SecretsVault::new(store, [2u8; 32]));
        vault
            .put_secret(
                BINDING_ANTHROPIC,
                "sk-anthropic",
                SecretKind::Inference,
                &[],
                None,
            )
            .unwrap();
        let lookup = VaultSecretLookup::new(vault);
        let secret = lookup.get_secret(BINDING_ANTHROPIC).unwrap();
        assert_eq!(secret.expose_secret(), "sk-anthropic");
    }

    #[test]
    fn host_from_url_secret_strips_scheme_and_path() {
        assert_eq!(
            host_from_url_secret("https://acme.atlassian.net/wiki"),
            Some("acme.atlassian.net".into())
        );
        assert_eq!(
            host_from_url_secret("acme.atlassian.net"),
            Some("acme.atlassian.net".into())
        );
        assert_eq!(host_from_url_secret("   "), None);
    }

    #[test]
    fn session_egress_bindings_map_jira_placeholders() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store);
        vault
            .put_secret(
                BINDING_JIRA_BASE_URL,
                "https://acme.atlassian.net",
                SecretKind::Egress,
                &[],
                None,
            )
            .unwrap();
        vault
            .put_secret(
                BINDING_JIRA_AUTH_HEADER,
                "Basic dXNlcjp0b2tlbg==",
                SecretKind::Egress,
                &[],
                None,
            )
            .unwrap();

        let bindings = vault.session_egress_bindings().unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].placeholder, PLACEHOLDER_JIRA_BASE_URL);
        assert_eq!(bindings[0].secret_ref, BINDING_JIRA_BASE_URL);
        assert_eq!(bindings[0].allowed_destinations, vec!["acme.atlassian.net"]);
        assert_eq!(
            bindings[0].inject_at.as_ref().unwrap().r#type,
            InjectionSiteType::InjectionSiteQueryParam as i32
        );
        assert_eq!(bindings[1].placeholder, PLACEHOLDER_JIRA_AUTH_TOKEN);
        assert_eq!(
            bindings[1].allowed_destinations,
            vec!["*.atlassian.net".to_string(), "*.jira.com".to_string()]
        );
        assert_eq!(
            broker_allowed_domains(&bindings),
            vec![
                "acme.atlassian.net".to_string(),
                "*.atlassian.net".to_string(),
                "*.jira.com".to_string(),
            ]
        );
    }

    #[test]
    fn session_egress_bindings_map_hrms_api_key() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store);
        vault
            .put_secret(
                BINDING_HRMS_API_URL,
                "https://hrms.example.com",
                SecretKind::Egress,
                &[],
                None,
            )
            .unwrap();
        vault
            .put_secret(
                BINDING_HRMS_API_KEY,
                "sk-hrms-test",
                SecretKind::Egress,
                &["hrms.example.com".to_string()],
                None,
            )
            .unwrap();

        let bindings = vault.session_egress_bindings().unwrap();
        assert_eq!(bindings.len(), 2);
        let base = bindings
            .iter()
            .find(|b| b.binding_id == BINDING_HRMS_API_URL)
            .unwrap();
        let key = bindings
            .iter()
            .find(|b| b.binding_id == BINDING_HRMS_API_KEY)
            .unwrap();
        assert_eq!(base.placeholder, PLACEHOLDER_HRMS_API_URL);
        assert_eq!(
            base.allowed_destinations,
            vec!["hrms.example.com".to_string()]
        );
        assert_eq!(key.placeholder, PLACEHOLDER_HRMS_API_KEY);
        assert_eq!(key.inject_at.as_ref().unwrap().key, "x-api-key");
        assert_eq!(
            key.allowed_destinations,
            vec!["hrms.example.com".to_string()]
        );
        assert_eq!(
            broker_allowed_domains(&bindings),
            vec!["hrms.example.com".to_string()]
        );
    }

    #[test]
    fn two_users_can_store_the_same_binding_without_overwrite() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.create_user("alice", "alice").unwrap();
        store.create_user("bob", "bob").unwrap();
        let vault = SecretsVault::for_tests(store.clone());
        let alice = vault
            .put_secret_with_owner(
                BINDING_JIRA_AUTH_HEADER,
                "alice-header",
                SecretKind::Egress,
                &["*.atlassian.net".to_string()],
                None,
                "alice",
            )
            .unwrap();
        let bob = vault
            .put_secret_with_owner(
                BINDING_JIRA_AUTH_HEADER,
                "bob-header",
                SecretKind::Egress,
                &["*.atlassian.net".to_string()],
                None,
                "bob",
            )
            .unwrap();
        assert_eq!(alice.scope, SecretScope::User);
        assert_eq!(alice.owner_user_id, "alice");
        assert_eq!(bob.owner_user_id, "bob");
        assert_eq!(alice.fingerprint, SecretsVault::fingerprint("alice-header"));
        assert_eq!(bob.fingerprint, SecretsVault::fingerprint("bob-header"));
        assert_eq!(
            vault
                .decrypt_secret_at(SecretScope::User, "alice", BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "alice-header"
        );
        assert_eq!(
            vault
                .decrypt_secret_at(SecretScope::User, "bob", BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "bob-header"
        );
        let alice_visible = vault.list_visible_metadata("alice").unwrap();
        assert!(alice_visible
            .iter()
            .any(|m| m.binding_id == BINDING_JIRA_AUTH_HEADER && m.owner_user_id == "alice"));
        assert!(!alice_visible.iter().any(|m| m.owner_user_id == "bob"));
    }

    #[test]
    fn decrypt_secret_for_caller_uses_user_row_not_peer() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.create_user("alice", "alice").unwrap();
        store.create_user("bob", "bob").unwrap();
        let vault = SecretsVault::for_tests(store);
        vault
            .put_secret_with_owner(
                BINDING_JIRA_AUTH_HEADER,
                "alice-header",
                SecretKind::Egress,
                &[],
                None,
                "alice",
            )
            .unwrap();
        vault
            .put_secret_with_owner(
                BINDING_JIRA_AUTH_HEADER,
                "bob-header",
                SecretKind::Egress,
                &[],
                None,
                "bob",
            )
            .unwrap();
        vault
            .put_secret(
                BINDING_JIRA_AUTH_HEADER,
                "org-header",
                SecretKind::Egress,
                &[],
                None,
            )
            .unwrap();
        assert_eq!(
            vault
                .decrypt_secret_for_caller("alice", BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "alice-header"
        );
        assert_eq!(
            vault
                .decrypt_secret_for_caller("bob", BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "bob-header"
        );
        assert_ne!(
            vault
                .decrypt_secret_for_caller("alice", BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "bob-header"
        );
    }

    #[test]
    fn user_scope_miss_does_not_fall_back_to_org() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.create_user("alice", "alice").unwrap();
        let vault = SecretsVault::for_tests(store);
        vault
            .put_secret(
                BINDING_JIRA_AUTH_HEADER,
                "org-header",
                SecretKind::Egress,
                &["*.atlassian.net".to_string()],
                None,
            )
            .unwrap();
        let miss = vault
            .get_metadata_at(SecretScope::User, "alice", BINDING_JIRA_AUTH_HEADER)
            .expect_err("user miss must not fall back to org");
        match miss {
            OrchestratorError::NotFound(msg) => {
                assert_eq!(
                    msg,
                    format!("secret '{BINDING_JIRA_AUTH_HEADER}' not found")
                );
            }
            other => panic!("expected NotFound, got {other}"),
        }
        assert_eq!(
            vault
                .get_metadata(BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .fingerprint,
            SecretsVault::fingerprint("org-header")
        );
        assert_eq!(
            vault
                .decrypt_secret(BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "org-header"
        );
        let user_decrypt =
            vault.decrypt_secret_at(SecretScope::User, "alice", BINDING_JIRA_AUTH_HEADER);
        assert!(user_decrypt.is_err(), "{user_decrypt:?}");
    }

    #[test]
    fn authorize_org_secret_write_is_admin_or_operator_only() {
        use crate::users::UserRole;
        authorize_org_secret_write(SecretScope::Org, UserRole::Operator).unwrap();
        authorize_org_secret_write(SecretScope::Org, UserRole::OrgAdmin).unwrap();
        let member = authorize_org_secret_write(SecretScope::Org, UserRole::Member)
            .expect_err("member cannot write org");
        match member {
            OrchestratorError::PermissionDenied(msg) => {
                assert!(msg.contains("org-scope"), "{msg}");
            }
            other => panic!("expected PermissionDenied, got {other}"),
        }
        let publisher = authorize_org_secret_write(SecretScope::Org, UserRole::Publisher)
            .expect_err("publisher cannot write org");
        assert!(matches!(publisher, OrchestratorError::PermissionDenied(_)));
        authorize_org_secret_write(SecretScope::User, UserRole::Member).unwrap();
        assert_eq!(default_secret_scope(UserRole::Operator), SecretScope::Org);
        assert_eq!(default_secret_scope(UserRole::Member), SecretScope::User);
    }

    #[test]
    fn import_env_secrets_are_org_scope_and_ignore_user_rows() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.create_user("alice", "alice").unwrap();
        let vault = SecretsVault::for_tests(store);
        vault
            .put_secret_with_owner(
                BINDING_OPENAI,
                "sk-alice",
                SecretKind::Inference,
                &[],
                None,
                "alice",
            )
            .unwrap();
        vault
            .import_well_known_from_env([("OPENAI_API_KEY", "sk-org-imported")])
            .unwrap();
        let org = vault.get_metadata(BINDING_OPENAI).unwrap();
        assert_eq!(org.scope, SecretScope::Org);
        assert_eq!(org.owner_user_id, ORG_OWNER_USER_ID);
        assert_eq!(
            org.fingerprint,
            SecretsVault::fingerprint("sk-org-imported")
        );
        assert_eq!(
            vault
                .decrypt_secret_at(SecretScope::User, "alice", BINDING_OPENAI)
                .unwrap()
                .expose_secret(),
            "sk-alice"
        );
        vault
            .import_well_known_from_env([("OPENAI_API_KEY", "sk-should-not-overwrite")])
            .unwrap();
        assert_eq!(
            vault
                .decrypt_secret(BINDING_OPENAI)
                .unwrap()
                .expose_secret(),
            "sk-org-imported"
        );
    }

    #[test]
    fn host_secrets_idempotency_is_scoped_to_vault_key() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.create_user("alice", "alice").unwrap();
        store.create_user("bob", "bob").unwrap();
        let vault = SecretsVault::for_tests(store.clone());
        vault
            .put_secret_with_owner(
                BINDING_JIRA_AUTH_HEADER,
                "alice-1",
                SecretKind::Egress,
                &[],
                Some("shared-idem"),
                "alice",
            )
            .unwrap();
        vault
            .put_secret_with_owner(
                BINDING_JIRA_AUTH_HEADER,
                "bob-1",
                SecretKind::Egress,
                &[],
                Some("shared-idem"),
                "bob",
            )
            .unwrap();
        let alice_replay = vault
            .put_secret_with_owner(
                BINDING_OPENAI,
                "should-not-write",
                SecretKind::Inference,
                &[],
                Some("shared-idem"),
                "alice",
            )
            .unwrap();
        assert_eq!(alice_replay.binding_id, BINDING_JIRA_AUTH_HEADER);
        assert_eq!(
            alice_replay.fingerprint,
            SecretsVault::fingerprint("alice-1")
        );
        assert_eq!(
            vault
                .decrypt_secret_at(SecretScope::User, "bob", BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "bob-1"
        );
        assert!(!store.has_global_idempotency_unique("host_secrets").unwrap());
        assert!(store.host_secrets_has_vault_key().unwrap());
        vault
            .put_secret(
                BINDING_OPENAI,
                "org-1",
                SecretKind::Inference,
                &[],
                Some("shared-idem"),
            )
            .unwrap();
        assert_eq!(
            vault
                .decrypt_secret(BINDING_OPENAI)
                .unwrap()
                .expose_secret(),
            "org-1"
        );
    }

    #[test]
    fn existing_host_secrets_migrate_to_org_scope() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "vam-vault-migrate-{}-{}.db",
            std::process::id(),
            nanos
        ));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                CREATE TABLE host_secrets (
                    binding_id TEXT PRIMARY KEY NOT NULL,
                    kind TEXT NOT NULL,
                    nonce BLOB NOT NULL,
                    ciphertext BLOB NOT NULL,
                    fingerprint TEXT NOT NULL,
                    allowed_destinations_json TEXT NOT NULL DEFAULT '[]',
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    last_used_at INTEGER,
                    idempotency_key TEXT UNIQUE
                );
                INSERT INTO host_secrets (
                    binding_id, kind, nonce, ciphertext, fingerprint,
                    allowed_destinations_json, created_at, updated_at, idempotency_key
                ) VALUES ('jira/auth-header', 'egress', x'00', x'00', 'deadbeef', '[]', 1, 1, 'sec-k');
                ",
            )
            .unwrap();
        }
        let store = SqliteWorkflowStore::open(&path).unwrap();
        assert!(store.host_secrets_has_vault_key().unwrap());
        assert!(!store.has_global_idempotency_unique("host_secrets").unwrap());
        let meta = store
            .get_host_secret_metadata(
                SecretScope::Org,
                ORG_OWNER_USER_ID,
                BINDING_JIRA_AUTH_HEADER,
            )
            .unwrap();
        assert_eq!(meta.scope, SecretScope::Org);
        assert_eq!(meta.owner_user_id, ORG_OWNER_USER_ID);
        assert_eq!(meta.binding_id, BINDING_JIRA_AUTH_HEADER);
        assert_eq!(meta.fingerprint, "deadbeef");
        let user_miss = store.get_host_secret_metadata(
            SecretScope::User,
            crate::users::LOCAL_USER_ID,
            BINDING_JIRA_AUTH_HEADER,
        );
        assert!(user_miss.is_err(), "{user_miss:?}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn secret_write_stamps_owner_and_list_stays_scoped() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        let org = vault
            .put_secret(BINDING_OPENAI, "sk-org", SecretKind::Inference, &[], None)
            .unwrap();
        let loc = vault
            .put_secret_with_owner(
                BINDING_ANTHROPIC,
                "sk-loc",
                SecretKind::Inference,
                &[],
                None,
                crate::users::LOCAL_USER_ID,
            )
            .unwrap();
        assert_eq!(org.scope, SecretScope::Org);
        assert_eq!(org.owner_user_id, ORG_OWNER_USER_ID);
        assert_eq!(loc.scope, SecretScope::User);
        assert_eq!(loc.owner_user_id, crate::users::LOCAL_USER_ID);
        assert_eq!(
            store
                .host_secret_owner_user_id(
                    SecretScope::User,
                    crate::users::LOCAL_USER_ID,
                    BINDING_ANTHROPIC,
                )
                .unwrap(),
            crate::users::LOCAL_USER_ID
        );

        let visible = vault
            .list_visible_metadata(crate::users::LOCAL_USER_ID)
            .unwrap();
        let ids: Vec<_> = visible.iter().map(|m| m.binding_id.as_str()).collect();
        assert!(ids.contains(&BINDING_OPENAI));
        assert!(ids.contains(&BINDING_ANTHROPIC));

        // Broker resolve stays org-scope only (no user fallback).
        let by_id = vault.get_metadata(BINDING_OPENAI).unwrap();
        assert_eq!(by_id.binding_id, BINDING_OPENAI);
        assert_eq!(by_id.scope, SecretScope::Org);
        let lookup = VaultSecretLookup::new(vault.clone());
        assert_eq!(
            lookup.get_secret(BINDING_OPENAI).unwrap().expose_secret(),
            "sk-org"
        );
        assert!(lookup.get_secret(BINDING_ANTHROPIC).is_none());
        assert_eq!(store.count_null_owners().unwrap(), 0);
    }

    #[test]
    fn session_egress_bindings_include_unlisted_egress_secret() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store);
        vault
            .put_secret(
                "otherapp/api-key",
                "sk-other-test",
                SecretKind::Egress,
                &["api.example.com".to_string()],
                None,
            )
            .unwrap();

        let bindings = vault.session_egress_bindings().unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].binding_id, "otherapp/api-key");
        assert_eq!(bindings[0].placeholder, "vamsec_otherapp_api_key");
        assert_eq!(
            bindings[0].allowed_destinations,
            vec!["api.example.com".to_string()]
        );
        assert_eq!(
            broker_allowed_domains(&bindings),
            vec!["api.example.com".to_string()]
        );
    }

    #[test]
    fn vault_secret_lookup_and_provider_resolve_user_scope_for_caller() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store);

        // Org has OpenAI key and Jira token
        vault
            .put_secret(
                BINDING_OPENAI,
                "sk-org-openai",
                SecretKind::Inference,
                &[],
                None,
            )
            .unwrap();
        vault
            .put_secret(
                BINDING_JIRA_AUTH_HEADER,
                "org-jira-tok",
                SecretKind::Egress,
                &[],
                None,
            )
            .unwrap();

        // Alice overrides OpenAI key and has a user-scoped Jira token
        vault
            .put_secret_with_owner(
                BINDING_OPENAI,
                "sk-alice-openai",
                SecretKind::Inference,
                &[],
                None,
                "alice",
            )
            .unwrap();
        vault
            .put_secret_with_owner(
                BINDING_JIRA_AUTH_HEADER,
                "alice-jira-tok",
                SecretKind::Egress,
                &[],
                None,
                "alice",
            )
            .unwrap();

        // Lookup for Alice: user key wins for OpenAI
        let lookup_alice = VaultSecretLookup::new(vault.clone()).with_caller("alice".to_string());
        assert_eq!(
            lookup_alice
                .get_secret(BINDING_OPENAI)
                .unwrap()
                .expose_secret(),
            "sk-alice-openai"
        );

        // Lookup for Bob (no user key): falls back to org key
        let lookup_bob = VaultSecretLookup::new(vault.clone()).with_caller("bob".to_string());
        assert_eq!(
            lookup_bob
                .get_secret(BINDING_OPENAI)
                .unwrap()
                .expose_secret(),
            "sk-org-openai"
        );

        // Lookup without caller: org key
        let lookup_none = VaultSecretLookup::new(vault.clone());
        assert_eq!(
            lookup_none
                .get_secret(BINDING_OPENAI)
                .unwrap()
                .expose_secret(),
            "sk-org-openai"
        );

        // Provider for Alice: user Jira token wins
        let prov_alice = VaultSecretProvider::new(vault.clone()).with_caller("alice".to_string());
        assert_eq!(
            prov_alice
                .get_secret(BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "alice-jira-tok"
        );

        // Provider for Bob: falls back to org Jira token
        let prov_bob = VaultSecretProvider::new(vault.clone()).with_caller("bob".to_string());
        assert_eq!(
            prov_bob
                .get_secret(BINDING_JIRA_AUTH_HEADER)
                .unwrap()
                .expose_secret(),
            "org-jira-tok"
        );
    }

    #[test]
    fn session_egress_bindings_for_caller_merges_user_and_org() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store);

        // Org has an egress secret
        vault
            .put_secret(
                "org-service/key",
                "org-val",
                SecretKind::Egress,
                &["org.example.com".to_string()],
                None,
            )
            .unwrap();
        // Org has a Jira URL
        vault
            .put_secret(
                BINDING_JIRA_BASE_URL,
                "https://org.atlassian.net",
                SecretKind::Egress,
                &["org.atlassian.net".to_string()],
                None,
            )
            .unwrap();

        // Alice overrides Jira URL to her own instance
        vault
            .put_secret_with_owner(
                BINDING_JIRA_BASE_URL,
                "https://alice.atlassian.net",
                SecretKind::Egress,
                &["alice.atlassian.net".to_string()],
                None,
                "alice",
            )
            .unwrap();

        let bindings = vault.session_egress_bindings_for_caller("alice").unwrap();
        // Should have 2 bindings: org-service/key from org, and BINDING_JIRA_BASE_URL from alice
        assert_eq!(bindings.len(), 2);

        let jira_binding = bindings
            .iter()
            .find(|b| b.binding_id == BINDING_JIRA_BASE_URL)
            .expect("jira binding present");
        assert_eq!(
            jira_binding.allowed_destinations,
            vec!["alice.atlassian.net".to_string()]
        );

        let org_binding = bindings
            .iter()
            .find(|b| b.binding_id == "org-service/key")
            .expect("org binding present");
        assert_eq!(
            org_binding.allowed_destinations,
            vec!["org.example.com".to_string()]
        );
    }
}
