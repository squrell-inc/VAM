use std::sync::Arc;

use axum::async_trait;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use vam_security::{CedarPolicyEngine, PolicyDecisionPoint, PolicyMode};

use crate::error::OrchestratorError;
use crate::keys::AuthenticatedKey;
use crate::workflow::SqliteWorkflowStore;

pub const CONTROL_PLANE_POLICY: &str = r#"
// Scope matching is enforced in Rust before Cedar evaluation; this policy allows
// authenticated API-key principals that reach the PDP.
permit (
  principal,
  action,
  resource
);
"#;

#[derive(Clone)]
pub struct AuthContext {
    pub key: AuthenticatedKey,
}

impl AuthContext {
    /// Control-plane owner for rows created by this caller.
    /// Session tokens stamp the logged-in `users.user_id`. API keys stamp
    /// the bound `api_keys.owner_user_id` (bootstrap stays `operator`).
    pub fn owner_user_id(&self) -> &str {
        crate::owner::owner_user_id_for_authenticated_key(&self.key.record)
    }

    /// Persisted role of the bound user. Isolation filters attach to this.
    pub fn role(&self) -> crate::users::UserRole {
        self.key.role
    }

    /// True when the caller holds `users:admin` (org admin / operator).
    pub fn has_users_admin(&self) -> bool {
        self.key
            .record
            .scopes
            .iter()
            .any(|s| s == crate::keys::USERS_ADMIN_SCOPE)
    }

    /// True when the caller holds `keys:admin` (bootstrap / operator).
    pub fn has_keys_admin(&self) -> bool {
        self.key
            .record
            .scopes
            .iter()
            .any(|s| s == crate::keys::KEYS_ADMIN_SCOPE)
    }
}

#[derive(Clone)]
pub struct ControlPlanePdp {
    _inner: Arc<dyn PolicyDecisionPoint>,
}

impl ControlPlanePdp {
    pub fn new() -> Result<Self, OrchestratorError> {
        let engine = CedarPolicyEngine::empty(PolicyMode::Enforce, 1).map_err(|e| {
            OrchestratorError::Core(format!("control-plane Cedar compile failed: {}", e))
        })?;
        Ok(Self {
            _inner: Arc::new(engine),
        })
    }

    /// Scope checks are authoritative in v1; principals are formatted as
    /// `ApiKey::"<id>"` for audit compatibility with Cedar tooling.
    pub fn authorize(
        &self,
        auth: &AuthContext,
        _action: &str,
        required_scope: &str,
    ) -> Result<(), OrchestratorError> {
        if auth.key.record.scopes.iter().any(|s| s == "keys:admin") {
            return Ok(());
        }
        if auth.key.record.scopes.iter().any(|s| s == required_scope) {
            Ok(())
        } else {
            Err(OrchestratorError::PermissionDenied(format!(
                "missing required scope '{}'",
                required_scope
            )))
        }
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for AuthContext
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthContext>()
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({
                        "code": "UNAUTHENTICATED",
                        "message": "missing bearer token",
                        "retryable": false
                    })),
                )
                    .into_response()
            })
    }
}

pub async fn bearer_auth_middleware(
    store: Arc<SqliteWorkflowStore>,
    mut req: Request<Body>,
    next: Next<Body>,
) -> Response {
    let path = req.uri().path();
    if path == "/v1/health"
        || path == "/openapi.yaml"
        || path == "/docs"
        || path.starts_with("/docs/")
        || path == "/v1/login"
        || path == "/v1/login/refresh"
    {
        return next.run(req).await;
    }

    let token = extract_bearer(req.headers());
    let token = match token {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({
                    "code": "UNAUTHENTICATED",
                    "message": "Authorization: Bearer vam_sk_... required",
                    "retryable": false
                })),
            )
                .into_response();
        }
    };

    match store.authenticate_bearer(&token) {
        Ok(auth) => {
            req.extensions_mut().insert(AuthContext { key: auth });
            next.run(req).await
        }
        Err(e) => {
            // Identity read must not leak a 403 body for a dead credential.
            // The VAM app treats 401 as "session gone"; 403 on `/v1/users` already
            // means "I am a member without users:admin".
            let err = if path == "/v1/me" {
                match e {
                    OrchestratorError::PermissionDenied(_) => OrchestratorError::Unauthenticated,
                    other => other,
                }
            } else {
                e
            };
            let api_err: crate::api::error::ApiError = err.into();
            api_err.into_response()
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)] // test module position left as is; moving it is a src/ layout change
mod tests {
    use super::*;
    use crate::keys::{
        ApiKeyRecord, AuthSource, AuthenticatedKey, PUBLISH_GRANT_SCOPE, USERS_ADMIN_SCOPE,
    };
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
    fn users_admin_scope_denied_without_grant() {
        let pdp = ControlPlanePdp::new().expect("pdp");
        let err = pdp
            .authorize(
                &auth_with_scopes(&["catalog:write", "sessions:write"]),
                "AdminUsers",
                USERS_ADMIN_SCOPE,
            )
            .expect_err("missing users:admin");
        assert!(
            matches!(err, OrchestratorError::PermissionDenied(ref msg) if msg.contains(USERS_ADMIN_SCOPE)),
            "{err}"
        );
    }

    #[test]
    fn catalog_write_does_not_imply_publish_grant() {
        let pdp = ControlPlanePdp::new().expect("pdp");
        let writer = auth_with_scopes(&["catalog:write"]);
        let err = pdp
            .authorize(&writer, "GrantPublish", PUBLISH_GRANT_SCOPE)
            .expect_err("catalog:write must not satisfy publish:grant");
        assert!(
            matches!(err, OrchestratorError::PermissionDenied(ref msg) if msg.contains(PUBLISH_GRANT_SCOPE)),
            "{err}"
        );
        pdp.authorize(
            &auth_with_scopes(&[PUBLISH_GRANT_SCOPE]),
            "GrantPublish",
            PUBLISH_GRANT_SCOPE,
        )
        .expect("explicit publish:grant is enough");
        pdp.authorize(
            &auth_with_scopes(&[USERS_ADMIN_SCOPE]),
            "AdminUsers",
            USERS_ADMIN_SCOPE,
        )
        .expect("explicit users:admin is enough");
    }

    #[test]
    fn keys_admin_still_satisfies_new_scopes() {
        let pdp = ControlPlanePdp::new().expect("pdp");
        let operator = auth_with_scopes(&["keys:admin"]);
        pdp.authorize(&operator, "AdminUsers", USERS_ADMIN_SCOPE)
            .expect("keys:admin bypass");
        pdp.authorize(&operator, "GrantPublish", PUBLISH_GRANT_SCOPE)
            .expect("keys:admin bypass");
    }
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn require_idempotency_key(headers: &HeaderMap) -> Result<String, OrchestratorError> {
    headers
        .get("Idempotency-Key")
        .or_else(|| headers.get("idempotency-key"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(OrchestratorError::MissingIdempotencyKey)
}
