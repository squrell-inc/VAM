use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VamNetworkError {
    #[error("namespace creation failed: {0}")]
    NamespaceCreationFailed(String),

    #[error("TAP allocation failed: interface '{name}' already exists")]
    TapInterfaceConflict { name: String },

    #[error("IPAM exhausted: no available IP addresses in subnet {subnet}")]
    IpamExhausted { subnet: String },

    #[error("IP lease conflict: IP {ip} is already assigned to active session")]
    IpLeaseConflict { ip: std::net::Ipv4Addr },

    #[error("nftables/iptables transaction failed: {0}")]
    NftablesTransactionFailed(String),

    #[error("DNS Proxy binding error: {details}")]
    DnsProxyBindingFailed { details: String },

    #[error("Traffic Control (tc) configuration failed: {0}")]
    TcConfigFailed(String),

    #[error("cleanup failure: failed to remove interface or resource '{name}': {details}")]
    CleanupFailed { name: String, details: String },

    #[error("egress forbidden: {reason}")]
    EgressForbidden { reason: String },

    #[error("secret not found in secure vault: {key}")]
    SecretNotFound { key: String },

    #[error("path contains invalid characters (NUL byte or control characters)")]
    InvalidPathCharacters(PathBuf),

    #[error("storage/network database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization/deserialization failed: {0}")]
    Serialization(String),

    #[error("connection closed")]
    ConnectionClosed,

    #[error("internal/unexpected error: {0}")]
    Internal(String),
}
