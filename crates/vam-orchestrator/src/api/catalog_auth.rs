//! Owner-only versioning, catalog read vs write, and live `publish:grant`.

use crate::api::auth::AuthContext;
use crate::error::OrchestratorError;
use crate::keys::{AGENTS_DRAFT_SCOPE, PUBLISH_GRANT_SCOPE};
use crate::workflow::SqliteWorkflowStore;

/// List/get visible catalog. Members with run rights (`runs:write`) may read
/// released rows. Catalog writers may also read (own drafts + released).
/// PUT/release stay on `catalog:write` / `publish:grant`. Visibility filters
/// (`list_*_visible`) still hide foreign drafts.
pub fn require_catalog_read(auth: &AuthContext) -> Result<(), OrchestratorError> {
    if auth.has_keys_admin() {
        return Ok(());
    }
    if auth
        .key
        .record
        .scopes
        .iter()
        .any(|s| s == "runs:write" || s == "catalog:write")
    {
        Ok(())
    } else {
        Err(OrchestratorError::PermissionDenied(
            "missing required scope 'runs:write'".into(),
        ))
    }
}

/// How a caller may write an agent draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentWriteMode {
    /// `catalog:write` (or `keys:admin`): today's path, no member rules.
    Full,
    /// `agents:draft` without `catalog:write`: the caller's own drafts only, from released
    /// parts, under the rules of [`crate::member_draft`].
    Restricted,
}

/// The one gate for the two routes open to `agents:draft`: `PUT /v1/agents` and
/// `PUT /v1/agents/{urn}/permissions`. Decided on the **effective** scopes, so a key narrowed
/// to `agents:draft` is restricted even when its owner is a publisher. Every other
/// `catalog:write` route keeps its own `authorize(…, "catalog:write")`, which `agents:draft`
/// does not satisfy.
pub fn require_agent_draft_write(auth: &AuthContext) -> Result<AgentWriteMode, OrchestratorError> {
    let scopes = &auth.key.record.scopes;
    if auth.has_keys_admin() || scopes.iter().any(|s| s == "catalog:write") {
        Ok(AgentWriteMode::Full)
    } else if scopes.iter().any(|s| s == AGENTS_DRAFT_SCOPE) {
        Ok(AgentWriteMode::Restricted)
    } else {
        Err(OrchestratorError::PermissionDenied(format!(
            "missing required scope 'catalog:write' or '{AGENTS_DRAFT_SCOPE}'"
        )))
    }
}

/// Release needs `publish:grant` in the caller's **effective** scopes (live role at
/// authentication, further narrowed by an explicit key's list), or `keys:admin` (the
/// operator superuser bypass). A key narrowed without `publish:grant` cannot release, and a
/// revoked publisher's keys lose it with the role.
pub fn require_publish_grant(auth: &AuthContext) -> Result<(), OrchestratorError> {
    if auth.has_keys_admin()
        || auth
            .key
            .record
            .scopes
            .iter()
            .any(|s| s == PUBLISH_GRANT_SCOPE)
    {
        Ok(())
    } else {
        Err(OrchestratorError::PermissionDenied(format!(
            "missing required scope '{PUBLISH_GRANT_SCOPE}'"
        )))
    }
}

pub fn require_release_authorization(
    store: &SqliteWorkflowStore,
    auth: &AuthContext,
    table: &str,
    urn: &str,
    label: &str,
) -> Result<(), OrchestratorError> {
    require_publish_grant(auth)?;
    store.require_not_installed(urn, label)?;
    store.require_catalog_owner(table, urn, auth.owner_user_id(), label)
}

pub fn require_skill_package_release_authorization(
    store: &SqliteWorkflowStore,
    auth: &AuthContext,
    urn: &str,
) -> Result<(), OrchestratorError> {
    require_publish_grant(auth)?;
    store.require_not_installed(urn, "skill package")?;
    store.require_catalog_owner("skill_packages", urn, auth.owner_user_id(), "skill package")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{ApiKeyRecord, AuthSource, AuthenticatedKey};
    use crate::users::UserRole;

    fn auth_with_scopes(scopes: &[&str]) -> AuthContext {
        AuthContext {
            key: AuthenticatedKey {
                record: ApiKeyRecord {
                    key_id: "test-key".to_string(),
                    name: "test".to_string(),
                    sha256_secret: "hash".to_string(),
                    principal: "User::\"tester\"".to_string(),
                    scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
                    created_at: 1,
                    last_used_at: None,
                    revoked_at: None,
                    owner_user_id: crate::users::LOCAL_USER_ID.to_string(),
                    ..Default::default()
                },
                source: AuthSource::ApiKey,
                role: UserRole::Member,
            },
        }
    }

    #[test]
    fn agent_draft_write_modes() {
        let mode = |scopes: &[&str]| require_agent_draft_write(&auth_with_scopes(scopes));
        assert_eq!(mode(&["catalog:write"]).unwrap(), AgentWriteMode::Full);
        assert_eq!(
            mode(&["catalog:write", "agents:draft"]).unwrap(),
            AgentWriteMode::Full
        );
        assert_eq!(mode(&["keys:admin"]).unwrap(), AgentWriteMode::Full);
        assert_eq!(mode(&["agents:draft"]).unwrap(), AgentWriteMode::Restricted);
        let err = mode(&["runs:write"]).expect_err("neither scope");
        assert!(err.to_string().contains("catalog:write"), "{err}");
    }

    #[test]
    fn member_runs_write_may_read_catalog() {
        require_catalog_read(&auth_with_scopes(&["runs:write", "sessions:write"]))
            .expect("member with run rights may list/get");
    }

    #[test]
    fn catalog_write_may_read_catalog() {
        require_catalog_read(&auth_with_scopes(&["catalog:write"]))
            .expect("catalog writer may list/get");
    }

    #[test]
    fn neither_runs_nor_catalog_write_is_denied() {
        let err = require_catalog_read(&auth_with_scopes(&["sessions:write", "node:read"]))
            .expect_err("no run or catalog write");
        assert!(
            matches!(err, OrchestratorError::PermissionDenied(ref msg) if msg.contains("runs:write")),
            "{err}"
        );
    }

    #[test]
    fn keys_admin_may_read_catalog() {
        require_catalog_read(&auth_with_scopes(&["keys:admin"])).expect("operator bypass");
    }

    #[test]
    fn release_requires_publish_grant_in_effective_scopes() {
        let err = require_publish_grant(&auth_with_scopes(&["catalog:write", "runs:write"]))
            .expect_err("catalog:write alone cannot release");
        assert!(
            matches!(err, OrchestratorError::PermissionDenied(ref msg) if msg.contains("publish:grant")),
            "{err}"
        );
        require_publish_grant(&auth_with_scopes(&["catalog:write", "publish:grant"]))
            .expect("publish:grant releases");
    }

    #[test]
    fn publisher_key_narrowed_without_publish_grant_cannot_release() {
        // The live role is Publisher, but the effective scopes (an explicit key's list) lack it.
        let mut auth = auth_with_scopes(&["catalog:write", "runs:write"]);
        auth.key.role = UserRole::Publisher;
        assert!(require_publish_grant(&auth).is_err());
    }

    #[test]
    fn keys_admin_may_release() {
        require_publish_grant(&auth_with_scopes(&["keys:admin"])).expect("operator bypass");
    }
}
