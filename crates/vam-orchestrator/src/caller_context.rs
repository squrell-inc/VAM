//! Host-stamped [`CallerContext`] for HSP invoke and lifecycle hooks.
//!
//! Identity is copied from the run/session record — never from guest args.
//! Brokers are trusted host processes that may *see* this context; Cedar
//! stays the host policy decision point.

use vam_spec::v1::{CallerContext, IdentityRequirement, RunSpec, Version};

use crate::error::OrchestratorError;
use crate::run_context::resolve_thread_ref;
use crate::secrets::SecretsVault;
use crate::workflow::SqliteWorkflowStore;
use secrecy::ExposeSecret;
use std::collections::HashMap;
use vam_spec::v1::ManifestSecretBinding;

/// Host-stamped caller identity for a session/run. Guest args are ignored.
pub fn stamp_caller_context(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    agent_urn_hint: Option<&str>,
    run_spec: Option<&RunSpec>,
) -> Result<CallerContext, OrchestratorError> {
    let user_id = stamp_user_id(store, session_id, run_id);
    let tenant_id = vam_telemetry::current_tenant_id().as_str().to_string();

    let (spec_json, _, run_agent_urn) = match store.get_run_spec_context(run_id) {
        Ok(ctx) => ctx,
        Err(OrchestratorError::NotFound(_)) => (None, None, None),
        Err(err) => return Err(err),
    };

    let parsed_spec = run_spec.cloned().or_else(|| {
        spec_json.as_deref().and_then(|json| {
            vam_spec::encoding::json_to_proto::<RunSpec>("vam.v1.RunSpec", json).ok()
        })
    });

    let agent_urn = run_agent_urn
        .filter(|urn| !urn.trim().is_empty())
        .or_else(|| {
            agent_urn_hint
                .map(str::trim)
                .filter(|urn| !urn.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();

    let (agent_version, agent_digest) = match parsed_spec.as_ref() {
        Some(spec) => (
            version_string(spec.agent_ref.as_ref().and_then(|r| r.version.as_ref())),
            agent_digest_from_spec(spec),
        ),
        None => (String::new(), String::new()),
    };

    let workspace_handle = match store.load_session(session_id) {
        Ok(session) => session.workspace_id,
        Err(OrchestratorError::NotFound(_)) => String::new(),
        Err(err) => return Err(err),
    };

    let thread_handle = match parsed_spec.as_ref() {
        Some(spec) if !user_id.is_empty() => resolve_thread_ref(&user_id, spec).unwrap_or_default(),
        Some(spec) => spec.thread_ref.clone(),
        None => String::new(),
    };

    let (space_id, thread_id) = parsed_spec
        .as_ref()
        .map(|spec| (spec.space_id.clone(), spec.thread_id.clone()))
        .unwrap_or_default();
    // `broker:<id>` for a triggered run, empty for a person (or an unknown run).
    let run_source = store.run_source(run_id).ok().flatten().unwrap_or_default();

    Ok(CallerContext {
        space_id,
        thread_id,
        run_source,
        tenant_id,
        user_id,
        agent_urn,
        agent_version,
        agent_digest,
        workspace_handle,
        thread_handle,
    })
}

fn stamp_user_id(store: &SqliteWorkflowStore, session_id: &str, run_id: &str) -> String {
    if !run_id.trim().is_empty() {
        if let Ok(owner) = store.run_owner_user_id(run_id) {
            if !owner.trim().is_empty() {
                return owner;
            }
        }
    }
    if !session_id.trim().is_empty() {
        if let Ok(owner) = store.session_owner_user_id(session_id) {
            if !owner.trim().is_empty() {
                return owner;
            }
        }
    }
    String::new()
}

fn version_string(version: Option<&Version>) -> String {
    let Some(v) = version else {
        return String::new();
    };
    if v.major == 0 && v.minor == 0 && v.patch == 0 && v.prerelease.is_empty() && v.build.is_empty()
    {
        return String::new();
    }
    let mut out = format!("{}.{}.{}", v.major, v.minor, v.patch);
    if !v.prerelease.is_empty() {
        out.push('-');
        out.push_str(&v.prerelease);
    }
    if !v.build.is_empty() {
        out.push('+');
        out.push_str(&v.build);
    }
    out
}

fn agent_digest_from_spec(spec: &RunSpec) -> String {
    spec.agent_ref
        .as_ref()
        .and_then(|r| r.digest.as_ref())
        .map(|d| d.hex_hash.clone())
        .filter(|hex| !hex.trim().is_empty())
        .unwrap_or_default()
}

/// True when the broker declared at least one required identity field.
pub fn identity_requirement_active(req: Option<&IdentityRequirement>) -> bool {
    match req {
        Some(req) => {
            req.tenant_id || req.user_id || req.agent || req.workspace_handle || req.thread_handle
        }
        None => false,
    }
}

/// Presented identity_requirement must not exceed the operator-approved set.
pub fn identity_requirement_is_subset(
    presented: Option<&IdentityRequirement>,
    approved: Option<&IdentityRequirement>,
) -> bool {
    let presented = presented.cloned().unwrap_or_default();
    let approved = approved.cloned().unwrap_or_default();
    (!presented.tenant_id || approved.tenant_id)
        && (!presented.user_id || approved.user_id)
        && (!presented.agent || approved.agent)
        && (!presented.workspace_handle || approved.workspace_handle)
        && (!presented.thread_handle || approved.thread_handle)
}

/// Fail closed when the approved manifest requires a field the host could not stamp.
pub fn assert_identity_satisfied(
    requirement: Option<&IdentityRequirement>,
    caller: Option<&CallerContext>,
) -> Result<(), OrchestratorError> {
    if !identity_requirement_active(requirement) {
        return Ok(());
    }
    let Some(req) = requirement else {
        return Ok(());
    };
    let Some(caller) = caller else {
        return Err(deny_missing_identity());
    };
    if req.tenant_id && caller.tenant_id.trim().is_empty() {
        return Err(deny_missing_identity());
    }
    if req.user_id && caller.user_id.trim().is_empty() {
        return Err(deny_missing_identity());
    }
    if req.agent && caller.agent_urn.trim().is_empty() {
        return Err(deny_missing_identity());
    }
    if req.workspace_handle && caller.workspace_handle.trim().is_empty() {
        return Err(deny_missing_identity());
    }
    if req.thread_handle && caller.thread_handle.trim().is_empty() {
        return Err(deny_missing_identity());
    }
    Ok(())
}

fn deny_missing_identity() -> OrchestratorError {
    OrchestratorError::Hsp("hsp denied: missing caller identity".into())
}

/// Resolve HSP manifest bindings against the calling user's vault.
///
/// User-scope row for `caller_user_id` wins. Else org-scope. Never another
/// user's user-scope row. A row is given to the broker only when it is not an
/// MCP connection credential and its pinned destinations lie inside the
/// broker's approved `egress_domains`
/// ([`SecretsVault::decrypt_secret_for_broker`]). A dedicated user-scope
/// lookup still has no implicit fallback ([`SecretsVault::decrypt_secret_at`]).
pub fn resolve_secrets_for_caller(
    vault: Option<&SecretsVault>,
    caller_user_id: &str,
    bindings: &[ManifestSecretBinding],
    egress_domains: &[String],
) -> Result<HashMap<String, String>, String> {
    let mut resolved = HashMap::new();
    for binding in bindings {
        if binding.required && binding.vault_key.trim().is_empty() {
            return Err(format!(
                "hsp denied: unbound secret: {}",
                binding.logical_name
            ));
        }
        match vault {
            Some(vault) => {
                match vault.decrypt_secret_for_broker(
                    caller_user_id,
                    &binding.vault_key,
                    egress_domains,
                ) {
                    Ok(value) => {
                        resolved.insert(
                            binding.logical_name.clone(),
                            value.expose_secret().to_string(),
                        );
                    }
                    // The vault holds the row but this broker may not have it.
                    // Refused even for an optional binding: skipping would hide
                    // that the manifest names a credential it cannot be given.
                    Err(OrchestratorError::PermissionDenied(_)) => {
                        return Err(format!(
                            "hsp denied: secret not permitted for this broker: {}",
                            binding.logical_name
                        ));
                    }
                    Err(_) if binding.required => {
                        return Err(format!(
                            "hsp denied: unbound secret: {}",
                            binding.logical_name
                        ));
                    }
                    Err(_) => {}
                }
            }
            None if binding.required => {
                return Err(format!(
                    "hsp denied: unbound secret: {}",
                    binding.logical_name
                ));
            }
            None => {}
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{SecretKind, SecretScope};
    use std::sync::Arc;
    use vam_spec::v1::{Digest, ResourceId, ResourceRef, Version};

    fn identity(user: bool, agent: bool) -> IdentityRequirement {
        IdentityRequirement {
            tenant_id: false,
            user_id: user,
            agent,
            workspace_handle: false,
            thread_handle: false,
        }
    }

    #[test]
    fn identity_subset_rejects_unapproved_user_id() {
        let presented = identity(true, false);
        assert!(!identity_requirement_is_subset(
            Some(&presented),
            Some(&identity(false, false))
        ));
        assert!(!identity_requirement_is_subset(Some(&presented), None));
        assert!(identity_requirement_is_subset(
            Some(&presented),
            Some(&identity(true, true))
        ));
        assert!(identity_requirement_is_subset(None, Some(&presented)));
    }

    #[test]
    fn missing_required_user_id_is_denied() {
        let req = identity(true, false);
        let err = assert_identity_satisfied(Some(&req), None).unwrap_err();
        assert!(matches!(err, OrchestratorError::Hsp(ref msg) if msg.contains("missing caller")));
        let empty = CallerContext {
            user_id: String::new(),
            ..Default::default()
        };
        assert!(assert_identity_satisfied(Some(&req), Some(&empty)).is_err());
        let stamped = CallerContext {
            user_id: "alice".into(),
            ..Default::default()
        };
        assert!(assert_identity_satisfied(Some(&req), Some(&stamped)).is_ok());
    }

    #[test]
    fn stamp_uses_run_owner_not_guest_principal() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store.create_user("alice", "alice").unwrap();
        store
            .create_session_with_owner(
                "sess-alice",
                None,
                "vm",
                "alice--assistant",
                "storage-default",
                "alice",
            )
            .unwrap();
        store
            .persist_run_spec_with_owner(
                "run-alice",
                "{}",
                Some("sess-alice"),
                "urn:vam:agent:local.vam.dev/acme/tester",
                None,
                "User::\"attacker\"",
                "alice",
            )
            .unwrap();

        let caller = stamp_caller_context(
            &store,
            "sess-alice",
            "run-alice",
            Some("urn:vam:agent:from-hint"),
            None,
        )
        .unwrap();
        assert_eq!(caller.user_id, "alice");
        assert_ne!(caller.user_id, "attacker");
        assert_eq!(caller.agent_urn, "urn:vam:agent:local.vam.dev/acme/tester");
        assert_eq!(caller.workspace_handle, "alice--assistant");
    }

    #[test]
    fn stamp_thread_and_agent_version_from_host_run_spec() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store.create_user("alice", "alice").unwrap();
        store
            .create_session_with_owner(
                "sess-alice",
                None,
                "vm",
                "alice--assistant",
                "storage-default",
                "alice",
            )
            .unwrap();
        store
            .persist_run_spec_with_owner(
                "run-alice",
                "{}",
                Some("sess-alice"),
                "urn:vam:agent:local.vam.dev/acme/tester",
                None,
                "User::\"alice\"",
                "alice",
            )
            .unwrap();

        let spec = RunSpec {
            thread_ref: "feature".into(),
            space_id: "sp_1".into(),
            thread_id: "th_1".into(),
            agent_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: "urn:vam:agent:local.vam.dev/acme/tester".into(),
                }),
                version: Some(Version {
                    major: 1,
                    minor: 2,
                    patch: 3,
                    prerelease: String::new(),
                    build: String::new(),
                }),
                digest: Some(Digest {
                    algorithm: 1,
                    hex_hash: "abcd".into(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let caller =
            stamp_caller_context(&store, "sess-alice", "run-alice", None, Some(&spec)).unwrap();
        assert_eq!(caller.thread_handle, "alice--feature");
        assert_eq!(caller.agent_version, "1.2.3");
        assert_eq!(caller.agent_digest, "abcd");
        assert_eq!(
            (caller.space_id.as_str(), caller.thread_id.as_str()),
            ("sp_1", "th_1")
        );
        assert_eq!(caller.run_source, "");
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE runs SET source = 'broker:b1' WHERE run_id = 'run-alice'",
                [],
            )
            .unwrap();
        let caller =
            stamp_caller_context(&store, "sess-alice", "run-alice", None, Some(&spec)).unwrap();
        assert_eq!(caller.run_source, "broker:b1");
    }

    #[test]
    fn resolve_secrets_uses_caller_user_row_not_peers() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.create_user("alice", "alice").unwrap();
        store.create_user("bob", "bob").unwrap();
        let vault = SecretsVault::for_tests(store);
        vault
            .put_secret_with_owner(
                "jira/auth-header",
                "alice-secret",
                SecretKind::Egress,
                &[],
                None,
                "alice",
            )
            .unwrap();
        vault
            .put_secret_with_owner(
                "jira/auth-header",
                "bob-secret",
                SecretKind::Egress,
                &[],
                None,
                "bob",
            )
            .unwrap();
        vault
            .put_secret_at(
                "jira/auth-header",
                "org-secret",
                SecretKind::Egress,
                &[],
                None,
                SecretScope::Org,
                "",
            )
            .unwrap();

        let bindings = vec![ManifestSecretBinding {
            logical_name: "auth".into(),
            vault_key: "jira/auth-header".into(),
            required: true,
        }];
        let alice =
            resolve_secrets_for_caller(Some(vault.as_ref()), "alice", &bindings, &[]).unwrap();
        let bob = resolve_secrets_for_caller(Some(vault.as_ref()), "bob", &bindings, &[]).unwrap();
        assert_eq!(alice.get("auth").map(String::as_str), Some("alice-secret"));
        assert_eq!(bob.get("auth").map(String::as_str), Some("bob-secret"));
        assert_ne!(alice.get("auth"), bob.get("auth"));
    }
}
