//! Artifact origin from the URN domain.
//!
//! `urn:vam:<kind>:<domain>/<namespace>/<name>`: the domain says where an
//! artifact comes from. A node knows three domains: the fixed local rule
//! (`*.local` and `local.vam.dev`), its configured registry domain (the org),
//! and the marketplace domain (Squrell's, `hub.squrell.cloud` unless a node overrides it).
//! Anything else is `Other`; the domain of a trusted marketplace an admin added is `Other` here
//! too, and its installed rows are told apart by their catalog tier.

use serde::{Deserialize, Serialize};

use crate::validation::{is_local_skill_registry, is_registry, is_slug};

/// The marketplace domain of a node that has not set `VAMD_MARKETPLACE_DOMAIN`: Squrell's.
pub const DEFAULT_MARKETPLACE_DOMAIN: &str = "hub.squrell.cloud";

/// The registry domain of a node that has not configured one.
pub const LOCAL_REGISTRY_DOMAIN: &str = "local.vam.dev";

/// Where an artifact comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// The org's own registry domain.
    Org,
    /// The Squrell marketplace domain.
    Squrell,
    /// A local catalog registry (`*.local`, `local.vam.dev`).
    Local,
    /// Any other domain.
    Other,
}

/// Invalid registry or marketplace domain configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryDomainError {
    #[error(
        "{field} '{value}' is not a valid domain: use lowercase labels of letters, digits, '-' separated by '.'"
    )]
    InvalidDomain { field: &'static str, value: String },
    #[error("registry domain and marketplace domain must differ, both are '{0}'")]
    SameDomain(String),
}

/// The domains a node classifies origin against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryDomains {
    registry: String,
    marketplace: Option<String>,
}

impl Default for RegistryDomains {
    fn default() -> Self {
        Self {
            registry: LOCAL_REGISTRY_DOMAIN.to_string(),
            marketplace: None,
        }
    }
}

impl RegistryDomains {
    /// Builds and validates the domains. A blank registry means
    /// `local.vam.dev`; a blank marketplace means none.
    pub fn new(registry: &str, marketplace: Option<&str>) -> Result<Self, RegistryDomainError> {
        let registry = registry.trim();
        let registry = if registry.is_empty() {
            LOCAL_REGISTRY_DOMAIN
        } else {
            registry
        };
        if !is_registry(registry) {
            return Err(RegistryDomainError::InvalidDomain {
                field: "registry domain",
                value: registry.to_string(),
            });
        }
        let marketplace = marketplace.map(str::trim).filter(|value| !value.is_empty());
        if let Some(value) = marketplace {
            if !is_registry(value) {
                return Err(RegistryDomainError::InvalidDomain {
                    field: "marketplace domain",
                    value: value.to_string(),
                });
            }
            if value == registry {
                return Err(RegistryDomainError::SameDomain(value.to_string()));
            }
        }
        Ok(Self {
            registry: registry.to_string(),
            marketplace: marketplace.map(str::to_string),
        })
    }

    pub fn registry_domain(&self) -> &str {
        &self.registry
    }

    /// Empty when no marketplace domain is configured.
    pub fn marketplace_domain(&self) -> &str {
        self.marketplace.as_deref().unwrap_or("")
    }

    /// Origin of a bare domain.
    pub fn origin_of_domain(&self, domain: &str) -> Origin {
        if is_local_skill_registry(domain) {
            Origin::Local
        } else if self.marketplace.as_deref() == Some(domain) {
            Origin::Squrell
        } else if self.registry == domain {
            Origin::Org
        } else {
            Origin::Other
        }
    }

    /// Origin of an artifact URN (`urn:vam:<kind>:<domain>/...`). A string
    /// that is not an artifact URN is `Other`.
    pub fn origin(&self, urn: &str) -> Origin {
        match urn_domain(urn) {
            Some(domain) => self.origin_of_domain(domain),
            None => Origin::Other,
        }
    }

    /// True when `urn` is under the configured marketplace domain.
    pub fn is_marketplace_urn(&self, urn: &str) -> bool {
        match (&self.marketplace, urn_domain(urn)) {
            (Some(marketplace), Some(domain)) => marketplace == domain,
            _ => false,
        }
    }

    /// A caller may publish a skill under the local registries and the configured
    /// registry domain, never under the marketplace domain or another trusted
    /// marketplace's domain (only the catalog import writes those).
    pub fn admits_skill_registry(&self, registry: &str) -> bool {
        is_local_skill_registry(registry) || registry == self.registry
    }

    /// A skill ref resolves from everything [`Self::admits_skill_registry`] admits, plus the
    /// marketplace domain and each trusted marketplace's `publisherDomain` (`marketplaces`), for
    /// resolving rows a marketplace installed. It never admits a caller's publish. Remote pull
    /// from any other domain is not supported.
    pub fn admits_installed_skill_registry<S: AsRef<str>>(
        &self,
        registry: &str,
        marketplaces: &[S],
    ) -> bool {
        self.admits_skill_registry(registry)
            || self.marketplace.as_deref() == Some(registry)
            || marketplaces.iter().any(|d| d.as_ref() == registry)
    }
}

/// The domain segment of `urn:vam:<kind>:<domain>/...`.
fn urn_domain(urn: &str) -> Option<&str> {
    let rest = urn.strip_prefix("urn:vam:")?;
    let (_, path) = rest.split_once(':')?;
    let (domain, _) = path.split_once('/')?;
    (!domain.is_empty()).then_some(domain)
}

/// The caller's default URN namespace. For an email-shaped username (one `@`) it is the part
/// before the `@`, with any `+tag` dropped, lowercased. Any other username is lowercased as is.
/// `default` when the result is not a valid URN segment. A name with `+` but no `@` is not
/// email-shaped: it is not a slug, so it gives `default`.
pub fn default_namespace(username: &str) -> String {
    let trimmed = username.trim();
    let base = match trimmed.split_once('@') {
        Some((local, _)) => local.split_once('+').map_or(local, |(head, _)| head),
        None => trimmed,
    };
    let lowered = base.to_lowercase();
    if is_slug(&lowered) {
        lowered
    } else {
        "default".to_string()
    }
}

/// Longest namespace base [`namespace_base`] makes, in characters (before any suffix): the
/// username limit, so a slug username is never cut. Nothing limits a URN segment's length.
pub const NAMESPACE_BASE_MAX: usize = 254;

/// Namespaces no new user is given: `default` (where non-slug names used to fall back), the
/// seeded `local` and `operator`, and `bootstrap` (the node's own key).
pub const RESERVED_NAMESPACES: [&str; 4] = ["default", "local", "operator", "bootstrap"];

/// Suffix lengths [`namespace_suffixed`] is tried with, in order.
pub const NAMESPACE_SUFFIX_LENGTHS: [usize; 3] = [6, 10, 16];

/// The readable base of a new user's namespace (user-identity decision 18). For an email-shaped
/// username (one `@`) it starts from the part before the `@` with any `+tag` dropped; otherwise
/// from the whole name. Lowercased; every character outside `[a-z0-9._-]` becomes `-`; leading and
/// trailing `.`, `-` and `_` are trimmed; at most [`NAMESPACE_BASE_MAX`] characters. `user` when
/// nothing is left. Always a valid URN segment, never `default` for a non-slug name.
///
/// For a username that is a slug, this equals [`default_namespace`].
pub fn namespace_base(username: &str) -> String {
    let trimmed = username.trim();
    let base = match trimmed.split_once('@') {
        Some((local, rest)) if !rest.contains('@') => {
            local.split_once('+').map_or(local, |(head, _)| head)
        }
        _ => trimmed,
    };
    let mapped: String = base
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let edge = |c: char| matches!(c, '.' | '_' | '-');
    let cut: String = mapped
        .trim_matches(edge)
        .chars()
        .take(NAMESPACE_BASE_MAX)
        .collect();
    let cut = cut.trim_matches(edge);
    if is_slug(cut) {
        cut.to_string()
    } else {
        "user".to_string()
    }
}

/// `<base>-<suffix>`, the suffix being the first `len` characters (`[a-z2-7]`, lowercase base32)
/// of a SHA-256 of `user_id`. Fixed for the user; it reveals nothing about the id or other users.
/// `len` is clamped to 1..=52 (the whole digest).
pub fn namespace_suffixed(base: &str, user_id: &str, len: usize) -> String {
    use sha2::{Digest, Sha256};
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = Sha256::digest(user_id.as_bytes());
    let mut out = String::with_capacity(52);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in digest {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    let len = len.clamp(1, out.len());
    format!("{base}-{}", &out[..len])
}

/// The namespace segment of a catalog URN `urn:vam:<kind>:<domain>/<namespace>/<name>`.
pub fn urn_namespace(urn: &str) -> Option<&str> {
    let rest = urn.strip_prefix("urn:vam:")?;
    let (_, path) = rest.split_once(':')?;
    let (_, after_domain) = path.split_once('/')?;
    let (namespace, name) = after_domain.split_once('/')?;
    (!namespace.is_empty() && !name.is_empty()).then_some(namespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org() -> RegistryDomains {
        RegistryDomains::new("vam.squrell.dev", Some("hub.example.test")).unwrap()
    }

    #[test]
    fn every_origin_branch() {
        let d = org();
        assert_eq!(d.origin("urn:vam:agent:vam.squrell.dev/ann/a"), Origin::Org);
        assert_eq!(
            d.origin("urn:vam:skill:hub.example.test/x/y"),
            Origin::Squrell
        );
        assert_eq!(
            d.origin("urn:vam:agent:local.vam.dev/default/a"),
            Origin::Local
        );
        assert_eq!(
            d.origin("urn:vam:policy:examples.local/vam/p"),
            Origin::Local
        );
        assert_eq!(d.origin("urn:vam:playbook:vam.local/x/p"), Origin::Local);
        assert_eq!(d.origin("urn:vam:agent:other.dev/x/a"), Origin::Other);
        assert_eq!(d.origin("not-a-urn"), Origin::Other);
    }

    #[test]
    fn unconfigured_node_is_local_not_org() {
        let d = RegistryDomains::default();
        assert_eq!(d.registry_domain(), "local.vam.dev");
        assert_eq!(d.marketplace_domain(), "");
        assert_eq!(
            d.origin("urn:vam:agent:local.vam.dev/default/a"),
            Origin::Local
        );
        assert_eq!(d.origin("urn:vam:agent:vam.squrell.dev/x/a"), Origin::Other);
        assert_eq!(RegistryDomains::new("", None).unwrap(), d);
        assert_eq!(RegistryDomains::new("  ", Some(" ")).unwrap(), d);
    }

    #[test]
    fn unset_marketplace_never_matches() {
        let d = RegistryDomains::new("vam.squrell.dev", None).unwrap();
        assert!(!d.is_marketplace_urn("urn:vam:agent:hub.example.test/x/a"));
        assert_eq!(
            d.origin("urn:vam:agent:hub.example.test/x/a"),
            Origin::Other
        );
    }

    #[test]
    fn domains_are_compared_exactly() {
        let d = org();
        assert_eq!(
            d.origin("urn:vam:agent:sub.vam.squrell.dev/x/a"),
            Origin::Other
        );
        assert!(d.is_marketplace_urn("urn:vam:agent:hub.example.test/x/a"));
        assert!(!d.is_marketplace_urn("urn:vam:agent:x.hub.example.test/x/a"));
    }

    #[test]
    fn invalid_config_is_rejected() {
        for bad in [
            "Vam.Squrell.dev",
            "a/b",
            "a b",
            ".x",
            "x.",
            "a..b",
            "a_b.dev",
        ] {
            assert!(
                matches!(
                    RegistryDomains::new(bad, None),
                    Err(RegistryDomainError::InvalidDomain { .. })
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            RegistryDomains::new("vam.squrell.dev", Some("Hub")),
            Err(RegistryDomainError::InvalidDomain { .. })
        ));
        assert!(matches!(
            RegistryDomains::new("a.dev", Some("a.dev")),
            Err(RegistryDomainError::SameDomain(_))
        ));
    }

    #[test]
    fn skill_registry_admission_follows_domain() {
        let d = org();
        assert!(d.admits_skill_registry("vam.squrell.dev"));
        assert!(d.admits_skill_registry("local.vam.dev"));
        assert!(d.admits_skill_registry("examples.local"));
        assert!(!d.admits_skill_registry("hub.vam.dev"));
        assert!(!d.admits_skill_registry("hub.example.test"));
        assert!(!RegistryDomains::default().admits_skill_registry("vam.squrell.dev"));
    }

    #[test]
    fn installed_skills_resolve_from_marketplace_domains_but_are_never_publishable() {
        let d = org();
        let none: [&str; 0] = [];
        // Publishing: the marketplace domain and a trusted marketplace's domain are refused.
        assert!(!d.admits_skill_registry("hub.example.test"));
        assert!(!d.admits_skill_registry("market.community.example"));
        // Resolving installed rows: the marketplace domain, and each trusted domain.
        assert!(d.admits_installed_skill_registry("hub.example.test", &none));
        assert!(d.admits_installed_skill_registry(
            "market.community.example",
            &["market.community.example"]
        ));
        assert!(!d.admits_installed_skill_registry("market.community.example", &none));
        assert!(!d.admits_installed_skill_registry("other.example", &["market.community.example"]));
        // What was already admitted still is.
        assert!(d.admits_installed_skill_registry("vam.squrell.dev", &none));
        assert!(d.admits_installed_skill_registry("examples.local", &none));
        // No marketplace domain configured and no trusted marketplace: nothing extra.
        assert!(
            !RegistryDomains::default().admits_installed_skill_registry("hub.example.test", &none)
        );
    }

    #[test]
    fn default_namespace_rules() {
        assert_eq!(default_namespace("Alice"), "alice");
        assert_eq!(default_namespace("a.b-c"), "a.b-c");
        assert_eq!(default_namespace(""), "default");
        assert_eq!(default_namespace("bad name"), "default");
        assert_eq!(default_namespace("-x"), "default");
        // Existing-style names are unchanged.
        assert_eq!(default_namespace("bootstrap"), "bootstrap");
        assert_eq!(default_namespace("Ann_Lee"), "ann_lee");
        assert_eq!(default_namespace("usr_abc"), "usr_abc");
        assert_eq!(default_namespace("a-b.c"), "a-b.c");
    }

    #[test]
    fn default_namespace_for_email_usernames() {
        assert_eq!(
            default_namespace("Ada.Lovelace+work@Example.com"),
            "ada.lovelace"
        );
        assert_eq!(default_namespace("bob@example.com"), "bob");
        assert_eq!(default_namespace("+tag@example.com"), "default");
        assert_eq!(default_namespace("@example.com"), "default");
        assert_eq!(default_namespace(".x@example.com"), "default");
        // `+` without `@` is not email-shaped: not a slug, so `default`.
        assert_eq!(default_namespace("ann+work"), "default");
    }

    #[test]
    fn namespace_base_rules() {
        // Every slug name gives today's value.
        for name in ["Alice", "a.b-c", "bootstrap", "Ann_Lee", "usr_abc", "a-b.c"] {
            assert_eq!(namespace_base(name), default_namespace(name), "{name}");
        }
        assert_eq!(
            namespace_base("Ada.Lovelace+work@Example.com"),
            "ada.lovelace"
        );
        assert_eq!(namespace_base("ann.lee@north.test"), "ann.lee");
        assert_eq!(namespace_base("ann.lee@south.test"), "ann.lee");
        // Not a slug as typed: trimmed or mapped, never `default`.
        assert_eq!(namespace_base("_Ann-1a2b@Example.Test"), "ann-1a2b");
        assert_eq!(namespace_base(".Bob-9f.@Example.Test"), "bob-9f");
        assert_eq!(namespace_base("ann+work"), "ann-work");
        assert_eq!(namespace_base("bad name"), "bad-name");
        assert_eq!(namespace_base("-x"), "x");
        assert_eq!(namespace_base("ann.@x"), "ann");
        // Nothing left.
        assert_eq!(namespace_base(""), "user");
        assert_eq!(namespace_base("+tag@example.com"), "user");
        assert_eq!(namespace_base("@example.com"), "user");
        assert_eq!(namespace_base("._-@x"), "user");
        // Two `@` is not email-shaped.
        assert_eq!(namespace_base("a@b@c"), "a-b-c");
        // A long slug is kept whole (the harness's names are long); past the cap the cut never
        // ends on `.`, `-` or `_`.
        let slug = "member-new-user-gets-a-generated-id-it-cannot-choose-1b57b9";
        assert_eq!(namespace_base(slug), slug);
        let long = format!("{}-{}", "a".repeat(NAMESPACE_BASE_MAX - 1), "b".repeat(10));
        assert_eq!(namespace_base(&long), "a".repeat(NAMESPACE_BASE_MAX - 1));
        assert_eq!(namespace_base(&"x".repeat(300)).len(), NAMESPACE_BASE_MAX);
    }

    #[test]
    fn namespace_suffix_is_fixed_per_user_and_base32() {
        let a = namespace_suffixed("ann.lee", "usr_aaaa", 6);
        assert_eq!(a, namespace_suffixed("ann.lee", "usr_aaaa", 6));
        assert_ne!(a, namespace_suffixed("ann.lee", "usr_bbbb", 6));
        let suffix = a.strip_prefix("ann.lee-").unwrap();
        assert_eq!(suffix.len(), 6);
        assert!(suffix
            .bytes()
            .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c)));
        let ten = namespace_suffixed("ann.lee", "usr_aaaa", 10);
        assert!(ten.starts_with(&a));
        assert_eq!(namespace_suffixed("x", "u", 999).len(), 2 + 52);
        assert!(is_slug(&namespace_suffixed("ann.lee", "usr_aaaa", 16)));
    }

    #[test]
    fn urn_namespace_reads_the_segment() {
        assert_eq!(
            urn_namespace("urn:vam:agent:local.vam.dev/ann.lee/one"),
            Some("ann.lee")
        );
        assert_eq!(urn_namespace("urn:vam:skill:x/acme/a/b"), Some("acme"));
        assert_eq!(urn_namespace("urn:vam:agent:local.vam.dev/ann"), None);
        assert_eq!(urn_namespace("urn:vam:agent:local.vam.dev//x"), None);
        assert_eq!(urn_namespace("not-a-urn"), None);
    }
}
