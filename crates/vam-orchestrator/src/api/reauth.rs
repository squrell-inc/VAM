//! A reconnect resumes the runs parked on that connection (E10 Phase 4, AR-4.9).
//!
//! A call to an MCP connection that needs signing in again is parked at the intent
//! (`security::park_for_reauth`): the run waits PAUSED with `waiting_for: reauth`. When the
//! connection is connected again, this resolves every park on it. A reconnect is not a
//! permission: the park is decided `RECONNECTED`, which mints no grant, and the call is decided
//! again like any other when it is re-run (an ask policy asks, a deny denies). Only a park
//! on this very connection (same scope, owner and id) is touched. A stop already rejects an
//! undecided reauth park like any approval.

use crate::mcp_registry::{McpConnection, McpOauthStatus};

use super::AppState;

/// Resolves the undecided reauth parks on `conn`: each is decided `RECONNECTED` (no grant). Does
/// nothing unless the connection is connected. Returns the decided parks, to be resumed.
pub(crate) fn resolve(
    store: &crate::workflow::SqliteWorkflowStore,
    conn: &McpConnection,
) -> Vec<crate::error::ApprovalRecord> {
    let connected = conn
        .oauth
        .as_ref()
        .is_some_and(|oauth| oauth.status == McpOauthStatus::Connected);
    if !connected {
        return Vec::new();
    }
    let key = crate::hitl::reauth_key(conn.scope, &conn.owner_user_id, &conn.connection_id);
    resolve_key(store, &key)
}

/// Resolves the undecided reauth parks named by `key` (`reauth:<scope>:<owner>:<id>`): each is
/// decided `RECONNECTED`. The key carries the owner, so only that owner's parks are touched.
pub(crate) fn resolve_key(
    store: &crate::workflow::SqliteWorkflowStore,
    key: &str,
) -> Vec<crate::error::ApprovalRecord> {
    let pending = match store.pending_reauth_approvals(key) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("reauth: parked calls could not be listed: {e}");
            return Vec::new();
        }
    };
    let mut decided = Vec::new();
    for (request_id, run_owner) in pending {
        let token_id = vam_spec::v1::ResourceId {
            urn: format!("urn:vam:token:local.vam.dev/acme/{}", super::uuid_like()),
        };
        match store.decide_approval_with_surface(
            &vam_spec::v1::ResourceId {
                urn: request_id.clone(),
            },
            &token_id,
            crate::hitl::DECISION_RECONNECTED,
            "system",
            &run_owner,
            None,
            None,
            None,
            None,
            "system",
        ) {
            Ok(record) => decided.push(record),
            // Decided meanwhile (a stop, a decline): nothing to resume.
            Err(e) => tracing::info!(request_id, "reauth: park not resolved: {e}"),
        }
    }
    decided
}

/// Resolves the reauth parks on `conn` and resumes their runs.
pub(crate) async fn resume_parked(state: &AppState, conn: &McpConnection) {
    resume_records(state, resolve(&state.store, conn));
}

/// Resolves the parks of one key (a connector connection) and resumes their runs. The caller
/// must not hold a connection lock: the resumed intents take it.
pub(crate) fn resume_key(state: &AppState, key: &str) {
    resume_records(state, resolve_key(&state.store, key));
}

fn resume_records(state: &AppState, records: Vec<crate::error::ApprovalRecord>) {
    for record in records {
        if let Err(e) = state.store.update_run_state(&record.run_id, "RUNNING") {
            tracing::error!(run_id = %record.run_id, "reauth: run not marked running: {e}");
        }
        let manager = state.manager.clone();
        let store = state.store.clone();
        tokio::spawn(async move {
            if let Err(e) =
                super::resume_run_after_approval(manager.clone(), store.clone(), record.clone())
                    .await
            {
                tracing::error!(run_id = %record.run_id, "resume after reconnect failed: {e}");
                let result_json = serde_json::json!({ "error": e.to_string() }).to_string();
                let _ = store.complete_run_with_result(&record.run_id, "FAILED", &result_json);
                let _ = manager.terminate_session(record.session_id.clone()).await;
            }
        });
    }
}
