//! Secret requirement scope plus discovery and admission surfacing.
//!
//! `SecretRequirement.scope` declares which vault partition can satisfy a
//! tool's secret: `org`, `user`, or `either`. Discovery resolves declared
//! requirements against the caller's vault without revealing whether another
//! user configured a user-scope secret (org existence is shared on purpose).
//! Admission fails fast listing which bindings are missing and at which scope.
//! Tool call stays fail-closed with a typed missing-binding error.
//!
//! Invoke-path `resolved_secrets` is unchanged.

use crate::error::OrchestratorError;
use crate::secrets::{SecretScope, SecretsVault};

/// Declared vault partition for a `SecretRequirement`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementScope {
    Org,
    User,
    Either,
}

impl RequirementScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Org => "org",
            Self::User => "user",
            Self::Either => "either",
        }
    }

    pub fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "org" => Ok(Self::Org),
            "user" => Ok(Self::User),
            "either" => Ok(Self::Either),
            _ => Err(OrchestratorError::InvalidContract(format!(
                "invalid secret requirement scope '{value}'; expected org, user, or either"
            ))),
        }
    }

    /// Proto `SCOPE_UNSPECIFIED` (0) defaults to `Either` for backwards
    /// compatibility with manifests that only declare `{name, required}`.
    pub fn from_proto(value: i32) -> Result<Self, OrchestratorError> {
        match value {
            0 | 3 => Ok(Self::Either),
            1 => Ok(Self::Org),
            2 => Ok(Self::User),
            _ => Err(OrchestratorError::InvalidContract(format!(
                "invalid secret requirement scope {value}; expected 0-3"
            ))),
        }
    }

    pub fn to_proto(self) -> i32 {
        match self {
            Self::Org => 1,
            Self::User => 2,
            Self::Either => 3,
        }
    }
}

/// One missing required binding for admission errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRequirement {
    pub binding_id: String,
    pub scope: RequirementScope,
}

impl MissingRequirement {
    pub fn describe(&self) -> String {
        format!("{}@{}", self.binding_id, self.scope.as_str())
    }
}

/// Format an admission missing list as `binding@scope, ...` for the typed error.
pub fn format_missing(missing: &[MissingRequirement]) -> String {
    missing
        .iter()
        .map(MissingRequirement::describe)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where an `either` requirement was satisfied. `None` when missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SatisfiedScope {
    Org,
    User,
}

impl SatisfiedScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Org => "org",
            Self::User => "user",
        }
    }

    pub fn to_host_scope_proto(self) -> i32 {
        match self {
            Self::Org => 1,
            Self::User => 2,
        }
    }
}

/// Resolve one requirement against the caller's vault.
///
/// Only checks org rows plus the caller's own user-scope row. Never probes
/// another user's vault, so a `false` result does not reveal whether another
/// user configured the same binding.
pub fn is_requirement_satisfied(
    vault: &SecretsVault,
    owner_user_id: &str,
    binding_id: &str,
    scope: RequirementScope,
) -> Result<bool, OrchestratorError> {
    if binding_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "secret requirement name is required".into(),
        ));
    }
    let satisfied = match scope {
        RequirementScope::Org => vault.has_binding_at(
            SecretScope::Org,
            crate::secrets::ORG_OWNER_USER_ID,
            binding_id,
        )?,
        RequirementScope::User => {
            vault.has_binding_at(SecretScope::User, owner_user_id, binding_id)?
        }
        RequirementScope::Either => {
            if vault.has_binding_at(SecretScope::User, owner_user_id, binding_id)? {
                true
            } else {
                vault.has_binding_at(
                    SecretScope::Org,
                    crate::secrets::ORG_OWNER_USER_ID,
                    binding_id,
                )?
            }
        }
    };
    Ok(satisfied)
}

/// Resolve where a requirement is satisfied, if anywhere visible to the caller.
pub fn satisfied_scope_for(
    vault: &SecretsVault,
    owner_user_id: &str,
    binding_id: &str,
    scope: RequirementScope,
) -> Result<Option<SatisfiedScope>, OrchestratorError> {
    match scope {
        RequirementScope::Org => {
            if vault.has_binding_at(
                SecretScope::Org,
                crate::secrets::ORG_OWNER_USER_ID,
                binding_id,
            )? {
                Ok(Some(SatisfiedScope::Org))
            } else {
                Ok(None)
            }
        }
        RequirementScope::User => {
            if vault.has_binding_at(SecretScope::User, owner_user_id, binding_id)? {
                Ok(Some(SatisfiedScope::User))
            } else {
                Ok(None)
            }
        }
        RequirementScope::Either => {
            if vault.has_binding_at(SecretScope::User, owner_user_id, binding_id)? {
                Ok(Some(SatisfiedScope::User))
            } else if vault.has_binding_at(
                SecretScope::Org,
                crate::secrets::ORG_OWNER_USER_ID,
                binding_id,
            )? {
                Ok(Some(SatisfiedScope::Org))
            } else {
                Ok(None)
            }
        }
    }
}

/// Discovery: resolve declared requirements against the caller's vault.
///
/// Returns `(configured, satisfied_scope)` per requirement in input order.
/// Optional requirements that are missing are reported, not failed.
pub fn discover_requirements(
    vault: &SecretsVault,
    owner_user_id: &str,
    requirements: &[vam_spec::v1::SecretRequirement],
) -> Result<Vec<vam_spec::v1::SecretRequirementStatus>, OrchestratorError> {
    let mut statuses = Vec::with_capacity(requirements.len());
    for req in requirements {
        if req.name.trim().is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "secret requirement name is required".into(),
            ));
        }
        let scope = RequirementScope::from_proto(req.scope)?;
        let satisfied = satisfied_scope_for(vault, owner_user_id, &req.name, scope)?;
        statuses.push(vam_spec::v1::SecretRequirementStatus {
            binding_id: req.name.clone(),
            scope: scope.to_proto(),
            required: req.required,
            configured: satisfied.is_some(),
            satisfied_scope: satisfied.map(|s| s.to_host_scope_proto()).unwrap_or(0),
        });
    }
    Ok(statuses)
}

/// Admission: fail fast when a required requirement is not satisfied.
///
/// Returns the typed [`OrchestratorError::MissingRequiredSecrets`] listing
/// `binding@scope` for each missing required requirement. Optional missing
/// requirements are ignored.
pub fn admit_requirements(
    vault: &SecretsVault,
    owner_user_id: &str,
    requirements: &[vam_spec::v1::SecretRequirement],
) -> Result<(), OrchestratorError> {
    let statuses = discover_requirements(vault, owner_user_id, requirements)?;
    let missing: Vec<MissingRequirement> = statuses
        .iter()
        .zip(requirements.iter())
        .filter(|(status, req)| req.required && !status.configured)
        .map(|(status, _)| {
            let scope =
                RequirementScope::from_proto(status.scope).unwrap_or(RequirementScope::Either);
            MissingRequirement {
                binding_id: status.binding_id.clone(),
                scope,
            }
        })
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(OrchestratorError::MissingRequiredSecrets(format_missing(
            &missing,
        )))
    }
}

/// Collect every `required_secrets` entry across skill manifests.
pub fn collect_skill_requirements(
    skills: &[vam_spec::v1::SkillManifest],
) -> Vec<vam_spec::v1::SecretRequirement> {
    let mut out = Vec::new();
    for skill in skills {
        for tool in &skill.tools {
            out.extend(tool.required_secrets.iter().cloned());
        }
    }
    out
}

/// Typed call-time error for a missing binding at a declared scope.
///
/// Callers keep fail-closed behavior; this replaces vague lookup failures
/// with `binding + scope`.
pub fn missing_binding_error(binding_id: &str, scope: RequirementScope) -> OrchestratorError {
    OrchestratorError::MissingSecretBinding {
        binding_id: binding_id.to_string(),
        scope: scope.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretKind;
    use crate::workflow::SqliteWorkflowStore;
    use std::sync::Arc;

    fn test_vault() -> Arc<SecretsVault> {
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        SecretsVault::for_tests(store)
    }

    fn requirement(
        name: &str,
        scope: RequirementScope,
        required: bool,
    ) -> vam_spec::v1::SecretRequirement {
        vam_spec::v1::SecretRequirement {
            name: name.to_string(),
            required,
            scope: scope.to_proto(),
        }
    }

    #[test]
    fn scope_unspecified_defaults_to_either() {
        assert_eq!(
            RequirementScope::from_proto(0).expect("unspecified"),
            RequirementScope::Either
        );
        assert_eq!(
            RequirementScope::from_proto(3).expect("either"),
            RequirementScope::Either
        );
    }

    #[test]
    fn scope_parse_accepts_org_user_either() {
        assert_eq!(
            RequirementScope::parse("org").expect("org"),
            RequirementScope::Org
        );
        assert_eq!(
            RequirementScope::parse("USER").expect("user"),
            RequirementScope::User
        );
        assert_eq!(
            RequirementScope::parse("either").expect("either"),
            RequirementScope::Either
        );
        assert!(RequirementScope::parse("other").is_err());
    }

    #[test]
    fn discovery_org_user_either_against_caller_vault() {
        let vault = test_vault();
        vault
            .put_secret_at(
                "shared/api-key",
                "org-value",
                SecretKind::Egress,
                &[],
                None,
                SecretScope::Org,
                crate::secrets::ORG_OWNER_USER_ID,
            )
            .expect("org secret");
        vault
            .put_secret_at(
                "mine/token",
                "alice-value",
                SecretKind::Egress,
                &[],
                None,
                SecretScope::User,
                "alice",
            )
            .expect("alice secret");

        let reqs = vec![
            requirement("shared/api-key", RequirementScope::Org, true),
            requirement("mine/token", RequirementScope::User, true),
            requirement("mine/token", RequirementScope::Either, true),
            requirement("shared/api-key", RequirementScope::Either, true),
            requirement("missing/binding", RequirementScope::Either, true),
            requirement("missing/optional", RequirementScope::User, false),
        ];
        let statuses = discover_requirements(&vault, "alice", &reqs).expect("discover");
        assert_eq!(statuses.len(), 6);
        assert!(statuses[0].configured);
        assert_eq!(statuses[0].satisfied_scope, 1);
        assert!(statuses[1].configured);
        assert_eq!(statuses[1].satisfied_scope, 2);
        assert!(statuses[2].configured);
        assert!(statuses[3].configured);
        assert!(!statuses[4].configured);
        assert_eq!(statuses[4].satisfied_scope, 0);
        assert!(!statuses[5].configured);

        // Bob sees Alice's user row as missing (no leak).
        let bob = discover_requirements(&vault, "bob", &reqs[1..2]).expect("bob discover");
        assert!(!bob[0].configured);
    }

    #[test]
    fn discovery_does_not_leak_other_user_rows() {
        let vault = test_vault();
        vault
            .put_secret_at(
                "jira/auth-header",
                "bob-value",
                SecretKind::Egress,
                &[],
                None,
                SecretScope::User,
                "bob",
            )
            .expect("bob secret");

        let reqs = vec![requirement(
            "jira/auth-header",
            RequirementScope::User,
            true,
        )];
        let alice = discover_requirements(&vault, "alice", &reqs).expect("alice");
        assert!(!alice[0].configured);
        assert_eq!(alice[0].binding_id, "jira/auth-header");
        // Response carries only caller-visible state: no owner, no fingerprint.
        assert_eq!(alice[0].satisfied_scope, 0);

        let bob = discover_requirements(&vault, "bob", &reqs).expect("bob");
        assert!(bob[0].configured);
    }

    #[test]
    fn admission_lists_missing_binding_plus_scope() {
        let vault = test_vault();
        let reqs = vec![
            requirement("jira/auth-header", RequirementScope::User, true),
            requirement("openai/api-key", RequirementScope::Org, true),
            requirement("optional/note", RequirementScope::Either, false),
        ];
        let err = admit_requirements(&vault, "alice", &reqs).expect_err("admission fails");
        match err {
            OrchestratorError::MissingRequiredSecrets(msg) => {
                assert!(msg.contains("jira/auth-header@user"), "{msg}");
                assert!(msg.contains("openai/api-key@org"), "{msg}");
                assert!(!msg.contains("optional/note"), "{msg}");
            }
            other => panic!("expected MissingRequiredSecrets, got {other:?}"),
        }
    }

    #[test]
    fn admission_ignores_optional_missing() {
        let vault = test_vault();
        let reqs = vec![requirement(
            "optional/note",
            RequirementScope::Either,
            false,
        )];
        admit_requirements(&vault, "alice", &reqs).expect("optional passes");
    }

    #[test]
    fn user_scope_has_no_org_fallback() {
        let vault = test_vault();
        vault
            .put_secret_at(
                "jira/auth-header",
                "org-value",
                SecretKind::Egress,
                &[],
                None,
                SecretScope::Org,
                crate::secrets::ORG_OWNER_USER_ID,
            )
            .expect("org secret");
        let reqs = vec![requirement(
            "jira/auth-header",
            RequirementScope::User,
            true,
        )];
        let statuses = discover_requirements(&vault, "alice", &reqs).expect("discover");
        assert!(!statuses[0].configured);
    }

    #[test]
    fn typed_missing_binding_error_carries_scope() {
        let err = missing_binding_error("jira/auth-header", RequirementScope::User);
        let msg = err.to_string();
        match err {
            OrchestratorError::MissingSecretBinding { binding_id, scope } => {
                assert_eq!(binding_id, "jira/auth-header");
                assert_eq!(scope, "user");
                assert!(msg.contains("jira/auth-header"));
                assert!(msg.contains("user"));
            }
            other => panic!("expected MissingSecretBinding, got {other:?}"),
        }
    }
}
