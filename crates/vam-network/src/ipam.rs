use crate::error::VamNetworkError;
use sha2::{Digest, Sha256};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpamLease {
    pub session_id: String,
    pub subnet_index: u32,
    pub tap_name: String,
    pub mac_address: String,
    pub gateway_ipv4: Ipv4Addr,
    pub guest_ipv4: Ipv4Addr,
    pub guest_ipv6_prefix: String,
    pub created_at: u64,
    pub released_at: Option<u64>,
}

pub fn generate_tap_name(session_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    let hash = hasher.finalize();
    // Prefix "tap_" (4 chars) + first 10 hex characters of SHA256 = 14 characters
    format!("tap_{:.10}", format!("{:x}", hash))
}

pub fn generate_mac_address(session_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    let hash = hasher.finalize();
    // Use locally-administered unicast MAC prefix: 02
    // Followed by 5 deterministic hashed bytes
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        hash[0], hash[1], hash[2], hash[3], hash[4]
    )
}

pub fn calculate_ipv4_addresses(
    subnet_index: u32,
) -> Result<(Ipv4Addr, Ipv4Addr), VamNetworkError> {
    if subnet_index >= 16384 {
        return Err(VamNetworkError::IpamExhausted {
            subnet: "172.20.0.0/16".to_string(),
        });
    }

    let val = subnet_index * 4;
    let b3 = ((val >> 8) & 255) as u8;
    let b4 = (val & 255) as u8;

    let gateway = Ipv4Addr::new(172, 20, b3, b4 + 1);
    let guest = Ipv4Addr::new(172, 20, b3, b4 + 2);

    Ok((gateway, guest))
}

pub fn calculate_ipv6_prefix(subnet_index: u32) -> String {
    format!("fd00:db8:vam:{:x}::/64", subnet_index)
}

pub struct IpamAllocator {
    quarantine_secs: u64,
}

impl IpamAllocator {
    pub fn new(quarantine_secs: u64) -> Self {
        Self { quarantine_secs }
    }

    /// Allocates the next available subnet_index given a list of active leases and current timestamp.
    pub fn allocate(
        &self,
        active_leases: &[IpamLease],
        session_id: &str,
        now_secs: u64,
    ) -> Result<IpamLease, VamNetworkError> {
        // Build a set of unavailable subnet indices
        let mut occupied = vec![false; 16384];

        for lease in active_leases {
            let is_unavailable = match lease.released_at {
                None => true, // Still active
                Some(released_at) => {
                    // In quarantine
                    now_secs < released_at + self.quarantine_secs
                }
            };

            if is_unavailable && (lease.subnet_index as usize) < occupied.len() {
                occupied[lease.subnet_index as usize] = true;
            }
        }

        // Find first free index with position(|&is_occupied| !is_occupied)
        let next_index = occupied
            .iter()
            .position(|&is_occupied| !is_occupied)
            .ok_or_else(|| VamNetworkError::IpamExhausted {
                subnet: "172.20.0.0/16".to_string(),
            })? as u32;

        let (gateway_ipv4, guest_ipv4) = calculate_ipv4_addresses(next_index)?;
        let guest_ipv6_prefix = calculate_ipv6_prefix(next_index);
        let tap_name = generate_tap_name(session_id);
        let mac_address = generate_mac_address(session_id);

        Ok(IpamLease {
            session_id: session_id.to_string(),
            subnet_index: next_index,
            tap_name,
            mac_address,
            gateway_ipv4,
            guest_ipv4,
            guest_ipv6_prefix,
            created_at: now_secs,
            released_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tap_and_mac_generation() {
        let session_id = "test-session-123";
        let tap = generate_tap_name(session_id);
        let mac = generate_mac_address(session_id);

        assert_eq!(tap.len(), 14);
        assert!(tap.starts_with("tap_"));
        assert_eq!(mac.len(), 17);
        assert!(mac.starts_with("02:"));
    }

    #[test]
    fn test_address_calculations() {
        let (gw, guest) = calculate_ipv4_addresses(0).unwrap();
        assert_eq!(gw, Ipv4Addr::new(172, 20, 0, 1));
        assert_eq!(guest, Ipv4Addr::new(172, 20, 0, 2));

        let (gw2, guest2) = calculate_ipv4_addresses(1).unwrap();
        assert_eq!(gw2, Ipv4Addr::new(172, 20, 0, 5));
        assert_eq!(guest2, Ipv4Addr::new(172, 20, 0, 6));

        let (gw_max, guest_max) = calculate_ipv4_addresses(16383).unwrap();
        assert_eq!(gw_max, Ipv4Addr::new(172, 20, 255, 253));
        assert_eq!(guest_max, Ipv4Addr::new(172, 20, 255, 254));

        assert!(calculate_ipv4_addresses(16384).is_err());
    }

    #[test]
    fn test_ipam_allocation_with_quarantine() {
        let allocator = IpamAllocator::new(30);
        let mut leases = vec![];

        // Allocate first
        let lease1 = allocator.allocate(&leases, "sess-1", 100).unwrap();
        assert_eq!(lease1.subnet_index, 0);
        leases.push(lease1);

        // Allocate second
        let lease2 = allocator.allocate(&leases, "sess-2", 100).unwrap();
        assert_eq!(lease2.subnet_index, 1);
        leases.push(lease2);

        // Release first at t=110
        leases[0].released_at = Some(110);

        // Allocate third at t=120 (lease1 is in quarantine until 140)
        let lease3 = allocator.allocate(&leases, "sess-3", 120).unwrap();
        assert_eq!(lease3.subnet_index, 2); // skips 0 because it's quarantined!
        leases.push(lease3);

        // Allocate fourth at t=145 (lease1 quarantine expired!)
        let lease4 = allocator.allocate(&leases, "sess-4", 145).unwrap();
        assert_eq!(lease4.subnet_index, 0); // reuses 0!
    }
}
