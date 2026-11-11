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

/// The caller's default URN namespace: the username lowercased, or `default`
/// when that is not a valid URN segment.
pub fn default_namespace(username: &str) -> String {
    let lowered = username.trim().to_lowercase();
    if is_slug(&lowered) {
        lowered
    } else {
        "default".to_string()
    }
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
    }
}
