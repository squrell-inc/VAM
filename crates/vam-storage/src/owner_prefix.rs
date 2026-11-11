//! Host-applied owner prefix for workspace ids and thread refs.
//!
//! Guest-supplied ids are the suffix only. The host prepends
//! `{owner_user_id}--`. A guest cannot set, swap, or strip that prefix.
//! Single-user mode uses the same rule with implicit owner `local`.
//!
//! Run workspace ids also include the agent:
//! `{owner}--{agent_key}--{name}`. `agent_key` is the URN's last segment,
//! slugged, plus `-` and the first 8 hex characters of SHA-256 of the full
//! URN. Legacy `{owner}--{name}` ids stay valid for the agent already bound
//! on that volume.

use crate::error::Drive9Error;
use crate::sha256_hex;

/// Separator between the host owner prefix and the guest suffix.
pub const OWNER_PREFIX_SEP: &str = "--";

/// Implicit single-user owner. Same prefix rule as every other user.
pub const IMPLICIT_OWNER_USER_ID: &str = "local";

/// Apply the host owner prefix to a guest-supplied workspace or thread id.
///
/// Empty `owner_user_id` resolves to [`IMPLICIT_OWNER_USER_ID`]. Path
/// segments and any existing `{owner}--` prefix on the guest value are
/// discarded so the caller cannot choose the prefix.
pub fn apply_owner_prefix(owner_user_id: &str, guest_id: &str) -> Result<String, Drive9Error> {
    let owner = normalize_owner(owner_user_id)?;
    let suffix = guest_id_suffix(guest_id)?;
    Ok(format!("{owner}{OWNER_PREFIX_SEP}{suffix}"))
}

/// Guest suffix after stripping path segments and any host-style prefix.
pub fn guest_id_suffix(guest_id: &str) -> Result<String, Drive9Error> {
    let trimmed = guest_id.trim();
    if trimmed.is_empty() {
        return Err(Drive9Error::Layout(
            "workspace/thread id must not be empty".into(),
        ));
    }
    if trimmed.contains("..") {
        return Err(Drive9Error::Layout(
            "workspace/thread id contains invalid path segments".into(),
        ));
    }
    let last = trimmed
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if last.is_empty() {
        return Err(Drive9Error::Layout(
            "workspace/thread id must not be empty".into(),
        ));
    }
    let suffix = match last.rsplit_once(OWNER_PREFIX_SEP) {
        Some((_, rest)) if !rest.is_empty() => rest,
        _ => last,
    };
    validate_id_segment(suffix, "workspace/thread suffix")?;
    Ok(suffix.to_string())
}

/// Suffix used for seed-dir lookup. Unprefixed ids are returned as-is.
///
/// Agent-scoped ids `{owner}--{agent_key}--{name}` yield `name`, which is
/// the guest workspace name the seed directory is keyed by.
pub fn owner_prefix_suffix(prefixed: &str) -> &str {
    match prefixed.rsplit_once(OWNER_PREFIX_SEP) {
        Some((_, rest)) if !rest.is_empty() => rest,
        _ => prefixed,
    }
}

/// Maximum length of the slugged URN segment inside an agent key.
const AGENT_KEY_SLUG_MAX: usize = 40;

/// Stable path-safe key for an agent URN.
///
/// The last URN segment (after the final `/` or `:`) is slugged: ASCII
/// letters and digits are kept and lowercased, every other character
/// becomes `-`, repeats collapse, and ends are trimmed. The slug is at
/// most 40 characters; an empty slug becomes `agent`. The key is
/// `{slug}-{first 8 hex of SHA-256(trimmed URN)}`.
pub fn agent_workspace_key(agent_urn: &str) -> Result<String, Drive9Error> {
    let urn = agent_urn.trim();
    if urn.is_empty() {
        return Err(Drive9Error::Layout(
            "agent URN is required to derive a workspace id".into(),
        ));
    }
    let segment = urn
        .rsplit(['/', ':'])
        .find(|part| !part.is_empty())
        .unwrap_or(urn);
    let slug = slug_id_segment(segment);
    let digest = sha256_hex(urn.as_bytes());
    let key = format!("{slug}-{}", &digest[..8]);
    validate_id_segment(&key, "agent key")?;
    Ok(key)
}

/// Workspace id `{owner}--{agent_key}--{name}`.
///
/// `name` is the guest suffix only. A name that already contains `--` is
/// rejected so the guest cannot choose the owner or agent prefix.
pub fn apply_agent_workspace_id(
    owner_user_id: &str,
    agent_urn: &str,
    name: &str,
) -> Result<String, Drive9Error> {
    let owner = normalize_owner(owner_user_id)?;
    let name = id_path_segment(name)?;
    if name.contains(OWNER_PREFIX_SEP) {
        return Err(Drive9Error::Layout(
            "workspace name must not contain '--'; vamd assigns the owner and agent".into(),
        ));
    }
    validate_id_segment(&name, "workspace name")?;
    let agent_key = agent_workspace_key(agent_urn)?;
    Ok(format!(
        "{owner}{OWNER_PREFIX_SEP}{agent_key}{OWNER_PREFIX_SEP}{name}"
    ))
}

/// Resolve a caller-supplied workspace id under `owner_user_id`.
///
/// Accepts the caller's own legacy id `{owner}--{name}` and agent-scoped id
/// `{owner}--{agent_key}--{name}` unchanged. An unprefixed name becomes the
/// legacy id. A foreign prefix is stripped and re-applied as this owner's
/// legacy id, so the caller cannot open another user's volume.
pub fn resolve_owned_workspace_id(
    owner_user_id: &str,
    guest_id: &str,
) -> Result<String, Drive9Error> {
    let owner = normalize_owner(owner_user_id)?;
    let segment = id_path_segment(guest_id)?;
    if let Some(owned) = owned_workspace_id(owner, &segment)? {
        return Ok(owned);
    }
    apply_owner_prefix(owner, &segment)
}

fn id_path_segment(guest_id: &str) -> Result<String, Drive9Error> {
    let trimmed = guest_id.trim();
    if trimmed.is_empty() {
        return Err(Drive9Error::Layout(
            "workspace/thread id must not be empty".into(),
        ));
    }
    if trimmed.contains("..") {
        return Err(Drive9Error::Layout(
            "workspace/thread id contains invalid path segments".into(),
        ));
    }
    let last = trimmed
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if last.is_empty() {
        return Err(Drive9Error::Layout(
            "workspace/thread id must not be empty".into(),
        ));
    }
    Ok(last.to_string())
}

/// `Some` when `segment` is already this owner's legacy or agent-scoped id.
fn owned_workspace_id(owner: &str, segment: &str) -> Result<Option<String>, Drive9Error> {
    let parts: Vec<&str> = segment.split(OWNER_PREFIX_SEP).collect();
    match parts.as_slice() {
        [prefix, name] if *prefix == owner => {
            validate_id_segment(name, "workspace name")?;
            Ok(Some(segment.to_string()))
        }
        [prefix, agent_key, name] if *prefix == owner => {
            validate_id_segment(agent_key, "agent key")?;
            validate_id_segment(name, "workspace name")?;
            Ok(Some(segment.to_string()))
        }
        _ => Ok(None),
    }
}

fn slug_id_segment(segment: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in segment.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.len() > AGENT_KEY_SLUG_MAX {
        slug[..AGENT_KEY_SLUG_MAX].trim_end_matches('-')
    } else {
        slug
    };
    if slug.is_empty() {
        "agent".to_string()
    } else {
        slug.to_string()
    }
}

fn normalize_owner(owner_user_id: &str) -> Result<&str, Drive9Error> {
    let owner = owner_user_id.trim();
    let owner = if owner.is_empty() {
        IMPLICIT_OWNER_USER_ID
    } else {
        owner
    };
    validate_id_segment(owner, "owner user id")?;
    Ok(owner)
}

fn validate_id_segment(s: &str, what: &str) -> Result<(), Drive9Error> {
    if s.is_empty() || s == ".." {
        return Err(Drive9Error::Layout(format!("{what} must not be empty")));
    }
    if s.contains('/') || s.contains('\\') || s.contains(OWNER_PREFIX_SEP) {
        return Err(Drive9Error::Layout(format!(
            "{what} '{s}' contains reserved characters"
        )));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(Drive9Error::Layout(format!(
            "{what} '{s}' has invalid characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_guest_suffix_for_each_owner() {
        assert_eq!(
            apply_owner_prefix("alice", "assistant").unwrap(),
            "alice--assistant"
        );
        assert_eq!(
            apply_owner_prefix("bob", "assistant").unwrap(),
            "bob--assistant"
        );
        assert_ne!(
            apply_owner_prefix("alice", "assistant").unwrap(),
            apply_owner_prefix("bob", "assistant").unwrap()
        );
    }

    #[test]
    fn guest_cannot_set_or_strip_host_prefix() {
        assert_eq!(
            apply_owner_prefix("alice", "bob--assistant").unwrap(),
            "alice--assistant"
        );
        assert_eq!(
            apply_owner_prefix("alice", "alice--assistant").unwrap(),
            "alice--assistant"
        );
        assert_eq!(
            apply_owner_prefix("bob", "alice/assistant").unwrap(),
            "bob--assistant"
        );
        assert_eq!(
            apply_owner_prefix("alice", "alice--bob--assistant").unwrap(),
            "alice--assistant"
        );
    }

    #[test]
    fn thread_ref_inherits_the_same_prefix_rule() {
        assert_eq!(apply_owner_prefix("alice", "main").unwrap(), "alice--main");
        assert_eq!(
            apply_owner_prefix("alice", "bob--main").unwrap(),
            "alice--main"
        );
        assert_eq!(
            apply_owner_prefix("local", "sprint-retro").unwrap(),
            "local--sprint-retro"
        );
    }

    #[test]
    fn empty_owner_is_implicit_local() {
        assert_eq!(
            apply_owner_prefix("", "assistant").unwrap(),
            "local--assistant"
        );
        assert_eq!(
            apply_owner_prefix("local", "assistant").unwrap(),
            apply_owner_prefix("", "assistant").unwrap()
        );
    }

    #[test]
    fn seed_lookup_uses_suffix() {
        assert_eq!(owner_prefix_suffix("local--assistant"), "assistant");
        assert_eq!(
            owner_prefix_suffix("alice--sqlite-assistant"),
            "sqlite-assistant"
        );
        assert_eq!(owner_prefix_suffix("assistant"), "assistant");
    }

    #[test]
    fn rejects_empty_and_traversal() {
        assert!(apply_owner_prefix("alice", "").is_err());
        assert!(apply_owner_prefix("alice", "../evil").is_err());
        assert!(apply_owner_prefix("alice/../bob", "assistant").is_err());
    }

    #[test]
    fn agent_key_is_slug_plus_urn_digest() {
        let urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
        assert_eq!(agent_workspace_key(urn).unwrap(), "test-agent-149c38b2");
        let slugged =
            agent_workspace_key("urn:vam:agent:local.vam.dev/acme/Jira Assistant").unwrap();
        assert!(slugged.starts_with("jira-assistant-"), "{slugged}");
        assert_eq!(slugged.len(), "jira-assistant-".len() + 8);
    }

    #[test]
    fn same_slug_different_urn_does_not_share_an_agent_key() {
        let a = agent_workspace_key("urn:vam:agent:local.vam.dev/acme/main").unwrap();
        let b = agent_workspace_key("urn:vam:agent:other.example/org/main").unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("main-"));
        assert!(b.starts_with("main-"));
    }

    #[test]
    fn agent_workspace_id_includes_owner_agent_and_name() {
        let urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
        assert_eq!(
            apply_agent_workspace_id("alice", urn, "main").unwrap(),
            "alice--test-agent-149c38b2--main"
        );
        assert_eq!(
            apply_agent_workspace_id("bob", urn, "main").unwrap(),
            "bob--test-agent-149c38b2--main"
        );
        let other = "urn:vam:agent:local.vam.dev/acme/other-agent";
        assert_ne!(
            apply_agent_workspace_id("alice", urn, "main").unwrap(),
            apply_agent_workspace_id("alice", other, "main").unwrap()
        );
    }

    #[test]
    fn agent_workspace_id_rejects_guest_chosen_prefix() {
        let urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
        let err = apply_agent_workspace_id("alice", urn, "bob--main").unwrap_err();
        assert!(err.to_string().contains("--"), "{err}");
        assert!(apply_agent_workspace_id("alice", urn, "").is_err());
        assert!(apply_agent_workspace_id("alice", "", "main").is_err());
    }

    #[test]
    fn owned_workspace_id_accepts_legacy_and_agent_forms() {
        let urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
        let scoped = apply_agent_workspace_id("alice", urn, "main").unwrap();
        assert_eq!(
            resolve_owned_workspace_id("alice", &scoped).unwrap(),
            scoped
        );
        assert_eq!(
            resolve_owned_workspace_id("alice", "alice--main").unwrap(),
            "alice--main"
        );
        assert_eq!(
            resolve_owned_workspace_id("alice", "main").unwrap(),
            "alice--main"
        );
        assert_eq!(
            resolve_owned_workspace_id("alice", "bob--main").unwrap(),
            "alice--main"
        );
        assert_eq!(
            resolve_owned_workspace_id("alice", "bob--some-key-abcd1234--main").unwrap(),
            "alice--main"
        );
    }

    #[test]
    fn seed_lookup_uses_name_from_agent_scoped_id() {
        assert_eq!(
            owner_prefix_suffix("alice--test-agent-149c38b2--main"),
            "main"
        );
    }
}
