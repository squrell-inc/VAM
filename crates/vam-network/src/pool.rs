use crate::capability::NetworkCapabilities;
use crate::config::NetworkMode;

/// Result of requesting a TAP lease from the network resource pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapLeaseOutcome {
    /// Isolated or BrokeredEgress — no TAP required on the hot path.
    Skipped,
    /// RestrictedDirect requested but the pool has no pre-provisioned TAP leases.
    NotProvisioned,
}

/// Host-side network resource pool hoisted at daemon startup.
///
/// Default sessions (Isolated / BrokeredEgress) skip kernel TAP/netns setup entirely.
/// RestrictedDirect leases are deferred until a later phase fills the pool.
#[derive(Debug)]
pub struct NetResourcePool {
    capabilities: NetworkCapabilities,
    default_mode: NetworkMode,
    tap_capacity: usize,
}

impl NetResourcePool {
    pub fn new(capabilities: NetworkCapabilities) -> Self {
        Self {
            capabilities,
            default_mode: NetworkMode::BrokeredEgress,
            tap_capacity: 0,
        }
    }

    pub fn with_default_mode(mut self, mode: NetworkMode) -> Self {
        self.default_mode = mode;
        self
    }

    pub fn capabilities(&self) -> &NetworkCapabilities {
        &self.capabilities
    }

    pub fn default_mode(&self) -> NetworkMode {
        self.default_mode
    }

    pub fn tap_capacity(&self) -> usize {
        self.tap_capacity
    }

    /// Request a TAP lease for the given network mode.
    ///
    /// Isolated and BrokeredEgress are no-ops. RestrictedDirect returns
    /// [`TapLeaseOutcome::NotProvisioned`] until the pool is filled in a later phase.
    pub fn lease(&self, mode: NetworkMode) -> TapLeaseOutcome {
        match mode {
            NetworkMode::Isolated | NetworkMode::BrokeredEgress => TapLeaseOutcome::Skipped,
            NetworkMode::RestrictedDirect => TapLeaseOutcome::NotProvisioned,
        }
    }

    pub fn startup_message(&self) -> String {
        format!(
            "Network pool initialized: {} TAP leases (isolated/brokered; virtio-net not attached)",
            self.tap_capacity
        )
    }

    /// Lightweight pool for unit and integration tests (no host probe).
    pub fn for_tests() -> Self {
        Self::new(NetworkCapabilities {
            has_ip_route2: true,
            has_iptables: true,
            has_nftables: false,
            has_tc: true,
            has_dnsmasq: false,
            has_net_admin: false,
            has_unprivileged_netns: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_caps() -> NetworkCapabilities {
        NetworkCapabilities {
            has_ip_route2: true,
            has_iptables: true,
            has_nftables: false,
            has_tc: true,
            has_dnsmasq: false,
            has_net_admin: false,
            has_unprivileged_netns: false,
        }
    }

    #[test]
    fn default_mode_is_brokered_egress_with_zero_tap_capacity() {
        let pool = NetResourcePool::new(test_caps());
        assert_eq!(pool.default_mode(), NetworkMode::BrokeredEgress);
        assert_eq!(pool.tap_capacity(), 0);
    }

    #[test]
    fn lease_skipped_for_isolated_and_brokered() {
        let pool = NetResourcePool::new(test_caps());
        assert_eq!(pool.lease(NetworkMode::Isolated), TapLeaseOutcome::Skipped);
        assert_eq!(
            pool.lease(NetworkMode::BrokeredEgress),
            TapLeaseOutcome::Skipped
        );
    }

    #[test]
    fn lease_not_provisioned_for_restricted_direct() {
        let pool = NetResourcePool::new(test_caps());
        assert_eq!(
            pool.lease(NetworkMode::RestrictedDirect),
            TapLeaseOutcome::NotProvisioned
        );
    }

    #[test]
    fn supports_mode_unchanged_for_default_modes() {
        let caps = test_caps();
        assert!(caps.supports_mode(NetworkMode::Isolated));
        assert!(caps.supports_mode(NetworkMode::BrokeredEgress));
        assert!(!caps.supports_mode(NetworkMode::RestrictedDirect));
    }

    #[test]
    fn startup_message_is_honest_about_tap_capacity() {
        let pool = NetResourcePool::new(test_caps());
        assert!(pool.startup_message().contains("0 TAP leases"));
        assert!(pool.startup_message().contains("virtio-net not attached"));
    }
}
