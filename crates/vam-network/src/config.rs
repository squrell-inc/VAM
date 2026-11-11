use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NetworkMode {
    #[serde(rename = "isolated")]
    #[default]
    Isolated,
    #[serde(rename = "brokered_egress")]
    BrokeredEgress,
    #[serde(rename = "restricted_direct")]
    RestrictedDirect,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllowedPortRange {
    pub from_port: u16,
    pub to_port: u16,
    pub protocol: String, // "tcp", "udp", or "any"
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionNetworkSpec {
    pub session_id: String,
    pub mode: NetworkMode,
    pub max_egress_bps: Option<u64>,
    pub max_ingress_bps: Option<u64>,
    pub max_packet_rate_pps: Option<u64>,
    pub allowed_domains: Vec<String>,
    pub allowed_cidr_destinations: Vec<String>,
    pub allowed_ports: Vec<AllowedPortRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkConfig {
    pub base_ipv4_subnet: String, // e.g. "172.20.0.0/16"
    pub base_ipv6_prefix: String, // e.g. "fd00:db8:vam::/48"
    pub default_mode: NetworkMode,
    pub quarantine_duration_secs: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            base_ipv4_subnet: "172.20.0.0/16".to_string(),
            base_ipv6_prefix: "fd00:db8:vam::/48".to_string(),
            default_mode: NetworkMode::Isolated,
            quarantine_duration_secs: 30,
        }
    }
}
