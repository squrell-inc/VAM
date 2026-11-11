use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkCapabilities {
    pub has_ip_route2: bool,
    pub has_iptables: bool,
    pub has_nftables: bool,
    pub has_tc: bool,
    pub has_dnsmasq: bool,
    pub has_net_admin: bool,
    pub has_unprivileged_netns: bool,
}

impl NetworkCapabilities {
    pub fn probe() -> Self {
        let has_ip_route2 = check_command("ip");
        let has_iptables = check_command("iptables");
        let has_nftables = check_command("nft");
        let has_tc = check_command("tc");
        let has_dnsmasq = check_command("dnsmasq");

        // Check if we are running as root or have CAP_NET_ADMIN
        let has_net_admin = check_net_admin();

        // Check unprivileged netns capability by attempting unshare -rn true
        let has_unprivileged_netns = check_unprivileged_netns();

        Self {
            has_ip_route2,
            has_iptables,
            has_nftables,
            has_tc,
            has_dnsmasq,
            has_net_admin,
            has_unprivileged_netns,
        }
    }

    /// Determines if a specific network mode is supported under current host capabilities.
    pub fn supports_mode(&self, mode: crate::config::NetworkMode) -> bool {
        match mode {
            crate::config::NetworkMode::Isolated => true,
            crate::config::NetworkMode::BrokeredEgress => true, // only needs vsock listener
            crate::config::NetworkMode::RestrictedDirect => {
                // Needs ip, tc, some firewall (iptables or nftables), and netns/net_admin privileges
                self.has_ip_route2
                    && (self.has_iptables || self.has_nftables)
                    && (self.has_net_admin || self.has_unprivileged_netns)
            }
        }
    }
}

fn check_command(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn check_net_admin() -> bool {
    #[cfg(unix)]
    if unsafe { libc::getuid() } == 0 {
        return true;
    }
    // Alternatively check by trying to create a dummy link (will fail if no CAP_NET_ADMIN)
    false
}

fn check_unprivileged_netns() -> bool {
    Command::new("unshare")
        .args(["-rn", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capabilities_probe() {
        let caps = NetworkCapabilities::probe();
        println!("Probed network capabilities: {:?}", caps);
        #[cfg(target_os = "linux")]
        {
            // On Linux environments ip command should be present
            if std::path::Path::new("/sbin/ip").exists()
                || std::path::Path::new("/usr/sbin/ip").exists()
                || check_command("ip")
            {
                assert!(caps.has_ip_route2, "ip command should be present");
            }
        }
        // isolated mode should always be supported
        assert!(caps.supports_mode(crate::config::NetworkMode::Isolated));
        assert!(caps.supports_mode(crate::config::NetworkMode::BrokeredEgress));
    }
}
