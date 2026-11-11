pub mod adapter;
pub mod broker;
pub mod capability;
pub mod config;
pub mod connection;
pub mod dns;
pub mod error;
pub mod gate;
pub mod ipam;
pub mod policy;
pub mod pool;
pub mod proxy;
pub mod secrets;
pub mod tls_ca;
pub mod upstream;

pub use adapter::{KernelNetAdapter, LinuxKernelNetAdapter, MockKernelAdapter};
pub use broker::{
    make_envelope, make_error_envelope, BrokerGate, EgressBrokerClient, EgressBrokerServer,
    EgressDecisionCallback,
};
pub use capability::NetworkCapabilities;
pub use config::{AllowedPortRange, NetworkConfig, NetworkMode, SessionNetworkSpec};
pub use connection::{
    is_read_method, ActiveCall, BindingSpec, CallWindow, ConnectionGrant, ConnectionRefusal,
    ConnectionRequest, ConnectionTokenSource, Envelope, Shape, WriteTicket,
};
pub use dns::SessionDnsProxy;
pub use error::VamNetworkError;
pub use gate::{
    EgressAuthorization, EgressAuthorizer, EgressGate, GateDecision, GateError, GateMode,
    HostResolver, PreparedEgress, SystemResolver, REFUSED_OVERSIZE_WITH_SECRET,
};
pub use ipam::{
    calculate_ipv4_addresses, calculate_ipv6_prefix, generate_mac_address, generate_tap_name,
    IpamAllocator, IpamLease,
};
pub use policy::{
    allow_destination, allow_port, get_protected_deny_set, is_ip_denied, match_domain,
    ActivePolicyContainer, Decision, NetworkPolicyCompiler,
};
pub use pool::{NetResourcePool, TapLeaseOutcome};
pub use secrets::{
    destination_holds_secret, generate_placeholder, inject_secrets, normalize_effective_url,
    phase1_substitute_url_secrets, phase2_inject_secrets, scrub_response, InMemorySecretProvider,
    SecretInjectionPlan, SecretProvider,
};
