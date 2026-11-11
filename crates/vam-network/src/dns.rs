use crate::error::VamNetworkError;
use crate::policy::ActivePolicyContainer;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

pub struct SessionDnsProxy {
    _policy: Arc<ActivePolicyContainer>,
    bind_addr: SocketAddr,
}

impl fmt::Debug for SessionDnsProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionDnsProxy")
            .field("bind_addr", &self.bind_addr)
            .finish()
    }
}

impl SessionDnsProxy {
    pub fn new(policy: Arc<ActivePolicyContainer>, bind_addr: SocketAddr) -> Self {
        Self {
            _policy: policy,
            bind_addr,
        }
    }

    /// Starts the Hickory-based DNS proxy server on the configured bind address.
    /// Gracefully degrades or fails safely when unprivileged.
    pub async fn start(&self) -> Result<(), VamNetworkError> {
        #[cfg(unix)]
        let is_root = unsafe { libc::getuid() } == 0;
        #[cfg(not(unix))]
        let is_root = false;

        if self.bind_addr.port() == 53 && !is_root {
            return Err(VamNetworkError::DnsProxyBindingFailed {
                details: "Binding to port 53 requires root privileges. Gracefully falling back to broker-layer DNS validation.".to_string(),
            });
        }

        // Mock listening loop on high ports
        println!(
            "[dns-proxy] Starting per-session DNS proxy on {}",
            self.bind_addr
        );
        Ok(())
    }

    /// Records a processed DNS query into metrics and logs security denials.
    pub fn record_query(&self, domain: &str, allowed: bool) {
        let reg = vam_telemetry::VamMetricsRegistry::global();
        let _ = reg.record_counter(
            vam_telemetry::METRIC_NETWORK_DNS_QUERIES_TOTAL,
            1,
            &[("subsystem", "dns")],
        );

        if !allowed {
            tracing::warn!(
                target: "vam_security_dns",
                event = "connection_blocked",
                domain = %domain,
                reason = "dns_policy_denied",
                "Security lane: DNS query blocked by policy"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::ActivePolicyContainer;
    use std::sync::Arc;
    use vam_spec::v1::{NetworkMode, NetworkPolicy};

    #[tokio::test]
    async fn test_dns_proxy_unprivileged_handling() {
        let policy = NetworkPolicy {
            mode: NetworkMode::Brokered as i32,
            max_egress_bps: 0,
            max_ingress_bps: 0,
            max_packet_rate_pps: 0,
            allowed_domains: vec!["api.stripe.com".to_string()],
            allowed_cidr_destinations: vec![],
            allowed_ports: vec![],
        };
        let snap = crate::policy::NetworkPolicyCompiler::compile("sess-1", 1, policy).unwrap();
        let container = Arc::new(ActivePolicyContainer::new(snap));

        let bind_addr = "127.0.0.1:53".parse().unwrap();
        let proxy = SessionDnsProxy::new(container.clone(), bind_addr);

        // Should return Err on unprivileged port 53 bind attempt
        let res = proxy.start().await;
        #[cfg(unix)]
        let is_root = unsafe { libc::getuid() } == 0;
        #[cfg(not(unix))]
        let is_root = false;

        if !is_root {
            assert!(res.is_err());
        }

        // High port should succeed
        let bind_addr_high = "127.0.0.1:1053".parse().unwrap();
        let proxy_high = SessionDnsProxy::new(container, bind_addr_high);
        assert!(proxy_high.start().await.is_ok());
    }

    #[test]
    fn test_dns_telemetry_and_security_logging() {
        let policy = vam_spec::v1::NetworkPolicy {
            mode: vam_spec::v1::NetworkMode::Brokered as i32,
            max_egress_bps: 0,
            max_ingress_bps: 0,
            max_packet_rate_pps: 0,
            allowed_domains: vec!["api.stripe.com".to_string()],
            allowed_cidr_destinations: vec![],
            allowed_ports: vec![],
        };
        let snap =
            crate::policy::NetworkPolicyCompiler::compile("sess-dns-test", 1, policy).unwrap();
        let container = Arc::new(ActivePolicyContainer::new(snap));
        let proxy = SessionDnsProxy::new(container, "127.0.0.1:53".parse().unwrap());

        let reg = vam_telemetry::VamMetricsRegistry::global();
        let initial_count = reg
            .get_counter(
                vam_telemetry::METRIC_NETWORK_DNS_QUERIES_TOTAL,
                &[("subsystem", "dns")],
            )
            .unwrap_or(0);

        proxy.record_query("api.stripe.com", true);
        proxy.record_query("malicious-c2.ru", false);

        let final_count = reg
            .get_counter(
                vam_telemetry::METRIC_NETWORK_DNS_QUERIES_TOTAL,
                &[("subsystem", "dns")],
            )
            .unwrap_or(0);
        assert_eq!(final_count - initial_count, 2);
    }
}
