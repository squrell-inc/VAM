//! `GET .../<urn>/release-suggestion`. Same authorization as release.
//!
//! Agent responses include `permissionChanges`: one entry per target whose
//! effective decision moved, plus advanced-ref adds and removals, and the
//! legacy Cedar-to-rules entry. Other kinds leave the list empty. The CLI
//! prints each `sentence` before the version prompt.

use axum::response::Response;

use crate::api::auth::AuthContext;
use crate::api::catalog_auth::{
    require_release_authorization, require_skill_package_release_authorization,
};
use crate::api::error::{json_response, ApiError};
use crate::api::{json_http_response, percent_decode_path, AppState};
use crate::catalog_suggest::SuggestKind;

pub(crate) const SUGGESTION_SUFFIX: &str = "/release-suggestion";

/// The URN in `rest` when it names a release-suggestion request.
pub(crate) fn suggestion_urn(rest: &str) -> Option<String> {
    rest.trim_start_matches('/')
        .strip_suffix(SUGGESTION_SUFFIX)
        .map(percent_decode_path)
}

pub(crate) fn release_suggestion(
    state: &AppState,
    auth: &AuthContext,
    kind: SuggestKind,
    urn: &str,
) -> Result<Response, ApiError> {
    match kind {
        SuggestKind::Agent => {
            require_release_authorization(&state.store, auth, "agents", urn, "agent")?
        }
        SuggestKind::Playbook => {
            require_release_authorization(&state.store, auth, "playbooks", urn, "playbook")?
        }
        SuggestKind::Skill => {
            require_release_authorization(&state.store, auth, "skills", urn, "skill")?
        }
        SuggestKind::Policy => {
            require_release_authorization(&state.store, auth, "policies", urn, "policy")?
        }
        SuggestKind::SkillPackage => {
            require_skill_package_release_authorization(state.store.as_ref(), auth, urn)?
        }
    }
    let resp = state.store.release_suggestion(kind, urn)?;
    let (code, json) = json_response("vam.v1.ReleaseSuggestion", &resp)?;
    Ok(json_http_response(code, json))
}
