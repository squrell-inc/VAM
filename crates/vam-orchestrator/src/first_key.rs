//! The node's first admin key (workstream `machine-base`, MB-1.7).
//!
//! The secret reaches exactly one place: the stdout of `vamd --init`, once. It is never given
//! to `tracing`, an error message or a file; the database keeps only its sha256. What is
//! logged is the key id and a hint.

use crate::api::bootstrap_api_key_if_empty;
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// The id of the first key (not a secret; the only part of it that is logged).
pub const FIRST_KEY_ID: &str = "bootstrap";

/// What `vamd --init` did about the key.
#[derive(Debug, PartialEq, Eq)]
pub enum InitKey {
    /// The node already had a key; nothing was minted.
    Existing,
    /// A key was created; the secret is for stdout and nowhere else.
    Created(String),
}

/// `vamd --init`: mint the first key when the node has none. `VAMD_BOOTSTRAP_API_KEY`
/// (given by the operator) is used when set, else a fresh random secret.
pub fn init_first_key(
    store: &SqliteWorkflowStore,
    operator_secret: Option<String>,
) -> Result<InitKey, OrchestratorError> {
    Ok(match bootstrap_api_key_if_empty(store, operator_secret)? {
        Some(secret) => InitKey::Created(secret),
        None => InitKey::Existing,
    })
}

/// Daemon start. With no key and no `VAMD_BOOTSTRAP_API_KEY` it mints nothing and says what
/// to do (a warning, not an error: the node still starts). With the operator's secret it
/// creates the key and logs only its id. Returns whether the node has a key afterwards.
pub fn start_first_key(
    store: &SqliteWorkflowStore,
    operator_secret: Option<String>,
) -> Result<bool, OrchestratorError> {
    store.initialize_api_keys_schema()?;
    if store.api_key_count()? > 0 {
        return Ok(true);
    }
    match operator_secret {
        Some(secret) => {
            bootstrap_api_key_if_empty(store, Some(secret))?;
            tracing::info!(
                key_id = FIRST_KEY_ID,
                "admin key created from VAMD_BOOTSTRAP_API_KEY"
            );
            Ok(true)
        }
        None => {
            tracing::warn!(
                "no API key yet: run `vam node init` (or `vamd --init`) to create the first admin key"
            );
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut v) = self.0.lock() {
                v.extend_from_slice(b);
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    /// Run `f` with every tracing event, at TRACE, captured; return what was written.
    fn captured(f: impl FnOnce()) -> String {
        let cap = Capture::default();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(cap.clone())
            .finish();
        tracing::subscriber::with_default(sub, f);
        let bytes = cap.0.lock().map(|v| v.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn minting_and_starting_never_log_the_secret() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let mut minted = String::new();
        let log = captured(|| {
            if let Ok(InitKey::Created(s)) = init_first_key(&store, None) {
                minted = s;
            }
            // A start with a key present, and a second init.
            assert!(start_first_key(&store, None).unwrap());
            assert_eq!(init_first_key(&store, None).unwrap(), InitKey::Existing);
        });
        assert!(minted.starts_with("vam_sk_"), "a key was minted: {minted}");
        assert!(!log.contains(&minted), "secret in the log: {log}");
    }

    #[test]
    fn a_start_with_the_operators_secret_logs_the_id_not_the_secret() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let secret = "operator-chosen-secret-0123456789";
        let log = captured(|| {
            assert!(start_first_key(&store, Some(secret.into())).unwrap());
        });
        assert!(!log.contains(secret), "secret in the log: {log}");
        assert!(log.contains(FIRST_KEY_ID), "{log}");
        assert_eq!(store.api_key_count().unwrap(), 1);
    }

    #[test]
    fn a_start_with_no_key_mints_nothing_and_warns_what_to_do() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let mut has_key = true;
        let log = captured(|| {
            has_key = start_first_key(&store, None).unwrap();
        });
        assert!(!has_key);
        assert_eq!(store.api_key_count().unwrap(), 0);
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains("vam node init"), "{log}");
        assert!(log.contains("vamd --init"), "{log}");
    }
}
