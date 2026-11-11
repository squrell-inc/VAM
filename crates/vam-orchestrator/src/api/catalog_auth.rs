//! Owner-only versioning, catalog read vs write, and live `publish:grant`.

use crate::api::auth::AuthContext;
use crate::error::OrchestratorError;
use crate::keys::PUBLISH_GRANT_SCOPE;
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

/// Live role is the source of truth so org-admin revoke sticks even when an
/// older token still lists `publish:grant`. `keys:admin` keeps the operator
/// superuser bypass used by the bootstrap key.
pub fn require_publish_grant(auth: &AuthContext) -> Result<(), OrchestratorError> {
    if auth.has_keys_admin() || auth.role().has_publish_grant() {
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
}
