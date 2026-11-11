use crate::error::VamNetworkError;
use async_trait::async_trait;
use std::sync::Mutex;

#[async_trait]
pub trait KernelNetAdapter: Send + Sync + std::fmt::Debug {
    async fn create_netns(&self, name: &str) -> Result<(), VamNetworkError>;
    async fn delete_netns(&self, name: &str) -> Result<(), VamNetworkError>;
    async fn create_tap(&self, name: &str) -> Result<(), VamNetworkError>;
    async fn create_veth_pair(
        &self,
        host_name: &str,
        peer_name: &str,
    ) -> Result<(), VamNetworkError>;
    async fn move_interface_to_netns(
        &self,
        iface: &str,
        netns: &str,
    ) -> Result<(), VamNetworkError>;
    async fn assign_ip(
        &self,
        iface: &str,
        ip_cidr: &str,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError>;
    async fn set_interface_up(
        &self,
        iface: &str,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError>;
    async fn apply_nat_masquerade(
        &self,
        host_iface: &str,
        guest_subnet: &str,
    ) -> Result<(), VamNetworkError>;
    async fn apply_inter_vm_isolation(&self, guest_subnet: &str) -> Result<(), VamNetworkError>;
    async fn apply_tc_shaping(
        &self,
        iface: &str,
        egress_bps: Option<u64>,
        ingress_bps: Option<u64>,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError>;
}

#[derive(Debug, Default)]
pub struct LinuxKernelNetAdapter;

impl LinuxKernelNetAdapter {
    pub fn new() -> Self {
        Self
    }

    fn run_cmd(&self, program: &str, args: &[&str]) -> Result<(), VamNetworkError> {
        let mut child = std::process::Command::new(program);
        child.args(args);

        let output = child.output().map_err(|e| {
            VamNetworkError::Internal(format!("Failed to execute command '{:?}': {}", child, e))
        })?;

        if !output.status.success() {
            let err_msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(VamNetworkError::Internal(format!(
                "Command '{:?}' failed with exit code {}: {}",
                child,
                output.status.code().unwrap_or(-1),
                err_msg
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl KernelNetAdapter for LinuxKernelNetAdapter {
    async fn create_netns(&self, name: &str) -> Result<(), VamNetworkError> {
        self.run_cmd("sudo", &["ip", "netns", "add", name])
    }

    async fn delete_netns(&self, name: &str) -> Result<(), VamNetworkError> {
        self.run_cmd("sudo", &["ip", "netns", "del", name])
    }

    async fn create_tap(&self, name: &str) -> Result<(), VamNetworkError> {
        self.run_cmd("sudo", &["ip", "tuntap", "add", "mode", "tap", name])
    }

    async fn create_veth_pair(
        &self,
        host_name: &str,
        peer_name: &str,
    ) -> Result<(), VamNetworkError> {
        self.run_cmd(
            "sudo",
            &[
                "ip", "link", "add", host_name, "type", "veth", "peer", "name", peer_name,
            ],
        )
    }

    async fn move_interface_to_netns(
        &self,
        iface: &str,
        netns: &str,
    ) -> Result<(), VamNetworkError> {
        self.run_cmd("sudo", &["ip", "link", "set", iface, "netns", netns])
    }

    async fn assign_ip(
        &self,
        iface: &str,
        ip_cidr: &str,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError> {
        if let Some(ns) = netns {
            self.run_cmd(
                "sudo",
                &[
                    "ip", "netns", "exec", ns, "ip", "addr", "add", ip_cidr, "dev", iface,
                ],
            )
        } else {
            self.run_cmd("sudo", &["ip", "addr", "add", ip_cidr, "dev", iface])
        }
    }

    async fn set_interface_up(
        &self,
        iface: &str,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError> {
        if let Some(ns) = netns {
            self.run_cmd(
                "sudo",
                &["ip", "netns", "exec", ns, "ip", "link", "set", iface, "up"],
            )
        } else {
            self.run_cmd("sudo", &["ip", "link", "set", iface, "up"])
        }
    }

    async fn apply_nat_masquerade(
        &self,
        host_iface: &str,
        guest_subnet: &str,
    ) -> Result<(), VamNetworkError> {
        self.run_cmd(
            "sudo",
            &[
                "iptables",
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-s",
                guest_subnet,
                "-o",
                host_iface,
                "-j",
                "MASQUERADE",
            ],
        )
    }

    async fn apply_inter_vm_isolation(&self, guest_subnet: &str) -> Result<(), VamNetworkError> {
        self.run_cmd(
            "sudo",
            &[
                "iptables",
                "-A",
                "FORWARD",
                "-s",
                guest_subnet,
                "-d",
                guest_subnet,
                "-j",
                "DROP",
            ],
        )
    }

    async fn apply_tc_shaping(
        &self,
        iface: &str,
        egress_bps: Option<u64>,
        _ingress_bps: Option<u64>,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError> {
        if let Some(ns) = netns {
            let _ = self.run_cmd(
                "sudo",
                &[
                    "ip", "netns", "exec", ns, "tc", "qdisc", "del", "dev", iface, "root",
                ],
            );
        } else {
            let _ = self.run_cmd("sudo", &["tc", "qdisc", "del", "dev", iface, "root"]);
        }

        if let Some(bps) = egress_bps {
            let rate_str = format!("{}bit", bps);
            if let Some(ns) = netns {
                self.run_cmd(
                    "sudo",
                    &[
                        "ip", "netns", "exec", ns, "tc", "qdisc", "add", "dev", iface, "root",
                        "tbf", "rate", &rate_str, "burst", "32k", "latency", "400ms",
                    ],
                )?;
            } else {
                self.run_cmd(
                    "sudo",
                    &[
                        "tc", "qdisc", "add", "dev", iface, "root", "tbf", "rate", &rate_str,
                        "burst", "32k", "latency", "400ms",
                    ],
                )?;
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct MockKernelAdapter {
    pub history: Mutex<Vec<String>>,
}

impl Default for MockKernelAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl MockKernelAdapter {
    pub fn new() -> Self {
        Self {
            history: Mutex::new(Vec::new()),
        }
    }

    pub fn record(&self, action: &str) {
        if let Ok(mut hist) = self.history.lock() {
            hist.push(action.to_string());
        }
    }

    pub fn get_history(&self) -> Vec<String> {
        if let Ok(hist) = self.history.lock() {
            hist.clone()
        } else {
            Vec::new()
        }
    }
}

#[async_trait]
impl KernelNetAdapter for MockKernelAdapter {
    async fn create_netns(&self, name: &str) -> Result<(), VamNetworkError> {
        self.record(&format!("create_netns: {}", name));
        Ok(())
    }

    async fn delete_netns(&self, name: &str) -> Result<(), VamNetworkError> {
        self.record(&format!("delete_netns: {}", name));
        Ok(())
    }

    async fn create_tap(&self, name: &str) -> Result<(), VamNetworkError> {
        self.record(&format!("create_tap: {}", name));
        Ok(())
    }

    async fn create_veth_pair(
        &self,
        host_name: &str,
        peer_name: &str,
    ) -> Result<(), VamNetworkError> {
        self.record(&format!(
            "create_veth_pair: {} <-> {}",
            host_name, peer_name
        ));
        Ok(())
    }

    async fn move_interface_to_netns(
        &self,
        iface: &str,
        netns: &str,
    ) -> Result<(), VamNetworkError> {
        self.record(&format!("move_interface_to_netns: {} -> {}", iface, netns));
        Ok(())
    }

    async fn assign_ip(
        &self,
        iface: &str,
        ip_cidr: &str,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError> {
        let ns_str = netns
            .map(|ns| format!(" in netns {}", ns))
            .unwrap_or_default();
        self.record(&format!("assign_ip: {} to {}{}", ip_cidr, iface, ns_str));
        Ok(())
    }

    async fn set_interface_up(
        &self,
        iface: &str,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError> {
        let ns_str = netns
            .map(|ns| format!(" in netns {}", ns))
            .unwrap_or_default();
        self.record(&format!("set_interface_up: {}{}", iface, ns_str));
        Ok(())
    }

    async fn apply_nat_masquerade(
        &self,
        host_iface: &str,
        guest_subnet: &str,
    ) -> Result<(), VamNetworkError> {
        self.record(&format!(
            "apply_nat_masquerade: {} via {}",
            guest_subnet, host_iface
        ));
        Ok(())
    }

    async fn apply_inter_vm_isolation(&self, guest_subnet: &str) -> Result<(), VamNetworkError> {
        self.record(&format!("apply_inter_vm_isolation: {}", guest_subnet));
        Ok(())
    }

    async fn apply_tc_shaping(
        &self,
        iface: &str,
        egress_bps: Option<u64>,
        ingress_bps: Option<u64>,
        netns: Option<&str>,
    ) -> Result<(), VamNetworkError> {
        let ns_str = netns
            .map(|ns| format!(" in netns {}", ns))
            .unwrap_or_default();
        self.record(&format!(
            "apply_tc_shaping: {} (egress: {:?}, ingress: {:?}){}",
            iface, egress_bps, ingress_bps, ns_str
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_kernel_adapter_lifecycle() {
        let adapter = MockKernelAdapter::new();
        adapter.create_netns("session_1").await.unwrap();
        adapter.create_tap("tap_s1").await.unwrap();
        adapter
            .assign_ip("tap_s1", "172.20.0.1/30", Some("session_1"))
            .await
            .unwrap();

        let history = adapter.get_history();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0], "create_netns: session_1");
        assert_eq!(history[1], "create_tap: tap_s1");
        assert_eq!(
            history[2],
            "assign_ip: 172.20.0.1/30 to tap_s1 in netns session_1"
        );
    }
}
