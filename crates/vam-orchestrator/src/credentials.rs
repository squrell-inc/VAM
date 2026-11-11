//! One credential type for everything that is not an interactive login: an `api_keys` row with a
//! holder. `http` holds user API keys (the bearer path in `keys.rs`); `broker:<id>` holds a grant
//! the broker presents on its own gateway. A secret is good only on the transport its holder
//! names, and HTTP never accepts a grant (`keys.rs::authenticate_bearer`).

use base64::Engine as _;
use rand::RngCore;
use rusqlite::params;

use crate::error::OrchestratorError;
use crate::keys::{hash_api_key_secret, key_cols, ApiKeyRecord, GRANT_TOKEN_PREFIX, HOLDER_HTTP};
use crate::workflow::SqliteWorkflowStore;

/// Prefix of a grant id (`rg_` + 26 base32 characters, from `spaces::mint_id`).
pub const GRANT_ID_PREFIX: &str = "rg_";

/// Random bytes in a grant secret (256 bits), base64url encoded without padding.
const GRANT_SECRET_BYTES: usize = 32;

/// Where a secret arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Http,
    /// The gateway of the broker with this id (the socket the call arrived on).
    Broker(String),
}

impl Transport {
    fn holder(&self) -> String {
        match self {
            Transport::Http => HOLDER_HTTP.to_string(),
            Transport::Broker(id) => format!("broker:{id}"),
        }
    }
}

/// Why a secret was not accepted. Callers answer all of them alike (401 or a gateway denial);
/// the variants let tests and logs tell them apart.
#[derive(Debug)]
pub enum CredentialError {
    Unknown,
    WrongTransport,
    /// Revoked; carries the owner so a gateway can tell "the user was deactivated" apart.
    Revoked {
        owner_user_id: String,
    },
    Expired,
    Storage(OrchestratorError),
}

impl From<CredentialError> for OrchestratorError {
    fn from(e: CredentialError) -> Self {
        match e {
            CredentialError::Storage(e) => e,
            _ => OrchestratorError::Unauthenticated,
        }
    }
}

impl From<OrchestratorError> for CredentialError {
    fn from(e: OrchestratorError) -> Self {
        CredentialError::Storage(e)
    }
}

/// What a good credential names: whose it is, what it points at, what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedCredential {
    pub key_id: String,
    pub owner_user_id: String,
    pub holder: String,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub permissions: Vec<String>,
    pub constraints_json: Option<String>,
    pub external_ref: Option<String>,
    pub source_run_id: Option<String>,
    pub tool_name: Option<String>,
    pub expires_at: Option<i64>,
}

/// Splits `vam_rg_<grant_id>.<secret>` into the id and the secret.
pub fn parse_grant_token(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix(GRANT_TOKEN_PREFIX)?;
    let (id, secret) = rest.split_once('.')?;
    (id.starts_with(GRANT_ID_PREFIX) && !secret.is_empty()).then_some((id, secret))
}

/// Compares two byte strings in time that depends only on their length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The fields of a grant row to insert. `expires_at` is required: a grant never lives forever.
#[derive(Debug, Clone)]
pub struct GrantInsert {
    pub name: String,
    pub owner_user_id: String,
    /// `broker:<id>`.
    pub holder: String,
    pub target_kind: String,
    pub target_id: String,
    pub constraints_json: String,
    pub issued_via: String,
    pub source_run_id: Option<String>,
    pub tool_name: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at: i64,
}

/// A fresh grant secret: 32 bytes from the OS CSPRNG, base64url, no padding. `OsRng` panics
/// rather than return weak bytes, the right failure for a credential.
fn mint_grant_secret() -> String {
    let mut bytes = [0u8; GRANT_SECRET_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

impl SqliteWorkflowStore {
    /// Checks `secret` presented on `transport`. The row is found by the SHA-256 of the secret,
    /// the stored hash is compared again in constant time, and the holder must be the one the
    /// transport names. A non-`http` holder without an expiry is refused (fail closed).
    /// Stamps `last_used_at` on success.
    pub fn check_credential(
        &self,
        secret: &str,
        transport: &Transport,
    ) -> Result<CheckedCredential, CredentialError> {
        // A full grant token carries its id: the secret is the part after the dot.
        let secret = match secret.strip_prefix(GRANT_TOKEN_PREFIX) {
            Some(rest) => rest.split_once('.').map(|(_, s)| s).unwrap_or(""),
            None => secret,
        };
        let hash = hash_api_key_secret(secret);
        let record = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            let sql = format!(
                "SELECT {} FROM api_keys WHERE sha256_secret = ?1",
                key_cols!()
            );
            let found = connection.query_row(&sql, [&hash], crate::keys::parse_api_key_row);
            match found {
                Ok(r) => r,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Err(CredentialError::Unknown),
                Err(e) => return Err(OrchestratorError::Database(e).into()),
            }
        };
        if !constant_time_eq(record.sha256_secret.as_bytes(), hash.as_bytes()) {
            return Err(CredentialError::Unknown);
        }
        if record.holder() != transport.holder() {
            return Err(CredentialError::WrongTransport);
        }
        if record.revoked_at.is_some() {
            return Err(CredentialError::Revoked {
                owner_user_id: record.owner_user_id,
            });
        }
        let now = unix_now();
        match record.expires_at {
            Some(at) if at <= now => return Err(CredentialError::Expired),
            None if !record.is_http() => return Err(CredentialError::Expired),
            _ => {}
        }
        {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            connection
                .execute(
                    "UPDATE api_keys SET last_used_at = ?1 WHERE key_id = ?2",
                    params![now, record.key_id],
                )
                .map_err(OrchestratorError::Database)?;
        }
        Ok(CheckedCredential {
            holder: record.holder().to_string(),
            key_id: record.key_id,
            owner_user_id: record.owner_user_id,
            target_kind: record.target_kind,
            target_id: record.target_id,
            permissions: record.scopes,
            constraints_json: record.constraints_json,
            external_ref: record.external_ref,
            source_run_id: record.source_run_id,
            tool_name: record.tool_name,
            expires_at: record.expires_at,
        })
    }

    /// Checks a full grant token presented on a broker's gateway: it must parse, the secret must
    /// find a row for this broker, and the id in the token must be that row's id (`Unknown`
    /// otherwise, before `last_used_at` is stamped).
    pub fn check_grant_token(
        &self,
        token: &str,
        broker_id: &str,
    ) -> Result<CheckedCredential, CredentialError> {
        let Some((id, secret)) = parse_grant_token(token) else {
            return Err(CredentialError::Unknown);
        };
        // The row is found by the secret's hash; the id is only compared.
        let found = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            connection
                .query_row(
                    "SELECT key_id FROM api_keys WHERE sha256_secret = ?1",
                    [hash_api_key_secret(secret)],
                    |r| r.get::<_, String>(0),
                )
                .ok()
        };
        match found {
            Some(key_id) if constant_time_eq(key_id.as_bytes(), id.as_bytes()) => {}
            _ => return Err(CredentialError::Unknown),
        }
        self.check_credential(token, &Transport::Broker(broker_id.to_string()))
    }

    /// Inserts a grant row and returns it with the token `vam_rg_<grant_id>.<secret>`. Only the
    /// SHA-256 of the secret is stored; the token is shown to the caller once.
    pub fn insert_grant_credential(
        &self,
        g: &GrantInsert,
    ) -> Result<(ApiKeyRecord, String), OrchestratorError> {
        if !g.holder.starts_with("broker:") || g.holder.len() <= "broker:".len() {
            return Err(OrchestratorError::InvalidContract(
                "a grant holder is broker:<id>".to_string(),
            ));
        }
        let key_id = crate::spaces::mint_id(GRANT_ID_PREFIX);
        let secret = mint_grant_secret();
        let hash = hash_api_key_secret(&secret);
        let scopes_json =
            serde_json::to_string(&g.scopes).map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let owner = crate::owner::resolved_owner(&g.owner_user_id).to_string();
        let now = unix_now();
        {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            connection.execute(
                "INSERT INTO api_keys (key_id, name, sha256_secret, principal, scopes, created_at,
                    owner_user_id, holder, target_kind, target_id, constraints_json, issued_via,
                    expires_at, source_run_id, tool_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    key_id,
                    g.name,
                    hash,
                    g.holder,
                    scopes_json,
                    now,
                    owner,
                    g.holder,
                    g.target_kind,
                    g.target_id,
                    g.constraints_json,
                    g.issued_via,
                    g.expires_at,
                    g.source_run_id,
                    g.tool_name,
                ],
            )?;
        }
        let record = self.get_api_key_for_owner(&key_id, &owner)?;
        Ok((record, format!("{GRANT_TOKEN_PREFIX}{key_id}.{secret}")))
    }
}

// ---- action handles (NT-2.4; `design/notifications.md` section 3.6) ----------------------------

/// Prefix of an action handle token: `vam_` + the id (`na_...`) gives `vam_na_<id>.<secret>`.
pub const HANDLE_TOKEN_PREFIX: &str = "vam_";
/// Prefix of an action handle's id.
pub const HANDLE_ID_PREFIX: &str = "na_";
/// `issued_via` of every action handle row. HTTP never accepts one (the holder is `broker:<id>`),
/// and the user's key listings leave them out.
pub const HANDLE_ISSUED_VIA: &str = "notification";
/// Where a handle points: `target_kind`, with `target_id` the approval request id.
const HANDLE_TARGET_KIND: &str = "approval";

/// What a handle is bound to when it is minted. Every field is `vamd`'s own fact.
#[derive(Debug, Clone)]
pub struct HandleMint<'a> {
    /// The channel broker the handle is delivered to; the only socket it works on.
    pub broker_id: &'a str,
    pub notification_id: &'a str,
    /// The approval request the handle decides.
    pub approval_id: &'a str,
    /// The user whose approval it is, and the contact the message goes to.
    pub user_id: &'a str,
    pub contact_id: &'a str,
    pub channel: &'a str,
    /// The decisions the message offered (gate option ids included). The handle decides nothing else.
    pub decisions: &'a [String],
    /// From the broker's approved manifest (`notification_channel.surface`).
    pub surface: &'a str,
    pub expires_at: i64,
}

/// A handle row, read back. The secret is not in it: only its hash is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionHandle {
    pub key_id: String,
    pub broker_id: String,
    pub notification_id: String,
    pub approval_id: String,
    pub user_id: String,
    pub contact_id: String,
    pub channel: String,
    pub decisions: Vec<String>,
    pub surface: String,
    pub expires_at: Option<i64>,
    /// Set when the handle was used, revoked (unlink, restart re-mint, user deactivated) or its
    /// record resolved. A handle in this state decides nothing.
    pub revoked_at: Option<i64>,
}

/// Splits `vam_na_<id>.<secret>` into the id (`na_...`) and the secret.
pub fn parse_handle_token(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix(HANDLE_TOKEN_PREFIX)?;
    let (id, secret) = rest.split_once('.')?;
    (id.starts_with(HANDLE_ID_PREFIX)
        && id.len() <= 64
        && !secret.is_empty()
        && secret.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && secret
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then_some((id, secret))
}

/// Revokes the open handles for `(notification_id, contact_id)` and mints a new one. The secret
/// exists only in the returned token; the row holds its SHA-256. Runs inside the caller's
/// transaction when it has one.
pub fn mint_action_handle(
    conn: &rusqlite::Connection,
    m: &HandleMint<'_>,
) -> Result<String, OrchestratorError> {
    let now = unix_now();
    conn.execute(
        "UPDATE api_keys SET revoked_at = ?3
         WHERE issued_via = ?4 AND revoked_at IS NULL AND external_ref = ?1
           AND json_extract(constraints_json, '$.contact_id') = ?2",
        params![m.notification_id, m.contact_id, now, HANDLE_ISSUED_VIA],
    )?;
    let key_id = crate::spaces::mint_id(HANDLE_ID_PREFIX);
    let secret = mint_grant_secret();
    let hash = hash_api_key_secret(&secret);
    let holder = format!("broker:{}", m.broker_id);
    let scopes_json =
        serde_json::to_string(m.decisions).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let constraints = serde_json::json!({
        "contact_id": m.contact_id,
        "channel": m.channel,
        "surface": m.surface,
    })
    .to_string();
    let owner = crate::owner::resolved_owner(m.user_id).to_string();
    conn.execute(
        "INSERT INTO api_keys (key_id, name, sha256_secret, principal, scopes, created_at,
            owner_user_id, holder, target_kind, target_id, constraints_json, issued_via,
            external_ref, expires_at)
         VALUES (?1, 'notification action handle', ?2, ?3, ?4, ?5, ?6, ?3, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            key_id,
            hash,
            holder,
            scopes_json,
            now,
            owner,
            HANDLE_TARGET_KIND,
            m.approval_id,
            constraints,
            HANDLE_ISSUED_VIA,
            m.notification_id,
            m.expires_at,
        ],
    )?;
    Ok(format!("{HANDLE_TOKEN_PREFIX}{key_id}.{secret}"))
}

/// Why a presented handle found no row. All of them are `INVALID` to the broker.
#[derive(Debug)]
pub enum HandleError {
    /// Does not parse, no row has this secret, the id does not match, the row is not a handle or
    /// belongs to another broker.
    Invalid,
    Storage(OrchestratorError),
}

impl From<rusqlite::Error> for HandleError {
    fn from(e: rusqlite::Error) -> Self {
        HandleError::Storage(OrchestratorError::Database(e))
    }
}

/// Finds the handle `token` names for the broker whose socket the call arrived on. The row is
/// found by the SHA-256 of the secret, the stored hash and the id are compared again in constant
/// time, and the holder must be this broker. Whether it is used, revoked or expired is the
/// caller's to read from the result; this changes nothing.
pub fn find_action_handle(
    conn: &rusqlite::Connection,
    token: &str,
    broker_id: &str,
) -> Result<ActionHandle, HandleError> {
    let Some((id, secret)) = parse_handle_token(token) else {
        return Err(HandleError::Invalid);
    };
    let hash = hash_api_key_secret(secret);
    let sql = format!(
        "SELECT {} FROM api_keys WHERE sha256_secret = ?1 AND issued_via = ?2",
        key_cols!()
    );
    let record = match conn.query_row(&sql, params![hash, HANDLE_ISSUED_VIA], |r| {
        crate::keys::parse_api_key_row(r)
    }) {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Err(HandleError::Invalid),
        Err(e) => return Err(e.into()),
    };
    if !constant_time_eq(record.sha256_secret.as_bytes(), hash.as_bytes())
        || !constant_time_eq(record.key_id.as_bytes(), id.as_bytes())
    {
        return Err(HandleError::Invalid);
    }
    let holder = record.holder().to_string();
    let Some(holder_broker) = holder.strip_prefix("broker:") else {
        return Err(HandleError::Invalid);
    };
    if !constant_time_eq(holder_broker.as_bytes(), broker_id.as_bytes()) {
        return Err(HandleError::Invalid);
    }
    let constraints: serde_json::Value = record
        .constraints_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or(serde_json::Value::Null);
    let text = |k: &str| {
        constraints
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Ok(ActionHandle {
        key_id: record.key_id,
        broker_id: holder_broker.to_string(),
        notification_id: record.external_ref.unwrap_or_default(),
        approval_id: record.target_id.unwrap_or_default(),
        user_id: record.owner_user_id,
        contact_id: text("contact_id"),
        channel: text("channel"),
        decisions: record.scopes,
        surface: text("surface"),
        expires_at: record.expires_at,
        revoked_at: record.revoked_at,
    })
}

/// Stamps `last_used_at` on a handle that decided something (informational; the decision's own
/// transaction already revoked it).
pub fn stamp_handle_used(
    conn: &rusqlite::Connection,
    key_id: &str,
) -> Result<(), OrchestratorError> {
    conn.execute(
        "UPDATE api_keys SET last_used_at = ?2 WHERE key_id = ?1 AND issued_via = ?3",
        params![key_id, unix_now(), HANDLE_ISSUED_VIA],
    )?;
    Ok(())
}

fn revoke_handles_where(
    conn: &rusqlite::Connection,
    clause: &str,
    value: &str,
    now: i64,
) -> Result<usize, OrchestratorError> {
    Ok(conn.execute(
        &format!(
            "UPDATE api_keys SET revoked_at = ?2
             WHERE issued_via = ?3 AND revoked_at IS NULL AND {clause}"
        ),
        params![value, now, HANDLE_ISSUED_VIA],
    )?)
}

/// Unlink: every open handle minted for this contact.
pub fn revoke_handles_for_contact(
    conn: &rusqlite::Connection,
    contact_id: &str,
    now: i64,
) -> Result<usize, OrchestratorError> {
    revoke_handles_where(
        conn,
        "json_extract(constraints_json, '$.contact_id') = ?1",
        contact_id,
        now,
    )
}

/// The record resolved (by anyone, from anywhere): every handle for it is spent. Called in the
/// decision's own transaction, so a handle is used up with the decision it made.
pub fn revoke_handles_for_notification(
    conn: &rusqlite::Connection,
    notification_id: &str,
    now: i64,
) -> Result<usize, OrchestratorError> {
    revoke_handles_where(conn, "external_ref = ?1", notification_id, now)
}

/// The broker's handles are gone from its memory (it restarted or reconnected): revoke them all.
pub fn revoke_handles_for_broker(
    conn: &rusqlite::Connection,
    broker_id: &str,
    now: i64,
) -> Result<usize, OrchestratorError> {
    revoke_handles_where(conn, "holder = ?1", &format!("broker:{broker_id}"), now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{KeyKind, KEYS_ADMIN_SCOPE};
    use crate::users::OPERATOR_USER_ID;

    fn store() -> SqliteWorkflowStore {
        SqliteWorkflowStore::in_memory().expect("store")
    }

    fn user(store: &SqliteWorkflowStore, name: &str) -> String {
        store.create_user(name, name).expect("user").user_id
    }

    fn grant_for(owner: &str, expires_at: i64) -> GrantInsert {
        GrantInsert {
            name: "grant".into(),
            owner_user_id: owner.into(),
            holder: "broker:vam.test".into(),
            target_kind: "space".into(),
            target_id: "sp_x".into(),
            constraints_json: r#"{"space_id":"sp_x","message":"ping the team\nsecond line"}"#
                .into(),
            issued_via: "invoke".into(),
            source_run_id: Some("run-1".into()),
            tool_name: Some("echo.arm".into()),
            scopes: vec!["runs:write".into()],
            expires_at,
        }
    }

    fn live(store: &SqliteWorkflowStore, owner: &str) -> (ApiKeyRecord, String) {
        store
            .insert_grant_credential(&grant_for(owner, unix_now() + 3600))
            .expect("grant")
    }

    fn secret_of(token: &str) -> &str {
        token.split_once('.').expect("dot").1
    }

    fn broker() -> Transport {
        Transport::Broker("vam.test".into())
    }

    #[test]
    fn grant_has_the_documented_shape_and_stores_only_a_hash() {
        let s = store();
        let alice = user(&s, "alice");
        let (rec, token) = live(&s, &alice);
        let id_rest = rec.key_id.strip_prefix("rg_").expect("rg_");
        assert_eq!(id_rest.len(), 26);
        assert!(id_rest
            .chars()
            .all(|c| "abcdefghijklmnopqrstuvwxyz234567".contains(c)));
        assert_eq!(
            token,
            format!("vam_rg_{}.{}", rec.key_id, secret_of(&token))
        );
        let secret = secret_of(&token);
        assert_eq!(secret.len(), 43, "32 bytes base64url, no padding");
        assert_ne!(rec.sha256_secret, secret);
        assert_eq!(rec.sha256_secret, hash_api_key_secret(secret));
        assert!(!format!("{rec:?}").contains(secret));
        let (_, other) = live(&s, &alice);
        assert_ne!(secret, secret_of(&other));
    }

    #[test]
    fn grant_token_on_http_is_401_full_and_bare() {
        let s = store();
        let alice = user(&s, "alice");
        let (rec, token) = live(&s, &alice);
        for presented in [token.as_str(), secret_of(&token)] {
            let err = s.authenticate_bearer(presented).expect_err("refused");
            assert!(matches!(err, OrchestratorError::Unauthenticated), "{err:?}");
        }
        // Not even a stamp on the row: HTTP never touched it.
        assert!(s
            .get_api_key_for_owner(&rec.key_id, &alice)
            .unwrap()
            .last_used_at
            .is_none());
        // A revoked grant is still 401, not a 403 that confirms the secret.
        s.revoke_api_key_for_owner(&rec.key_id, &alice).unwrap();
        let err = s
            .authenticate_bearer(secret_of(&token))
            .expect_err("refused");
        assert!(matches!(err, OrchestratorError::Unauthenticated));
    }

    #[test]
    fn grant_checks_on_its_own_broker_only() {
        let s = store();
        let alice = user(&s, "alice");
        let (rec, token) = live(&s, &alice);
        let ok = s.check_credential(&token, &broker()).expect("good");
        assert_eq!(ok.key_id, rec.key_id);
        assert_eq!(ok.owner_user_id, alice);
        assert_eq!(ok.holder, "broker:vam.test");
        assert_eq!(ok.target_kind.as_deref(), Some("space"));
        assert_eq!(ok.target_id.as_deref(), Some("sp_x"));
        assert_eq!(ok.permissions, vec!["runs:write".to_string()]);
        assert!(ok.constraints_json.unwrap().contains("ping the team"));
        // The bare secret works the same on the right transport.
        s.check_credential(secret_of(&token), &broker())
            .expect("bare");
        assert!(matches!(
            s.check_credential(&token, &Transport::Broker("other".into())),
            Err(CredentialError::WrongTransport)
        ));
        assert!(matches!(
            s.check_credential(&token, &Transport::Http),
            Err(CredentialError::WrongTransport)
        ));
        assert!(matches!(
            s.check_credential("vam_rg_rg_nope.nope", &broker()),
            Err(CredentialError::Unknown)
        ));
        assert!(s
            .get_api_key_for_owner(&rec.key_id, &alice)
            .unwrap()
            .last_used_at
            .is_some());
    }

    #[test]
    fn expired_missing_expiry_and_revoked_are_refused() {
        let s = store();
        let alice = user(&s, "alice");
        let (_, expired) = s
            .insert_grant_credential(&grant_for(&alice, unix_now() - 1))
            .unwrap();
        assert!(matches!(
            s.check_credential(&expired, &broker()),
            Err(CredentialError::Expired)
        ));
        let (rec, no_expiry) = live(&s, &alice);
        s.connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE api_keys SET expires_at = NULL WHERE key_id = ?1",
                [&rec.key_id],
            )
            .unwrap();
        assert!(matches!(
            s.check_credential(&no_expiry, &broker()),
            Err(CredentialError::Expired)
        ));
        let (rec2, token) = live(&s, &alice);
        s.revoke_api_key_for_owner(&rec2.key_id, &alice).unwrap();
        assert!(matches!(
            s.check_credential(&token, &broker()),
            Err(CredentialError::Revoked { .. })
        ));
    }

    #[test]
    fn a_holder_must_be_a_broker() {
        let s = store();
        let mut g = grant_for("local", unix_now() + 60);
        g.holder = "http".into();
        assert!(s.insert_grant_credential(&g).is_err());
        g.holder = "broker:".into();
        assert!(s.insert_grant_credential(&g).is_err());
    }

    #[test]
    fn existing_api_keys_are_unaffected() {
        let s = store();
        let alice = user(&s, "alice");
        let scopes = vec!["runs:write".to_string()];
        let secret = crate::keys::mint_api_key_secret();
        s.create_api_key_owned("key-1", "k", &secret, "User::a", &scopes, &alice)
            .unwrap();
        // NULL columns read as holder http.
        let rec = s.get_api_key("key-1").unwrap();
        assert_eq!(rec.holder, None);
        assert_eq!(rec.holder(), "http");
        assert!(rec.is_http());
        let auth = s.authenticate_bearer(&secret).expect("http key works");
        assert_eq!(auth.record.owner_user_id, alice);
        let ok = s.check_credential(&secret, &Transport::Http).expect("http");
        assert_eq!(ok.holder, "http");
        assert!(matches!(
            s.check_credential(&secret, &broker()),
            Err(CredentialError::WrongTransport)
        ));
        // The default listing is today's list; grants do not enter it.
        live(&s, &alice);
        let listed = s.list_api_keys_for_owner(&alice).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key_id, "key-1");
        let all = s.list_keys_of_kind(&alice, false, KeyKind::All).unwrap();
        assert_eq!(all.len(), 2);
        let grants = s.list_keys_of_kind(&alice, false, KeyKind::Grants).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].issued_via.as_deref(), Some("invoke"));
        assert_eq!(grants[0].tool_name.as_deref(), Some("echo.arm"));
    }

    #[test]
    fn old_tables_gain_the_columns_without_losing_rows() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE api_keys (key_id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL,
               sha256_secret TEXT NOT NULL UNIQUE, principal TEXT NOT NULL, scopes TEXT NOT NULL,
               created_at INTEGER NOT NULL, last_used_at INTEGER, revoked_at INTEGER,
               owner_user_id TEXT);
             INSERT INTO api_keys (key_id, name, sha256_secret, principal, scopes, created_at, owner_user_id)
               VALUES ('key-old', 'old', 'abc', 'p', '[]', 1, 'local');",
        )
        .unwrap();
        let s = SqliteWorkflowStore::initialize(conn).expect("migrates");
        s.initialize_api_keys_schema().expect("idempotent");
        let rec = s.get_api_key("key-old").unwrap();
        assert!(rec.is_http());
        assert!(rec.expires_at.is_none());
    }

    #[test]
    fn grants_are_owner_scoped_for_every_role() {
        let s = store();
        let alice = user(&s, "alice");
        let bob = user(&s, "bob");
        let (rec, token) = live(&s, &alice);
        // A foreign owner cannot list, get or revoke it; same NotFound as a missing id.
        assert!(s
            .list_keys_of_kind(&bob, false, KeyKind::All)
            .unwrap()
            .is_empty());
        let foreign = s.revoke_api_key_for_owner(&rec.key_id, &bob).unwrap_err();
        let missing = s.revoke_api_key_for_owner("rg_missing", &bob).unwrap_err();
        assert!(matches!(foreign, OrchestratorError::NotFound(_)));
        assert_eq!(
            foreign.to_string().replace(&rec.key_id, "X"),
            missing.to_string().replace("rg_missing", "X")
        );
        assert!(s.get_api_key_for_owner(&rec.key_id, &bob).is_err());
        // keys:admin (the operator) neither lists nor reaches it, in any listing or revoke.
        for kind in [KeyKind::Http, KeyKind::Grants, KeyKind::All] {
            let seen = s.list_keys_of_kind(OPERATOR_USER_ID, true, kind).unwrap();
            assert!(seen.iter().all(|k| k.key_id != rec.key_id), "{kind:?}");
        }
        assert!(matches!(
            s.revoke_api_key_as(&rec.key_id, OPERATOR_USER_ID, true),
            Err(OrchestratorError::NotFound(_))
        ));
        assert!(s.revoke_api_key(&rec.key_id).is_err());
        assert!(s.get_api_key(&rec.key_id).is_err());
        s.check_credential(&token, &broker()).expect("still live");
        // The owner lists and revokes it.
        assert_eq!(
            s.list_keys_of_kind(&alice, false, KeyKind::Grants)
                .unwrap()
                .len(),
            1
        );
        let revoked = s.revoke_api_key_as(&rec.key_id, &alice, false).unwrap();
        assert!(revoked.revoked_at.is_some());
        assert!(matches!(
            s.check_credential(&token, &broker()),
            Err(CredentialError::Revoked { .. })
        ));
    }

    #[test]
    fn keys_admin_keeps_its_reach_over_http_keys_and_own_grants() {
        let s = store();
        let alice = user(&s, "alice");
        let scopes = vec![KEYS_ADMIN_SCOPE.to_string()];
        let secret = crate::keys::mint_api_key_secret();
        s.create_api_key_owned("key-a", "a", &secret, "User::a", &scopes, &alice)
            .unwrap();
        let admin_view = s
            .list_keys_of_kind(OPERATOR_USER_ID, true, KeyKind::Http)
            .unwrap();
        assert!(admin_view.iter().any(|k| k.key_id == "key-a"));
        s.revoke_api_key_as("key-a", OPERATOR_USER_ID, true)
            .expect("admin revokes http key");
        // The operator's own grant is the operator's to list and revoke.
        let (own, _) = live(&s, OPERATOR_USER_ID);
        let mine = s
            .list_keys_of_kind(OPERATOR_USER_ID, true, KeyKind::Grants)
            .unwrap();
        assert_eq!(mine.len(), 1);
        s.revoke_api_key_as(&own.key_id, OPERATOR_USER_ID, true)
            .expect("own grant");
    }

    #[test]
    fn deactivating_a_user_revokes_their_grants() {
        let s = store();
        let alice = user(&s, "alice");
        let bob = user(&s, "bob");
        let (rec, token) = live(&s, &alice);
        let (_, bobs) = live(&s, &bob);
        s.check_credential(&token, &broker()).expect("live before");
        s.deactivate_user(&alice).expect("deactivate");
        assert!(s
            .get_api_key_for_owner(&rec.key_id, &alice)
            .unwrap()
            .revoked_at
            .is_some());
        assert!(matches!(
            s.check_credential(&token, &broker()),
            Err(CredentialError::Revoked { .. })
        ));
        s.check_credential(&bobs, &broker())
            .expect("another user's grant is untouched");
        // There is no reactivation path (`users.rs` has none), so a revoked grant cannot return
        // through one; the row stays revoked for good.
        s.deactivate_user(&alice).expect("idempotent");
        assert!(matches!(
            s.check_credential(&token, &broker()),
            Err(CredentialError::Revoked { .. })
        ));
    }
}
