//! Contacts: the verified address a user has for a channel (`design/notifications.md` section 9.5).
//!
//! An address is a verified contact, never a login field. A user starts linking (`start_contact_link`)
//! and gets a ten-minute, single-use code bound to them. The code is stored **hashed**; the plain
//! code is returned once, to the caller, and nowhere else. The channel adapter proves the code came
//! from the account that holds it (`verify_contact_with_code`, called by the broker's
//! `LinkChannelAccount` in wave 3). Records and receipts keep the user id, never the address.
//!
//! Only an enabled adapter (`node_notify`) can be linked. A channel account is verified for at most
//! one user. Unlink revokes the contact and calls [`revoke_open_handles_for_contact`], which
//! revokes every open action handle minted for it.

use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension, Row};
use sha2::{Digest, Sha256};

use crate::error::OrchestratorError;
use crate::node_notify::CHANNELS;
use crate::spaces::mint_id;
use crate::workflow::SqliteWorkflowStore;

/// How long a link code lives.
pub const CODE_TTL_SECS: i64 = 600;
/// Wrong tries on one contact, and wrong tries from one account per window, before it is refused.
pub const MAX_ATTEMPTS: i64 = 5;
const MAX_ADDRESS_LEN: usize = 128;

/// What `verify_contact_with_code` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The account is now this user's verified contact.
    Verified { contact_id: String, user_id: String },
    /// No pending code matches.
    Invalid,
    /// The code was right but is older than ten minutes.
    Expired,
    /// The code was used already, or its contact was unlinked.
    AlreadyUsed,
    /// The code was issued for a different account than the one that presented it.
    WrongAccount,
    /// The account is verified for another user.
    AccountTaken,
    /// Too many wrong tries; wait for the window to pass or start a new link.
    TooManyAttempts,
}

impl LinkOutcome {
    pub fn is_verified(&self) -> bool {
        matches!(self, LinkOutcome::Verified { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub user_id: String,
    pub channel: String,
    /// Empty until verified, unless the user named the account when starting.
    pub address: String,
    pub display: String,
    pub created_at: i64,
    pub code_expires_at: Option<i64>,
    pub verified_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

impl Contact {
    /// `pending` (waiting for the code), `verified`, or `revoked`.
    pub fn state(&self) -> &'static str {
        if self.revoked_at.is_some() {
            "revoked"
        } else if self.verified_at.is_some() {
            "verified"
        } else {
            "pending"
        }
    }

    /// The API view. Never the code, its hash or the attempt counter.
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "id": self.id,
            "channel": self.channel,
            "state": self.state(),
            "display": self.display,
            "createdAt": self.created_at,
        });
        if let Some(t) = self.verified_at {
            v["verifiedAt"] = t.into();
        }
        if self.verified_at.is_none() && self.revoked_at.is_none() {
            if let Some(t) = self.code_expires_at {
                v["expiresAt"] = t.into();
            }
        }
        if let Some(t) = self.revoked_at {
            v["revokedAt"] = t.into();
        }
        v
    }
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<Contact> {
    Ok(Contact {
        id: r.get(0)?,
        user_id: r.get(1)?,
        channel: r.get(2)?,
        address: r.get(3)?,
        display: r.get(4)?,
        created_at: r.get(5)?,
        code_expires_at: r.get(6)?,
        verified_at: r.get(7)?,
        revoked_at: r.get(8)?,
    })
}

const COLS: &str =
    "id, user_id, channel, address, display, created_at, code_expires_at, verified_at, revoked_at";

/// The stored form of a code. Salted with the channel so one table row never reads as a bare code.
fn hash_code(channel: &str, code: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"vam.contact.code.v1:");
    h.update(channel.as_bytes());
    h.update(b":");
    h.update(normalize_code(code).as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Crockford base32: no I, L, O or U.
pub const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Symbols in a code: 10 x 5 bits = 50 bits.
pub const CODE_SYMBOLS: usize = 10;

/// What is hashed: upper case, `-` and spaces dropped, Crockford's look-alikes folded (I, L to 1;
/// O to 0).
fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            u => u,
        })
        .collect()
}

/// Ten Crockford base32 symbols from the OS generator, shown as `XXXXX-XXXXX`.
fn new_code() -> String {
    let mut out = String::with_capacity(CODE_SYMBOLS + 1);
    for i in 0..CODE_SYMBOLS {
        if i == CODE_SYMBOLS / 2 {
            out.push('-');
        }
        let idx = rand::rngs::OsRng.gen_range(0..CODE_ALPHABET.len());
        out.push(CODE_ALPHABET[idx] as char);
    }
    out
}

fn invalid(msg: impl Into<String>) -> OrchestratorError {
    OrchestratorError::InvalidContract(msg.into())
}

/// An account as a channel names it (`<team>/<user>` for Slack): printable, no spaces.
pub fn valid_account(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ADDRESS_LEN && s.chars().all(|c| c.is_ascii_graphic())
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS contacts (
            id TEXT PRIMARY KEY NOT NULL,
            user_id TEXT NOT NULL,
            channel TEXT NOT NULL,
            address TEXT NOT NULL DEFAULT '',
            display TEXT NOT NULL DEFAULT '',
            code_hash TEXT,
            code_expires_at INTEGER,
            attempts INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            verified_at INTEGER,
            revoked_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS contacts_by_user ON contacts (user_id, channel);
        CREATE UNIQUE INDEX IF NOT EXISTS contacts_one_verified_account
            ON contacts (channel, address)
            WHERE verified_at IS NOT NULL AND revoked_at IS NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS contacts_one_pending_code
            ON contacts (code_hash)
            WHERE verified_at IS NULL AND revoked_at IS NULL AND code_hash IS NOT NULL;
        CREATE TABLE IF NOT EXISTS contact_link_failures (
            channel TEXT NOT NULL,
            account TEXT NOT NULL,
            window_start INTEGER NOT NULL,
            tries INTEGER NOT NULL,
            PRIMARY KEY (channel, account)
        );
        ",
    )?;
    Ok(())
}

/// Revokes every open action handle minted for this contact (`credentials::revoke_handles_for_contact`),
/// so an unlinked account cannot decide anything from a message it already holds. Called inside
/// the unlink transaction.
pub fn revoke_open_handles_for_contact(
    connection: &Connection,
    contact_id: &str,
) -> Result<(), OrchestratorError> {
    crate::credentials::revoke_handles_for_contact(connection, contact_id, now_secs())?;
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl SqliteWorkflowStore {
    /// Start linking `channel` for `user`. Returns the pending contact and the plain code, which
    /// exists only in this return value. `address` is the account the user says they will link
    /// from (optional); if given, only that account can verify. An earlier pending link of the
    /// same user and channel is replaced. The caller checked that the adapter is enabled
    /// (`notify_channel_enabled`); this checks it again.
    pub fn start_contact_link(
        &self,
        user_id: &str,
        channel: &str,
        address: Option<&str>,
    ) -> Result<(Contact, String), OrchestratorError> {
        self.start_contact_link_at(user_id, channel, address, now_secs())
    }

    pub fn start_contact_link_at(
        &self,
        user_id: &str,
        channel: &str,
        address: Option<&str>,
        now: i64,
    ) -> Result<(Contact, String), OrchestratorError> {
        if !CHANNELS.contains(&channel) {
            return Err(invalid(format!(
                "channel `{channel}` is not one of: {}",
                CHANNELS.join(", ")
            )));
        }
        let address = address.map(str::trim).filter(|a| !a.is_empty());
        if let Some(a) = address {
            if !valid_account(a) {
                return Err(invalid("address is not a channel account"));
            }
        }
        if !self.notify_channel_enabled(channel)? {
            return Err(OrchestratorError::Conflict(format!(
                "the {channel} adapter is not enabled on this node (ask the operator: vam node notify)"
            )));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.unchecked_transaction()?;
        let verified: i64 = tx.query_row(
            "SELECT COUNT(*) FROM contacts
             WHERE user_id = ?1 AND channel = ?2 AND verified_at IS NOT NULL AND revoked_at IS NULL",
            params![user_id, channel],
            |r| r.get(0),
        )?;
        if verified > 0 {
            return Err(OrchestratorError::Conflict(format!(
                "a {channel} contact is already linked; unlink it first"
            )));
        }
        tx.execute(
            "UPDATE contacts SET revoked_at = ?3
             WHERE user_id = ?1 AND channel = ?2 AND verified_at IS NULL AND revoked_at IS NULL",
            params![user_id, channel, now],
        )?;
        let id = mint_id("ct_");
        let mut tries = 0;
        let code = loop {
            let code = new_code();
            let hash = hash_code(channel, &code);
            let taken: i64 = tx.query_row(
                "SELECT COUNT(*) FROM contacts
                 WHERE code_hash = ?1 AND verified_at IS NULL AND revoked_at IS NULL",
                params![hash],
                |r| r.get(0),
            )?;
            if taken == 0 {
                tx.execute(
                    "INSERT INTO contacts (id, user_id, channel, address, display, code_hash,
                                           code_expires_at, attempts, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, 0, ?7)",
                    params![
                        id,
                        user_id,
                        channel,
                        address.unwrap_or(""),
                        hash,
                        now + CODE_TTL_SECS,
                        now
                    ],
                )?;
                break code;
            }
            tries += 1;
            if tries > 16 {
                return Err(OrchestratorError::Core("could not mint a link code".into()));
            }
        };
        let contact = tx.query_row(
            &format!("SELECT {COLS} FROM contacts WHERE id = ?1"),
            params![id],
            from_row,
        )?;
        tx.commit()?;
        Ok((contact, code))
    }

    /// The user's contacts that are not unlinked: pending and verified. Never a code.
    pub fn list_contacts(&self, user_id: &str) -> Result<Vec<Contact>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(&format!(
            "SELECT {COLS} FROM contacts WHERE user_id = ?1 AND revoked_at IS NULL
             ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map(params![user_id], from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Unlink: revoke the user's own contact. A foreign or unknown id is "not found". Open handles
    /// minted for the contact are revoked in the same transaction ([`revoke_open_handles_for_contact`]).
    pub fn unlink_contact(
        &self,
        user_id: &str,
        contact_id: &str,
    ) -> Result<Contact, OrchestratorError> {
        self.unlink_contact_at(user_id, contact_id, now_secs())
    }

    pub fn unlink_contact_at(
        &self,
        user_id: &str,
        contact_id: &str,
        now: i64,
    ) -> Result<Contact, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.unchecked_transaction()?;
        let found = tx
            .query_row(
                &format!(
                    "SELECT {COLS} FROM contacts
                     WHERE id = ?1 AND user_id = ?2 AND revoked_at IS NULL"
                ),
                params![contact_id, user_id],
                from_row,
            )
            .optional()?;
        let Some(mut contact) = found else {
            return Err(OrchestratorError::NotFound("contact not found".into()));
        };
        tx.execute(
            "UPDATE contacts SET revoked_at = ?2 WHERE id = ?1",
            params![contact_id, now],
        )?;
        revoke_open_handles_for_contact(&tx, contact_id)?;
        tx.commit()?;
        contact.revoked_at = Some(now);
        Ok(contact)
    }

    /// The user's verified contact on a channel (the delivery waves read the address from it).
    pub fn verified_contact(
        &self,
        user_id: &str,
        channel: &str,
    ) -> Result<Option<Contact>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection
            .query_row(
                &format!(
                    "SELECT {COLS} FROM contacts
                     WHERE user_id = ?1 AND channel = ?2
                       AND verified_at IS NOT NULL AND revoked_at IS NULL"
                ),
                params![user_id, channel],
                from_row,
            )
            .optional()?)
    }

    /// The user a verified account belongs to (a click or a message names an account).
    pub fn user_of_verified_account(
        &self,
        channel: &str,
        account: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection
            .query_row(
                "SELECT user_id FROM contacts
                 WHERE channel = ?1 AND address = ?2
                   AND verified_at IS NOT NULL AND revoked_at IS NULL",
                params![channel, account],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// What the broker's `LinkChannelAccount` calls: the account `external_account` presented
    /// `code`. A pending code that is in time, unused and issued for this account (or for no
    /// account in particular) verifies it for the user the code is bound to.
    pub fn verify_contact_with_code(
        &self,
        code: &str,
        external_account: &str,
    ) -> Result<LinkOutcome, OrchestratorError> {
        self.verify_contact_with_code_at(code, external_account, now_secs())
    }

    pub fn verify_contact_with_code_at(
        &self,
        code: &str,
        external_account: &str,
        now: i64,
    ) -> Result<LinkOutcome, OrchestratorError> {
        let account = external_account.trim();
        if !valid_account(account) {
            return Err(invalid("external_account is not a channel account"));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.unchecked_transaction()?;
        // Codes are channel-salted; the channel is known only from the row, so try each.
        #[allow(clippy::type_complexity)] // one contacts row, read once and destructured below
        let mut hit: Option<(
            String,
            String,
            String,
            Option<i64>,
            i64,
            Option<i64>,
            Option<i64>,
            String,
        )> = None;
        let mut channel_tried = CHANNELS[0].to_string();
        for channel in CHANNELS {
            channel_tried = (*channel).to_string();
            hit = tx
                .query_row(
                    "SELECT id, user_id, address, code_expires_at, attempts, verified_at, revoked_at, channel
                     FROM contacts WHERE code_hash = ?1 AND channel = ?2
                     ORDER BY (verified_at IS NULL AND revoked_at IS NULL) DESC, created_at DESC
                     LIMIT 1",
                    params![hash_code(channel, code), channel],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                        ))
                    },
                )
                .optional()?;
            if hit.is_some() {
                break;
            }
        }
        let channel = hit.as_ref().map(|h| h.7.clone()).unwrap_or(channel_tried);
        // An account that keeps guessing is refused before the code is looked at.
        let (window_start, tries): (i64, i64) = tx
            .query_row(
                "SELECT window_start, tries FROM contact_link_failures
                 WHERE channel = ?1 AND account = ?2",
                params![channel, account],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .filter(|(start, _)| now - start < CODE_TTL_SECS)
            .unwrap_or((now, 0));
        if tries >= MAX_ATTEMPTS {
            tx.commit()?;
            return Ok(LinkOutcome::TooManyAttempts);
        }
        let fail = |tx: &rusqlite::Transaction<'_>, outcome: LinkOutcome, contact: Option<&str>| {
            tx.execute(
                "INSERT INTO contact_link_failures (channel, account, window_start, tries)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(channel, account) DO UPDATE SET
                    window_start = excluded.window_start, tries = excluded.tries",
                params![channel, account, window_start, tries + 1],
            )?;
            if let Some(id) = contact {
                tx.execute(
                    "UPDATE contacts SET attempts = attempts + 1 WHERE id = ?1",
                    params![id],
                )?;
            }
            Ok::<_, OrchestratorError>(outcome)
        };
        let Some((id, user_id, address, expires, attempts, verified, revoked, _)) = hit else {
            let out = fail(&tx, LinkOutcome::Invalid, None)?;
            tx.commit()?;
            return Ok(out);
        };
        let out = if verified.is_some() || revoked.is_some() {
            LinkOutcome::AlreadyUsed
        } else if attempts >= MAX_ATTEMPTS {
            LinkOutcome::TooManyAttempts
        } else if expires.is_none_or(|e| now > e) {
            LinkOutcome::Expired
        } else if !address.is_empty() && address != account {
            fail(&tx, LinkOutcome::WrongAccount, Some(&id))?
        } else {
            let other: i64 = tx.query_row(
                "SELECT COUNT(*) FROM contacts
                 WHERE channel = ?1 AND address = ?2 AND user_id <> ?3
                   AND verified_at IS NOT NULL AND revoked_at IS NULL",
                params![channel, account, user_id],
                |r| r.get(0),
            )?;
            if other > 0 {
                fail(&tx, LinkOutcome::AccountTaken, Some(&id))?
            } else {
                tx.execute(
                    "UPDATE contacts SET address = ?2, display = ?2, verified_at = ?3
                     WHERE id = ?1",
                    params![id, account, now],
                )?;
                tx.execute(
                    "DELETE FROM contact_link_failures WHERE channel = ?1 AND account = ?2",
                    params![channel, account],
                )?;
                LinkOutcome::Verified {
                    contact_id: id,
                    user_id,
                }
            }
        };
        tx.commit()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_notify::NodeNotifySettings;

    fn store() -> SqliteWorkflowStore {
        let s = SqliteWorkflowStore::in_memory().unwrap();
        let mut n = NodeNotifySettings::default();
        n.slack.enabled = true;
        n.slack.bot_token_ref = Some("b".into());
        n.slack.app_token_ref = Some("a".into());
        s.put_node_notify_settings(&n).unwrap();
        s
    }

    #[test]
    fn only_an_enabled_adapter_can_be_linked() {
        let s = SqliteWorkflowStore::in_memory().unwrap();
        let e = s.start_contact_link("alice", "slack", None).unwrap_err();
        assert!(matches!(e, OrchestratorError::Conflict(_)), "{e:?}");
        let s = store();
        assert!(s.start_contact_link("alice", "email", None).is_err());
        assert!(s
            .start_contact_link("alice", "slack", Some("has space"))
            .is_err());
        assert!(s.start_contact_link("alice", "slack", None).is_ok());
    }

    #[test]
    fn the_code_is_stored_hashed_only() {
        let s = store();
        let (c, code) = s
            .start_contact_link_at("alice", "slack", None, 1000)
            .unwrap();
        let symbols: Vec<char> = code.chars().filter(|c| *c != '-').collect();
        assert_eq!(symbols.len(), CODE_SYMBOLS);
        assert_eq!(code.len(), CODE_SYMBOLS + 1);
        assert_eq!(code.as_bytes()[5], b'-');
        assert!(symbols.iter().all(|c| CODE_ALPHABET.contains(&(*c as u8))));
        assert_eq!(c.state(), "pending");
        assert_eq!(c.code_expires_at, Some(1000 + CODE_TTL_SECS));
        let conn = s.connection.lock().unwrap();
        let (hash, rows): (String, i64) = conn
            .query_row(
                "SELECT code_hash, (SELECT COUNT(*) FROM contacts WHERE id = ?1) FROM contacts WHERE id = ?1",
                params![c.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, 1);
        assert_ne!(hash, code);
        assert_eq!(hash.len(), 64);
        // No column holds the plain code.
        let dump: String = conn
            .query_row(
                "SELECT id||user_id||channel||address||display||COALESCE(code_hash,'') FROM contacts",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!dump.contains(&code));
        // The API view never carries it.
        assert!(!c.to_json().to_string().contains(&code));
    }

    #[test]
    fn a_code_verifies_once() {
        let s = store();
        let (c, code) = s
            .start_contact_link_at("alice", "slack", None, 1000)
            .unwrap();
        let out = s.verify_contact_with_code_at(&code, "T1/U1", 1100).unwrap();
        assert_eq!(
            out,
            LinkOutcome::Verified {
                contact_id: c.id.clone(),
                user_id: "alice".into()
            }
        );
        assert_eq!(
            s.verified_contact("alice", "slack")
                .unwrap()
                .unwrap()
                .address,
            "T1/U1"
        );
        assert_eq!(
            s.user_of_verified_account("slack", "T1/U1")
                .unwrap()
                .as_deref(),
            Some("alice")
        );
        // Reused.
        assert_eq!(
            s.verify_contact_with_code_at(&code, "T1/U1", 1101).unwrap(),
            LinkOutcome::AlreadyUsed
        );
        assert_eq!(s.list_contacts("alice").unwrap()[0].state(), "verified");
        // Already linked: a second link must start from an unlink.
        assert!(matches!(
            s.start_contact_link_at("alice", "slack", None, 1200),
            Err(OrchestratorError::Conflict(_))
        ));
    }

    #[test]
    fn a_lowercase_dashed_or_spaced_entry_verifies() {
        let s = store();
        let (_, code) = s
            .start_contact_link_at("alice", "slack", None, 1000)
            .unwrap();
        let typed = format!(" {} ", code.to_lowercase().replace('-', " - "));
        assert!(s
            .verify_contact_with_code_at(&typed, "T1/U1", 1001)
            .unwrap()
            .is_verified());
        let (_, other) = s.start_contact_link_at("bob", "slack", None, 1002).unwrap();
        assert!(s
            .verify_contact_with_code_at(&other.replace('-', "").to_lowercase(), "T1/U2", 1003)
            .unwrap()
            .is_verified());
    }

    #[test]
    fn an_old_code_is_expired() {
        let s = store();
        let (_, code) = s
            .start_contact_link_at("alice", "slack", None, 1000)
            .unwrap();
        assert_eq!(
            s.verify_contact_with_code_at(&code, "T1/U1", 1000 + CODE_TTL_SECS + 1)
                .unwrap(),
            LinkOutcome::Expired
        );
        // Right at the limit still works.
        let (_, code) = s.start_contact_link_at("bob", "slack", None, 2000).unwrap();
        assert!(s
            .verify_contact_with_code_at(&code, "T1/U2", 2000 + CODE_TTL_SECS)
            .unwrap()
            .is_verified());
    }

    #[test]
    fn the_wrong_account_or_a_taken_account_does_not_verify() {
        let s = store();
        let (_, code) = s
            .start_contact_link_at("alice", "slack", Some("T1/U1"), 1000)
            .unwrap();
        assert_eq!(
            s.verify_contact_with_code_at(&code, "T1/U9", 1001).unwrap(),
            LinkOutcome::WrongAccount
        );
        assert!(s.verified_contact("alice", "slack").unwrap().is_none());
        assert!(s
            .verify_contact_with_code_at(&code, "T1/U1", 1002)
            .unwrap()
            .is_verified());
        // Bob cannot claim alice's verified account.
        let (_, bob) = s.start_contact_link_at("bob", "slack", None, 1003).unwrap();
        assert_eq!(
            s.verify_contact_with_code_at(&bob, "T1/U1", 1004).unwrap(),
            LinkOutcome::AccountTaken
        );
        assert_eq!(
            s.user_of_verified_account("slack", "T1/U1")
                .unwrap()
                .as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn a_contact_burns_after_too_many_wrong_tries() {
        let s = store();
        let (c, code) = s
            .start_contact_link_at("alice", "slack", Some("T1/U1"), 1000)
            .unwrap();
        for i in 0..MAX_ATTEMPTS {
            // Different accounts, so only the contact's own counter fills.
            let who = format!("T1/X{i}");
            assert_eq!(
                s.verify_contact_with_code_at(&code, &who, 1001).unwrap(),
                LinkOutcome::WrongAccount
            );
        }
        assert_eq!(
            s.verify_contact_with_code_at(&code, "T1/U1", 1002).unwrap(),
            LinkOutcome::TooManyAttempts
        );
        assert!(s.verified_contact("alice", "slack").unwrap().is_none());
        assert_eq!(s.list_contacts("alice").unwrap()[0].id, c.id);
    }

    #[test]
    fn an_account_that_keeps_guessing_is_refused_even_with_a_good_code() {
        let s = store();
        let (_, code) = s
            .start_contact_link_at("alice", "slack", None, 1000)
            .unwrap();
        for _ in 0..MAX_ATTEMPTS {
            assert_eq!(
                s.verify_contact_with_code_at("000000x", "T1/U7", 1001)
                    .unwrap(),
                LinkOutcome::Invalid
            );
        }
        assert_eq!(
            s.verify_contact_with_code_at(&code, "T1/U7", 1002).unwrap(),
            LinkOutcome::TooManyAttempts
        );
        // The window passes (the old code has expired by then); a fresh link works.
        let later = 1001 + CODE_TTL_SECS;
        assert_eq!(
            s.verify_contact_with_code_at(&code, "T1/U7", later)
                .unwrap(),
            LinkOutcome::Expired
        );
        let (_, fresh) = s
            .start_contact_link_at("alice", "slack", None, later)
            .unwrap();
        assert!(s
            .verify_contact_with_code_at(&fresh, "T1/U7", later + 1)
            .unwrap()
            .is_verified());
    }

    #[test]
    fn unlink_revokes_and_a_foreign_id_is_not_found() {
        let s = store();
        let (c, code) = s
            .start_contact_link_at("alice", "slack", None, 1000)
            .unwrap();
        assert!(s
            .verify_contact_with_code_at(&code, "T1/U1", 1001)
            .unwrap()
            .is_verified());
        assert!(matches!(
            s.unlink_contact_at("bob", &c.id, 1002),
            Err(OrchestratorError::NotFound(_))
        ));
        let gone = s.unlink_contact_at("alice", &c.id, 1003).unwrap();
        assert_eq!(gone.state(), "revoked");
        assert!(s.list_contacts("alice").unwrap().is_empty());
        assert!(s.verified_contact("alice", "slack").unwrap().is_none());
        assert!(s
            .user_of_verified_account("slack", "T1/U1")
            .unwrap()
            .is_none());
        // Unlinking twice, and presenting the old code, find nothing to use.
        assert!(matches!(
            s.unlink_contact_at("alice", &c.id, 1004),
            Err(OrchestratorError::NotFound(_))
        ));
        assert_eq!(
            s.verify_contact_with_code_at(&code, "T1/U1", 1005).unwrap(),
            LinkOutcome::AlreadyUsed
        );
        // The account is free for someone else, and alice can link again.
        let (_, bob) = s.start_contact_link_at("bob", "slack", None, 1006).unwrap();
        assert!(s
            .verify_contact_with_code_at(&bob, "T1/U1", 1007)
            .unwrap()
            .is_verified());
        assert!(s
            .start_contact_link_at("alice", "slack", None, 1008)
            .is_ok());
    }

    #[test]
    fn starting_again_replaces_the_pending_link() {
        let s = store();
        let (a, first) = s
            .start_contact_link_at("alice", "slack", None, 1000)
            .unwrap();
        let (b, second) = s
            .start_contact_link_at("alice", "slack", None, 1001)
            .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(s.list_contacts("alice").unwrap().len(), 1);
        if first != second {
            assert_eq!(
                s.verify_contact_with_code_at(&first, "T1/U1", 1002)
                    .unwrap(),
                LinkOutcome::AlreadyUsed
            );
        }
        assert!(s
            .verify_contact_with_code_at(&second, "T1/U1", 1003)
            .unwrap()
            .is_verified());
    }
}
