use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::VamTelemetryError;
use crate::metrics::*;

/// vCPU exit and instruction filter metrics from Firecracker.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VcpuMetrics {
    #[serde(default)]
    pub exit_io_in: u64,
    #[serde(default)]
    pub exit_io_out: u64,
    #[serde(default)]
    pub exit_mmio_read: u64,
    #[serde(default)]
    pub exit_mmio_write: u64,
    #[serde(default)]
    pub filter_skips: u64,
}

impl VcpuMetrics {
    pub fn total_exits(&self) -> u64 {
        self.exit_io_in + self.exit_io_out + self.exit_mmio_read + self.exit_mmio_write
    }
}

/// VirtIO Block device I/O metrics from Firecracker.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BlockMetrics {
    #[serde(default)]
    pub read_bytes: u64,
    #[serde(default)]
    pub write_bytes: u64,
    #[serde(default)]
    pub read_count: u64,
    #[serde(default)]
    pub write_count: u64,
    #[serde(default)]
    pub rate_limiter_throttled_events: u64,
}

/// VirtIO Network device packet/byte metrics from Firecracker.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct NetMetrics {
    #[serde(default)]
    pub rx_bytes: u64,
    #[serde(default)]
    pub tx_bytes: u64,
    #[serde(default)]
    pub rx_packets: u64,
    #[serde(default)]
    pub tx_packets: u64,
    #[serde(default)]
    pub rx_dropped_packets: u64,
}

/// AF_VSOCK device packet/byte metrics from Firecracker.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VsockMetrics {
    #[serde(default)]
    pub rx_bytes: u64,
    #[serde(default)]
    pub tx_bytes: u64,
    #[serde(default)]
    pub rx_packets: u64,
    #[serde(default)]
    pub tx_packets: u64,
}

/// Single-line JSON metrics flushed periodically by Firecracker's `/metrics` subsystem.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct FirecrackerMetricsSnapshot {
    #[serde(default)]
    pub utc_timestamp_ms: u64,
    #[serde(default)]
    pub vcpu: VcpuMetrics,
    #[serde(default)]
    pub block: BlockMetrics,
    #[serde(default)]
    pub net: NetMetrics,
    #[serde(default)]
    pub vsock: VsockMetrics,
}

/// Parse a raw single-line JSON string from Firecracker's metrics pipe or file.
pub fn parse_firecracker_metrics(
    json_line: &str,
) -> Result<FirecrackerMetricsSnapshot, VamTelemetryError> {
    serde_json::from_str::<FirecrackerMetricsSnapshot>(json_line.trim()).map_err(|e| {
        VamTelemetryError::HypervisorMetricsParseFailed {
            details: format!("Failed to parse Firecracker JSON metrics: {}", e),
        }
    })
}

/// Record scraped Firecracker metrics into the VAM metrics registry and emit session span events.
pub fn record_firecracker_metrics(
    session_id: &str,
    snapshot: &FirecrackerMetricsSnapshot,
    registry: &VamMetricsRegistry,
) -> Result<(), VamTelemetryError> {
    // 1. vCPU Exits
    let total_vcpus_exits = snapshot.vcpu.total_exits();
    if total_vcpus_exits > 0 {
        registry.set_counter(
            METRIC_HYPERVISOR_VCPU_EXITS_TOTAL,
            total_vcpus_exits,
            &[("subsystem", "vcpu")],
        )?;
    }

    // 2. Block I/O bytes
    if snapshot.block.read_bytes > 0 {
        registry.set_counter(
            METRIC_HYPERVISOR_BLOCK_READ_BYTES,
            snapshot.block.read_bytes,
            &[("direction", "read"), ("subsystem", "block")],
        )?;
    }
    if snapshot.block.write_bytes > 0 {
        registry.set_counter(
            METRIC_HYPERVISOR_BLOCK_WRITE_BYTES,
            snapshot.block.write_bytes,
            &[("direction", "write"), ("subsystem", "block")],
        )?;
    }

    // 3. Network bytes
    if snapshot.net.rx_bytes > 0 {
        registry.set_counter(
            METRIC_HYPERVISOR_NET_RX_BYTES,
            snapshot.net.rx_bytes,
            &[("direction", "rx"), ("subsystem", "net")],
        )?;
    }
    if snapshot.net.tx_bytes > 0 {
        registry.set_counter(
            METRIC_HYPERVISOR_NET_TX_BYTES,
            snapshot.net.tx_bytes,
            &[("direction", "tx"), ("subsystem", "net")],
        )?;
    }

    // 4. Vsock bytes
    if snapshot.vsock.rx_bytes > 0 {
        registry.set_counter(
            METRIC_HYPERVISOR_VSOCK_RX_BYTES,
            snapshot.vsock.rx_bytes,
            &[("direction", "rx"), ("subsystem", "vsock")],
        )?;
    }
    if snapshot.vsock.tx_bytes > 0 {
        registry.set_counter(
            METRIC_HYPERVISOR_VSOCK_TX_BYTES,
            snapshot.vsock.tx_bytes,
            &[("direction", "tx"), ("subsystem", "vsock")],
        )?;
    }

    // 5. Emit structured event under the host session span
    info!(
        target: "vam_hypervisor",
        session_id = %session_id,
        vcpu_exits = total_vcpus_exits,
        block_read_bytes = snapshot.block.read_bytes,
        block_write_bytes = snapshot.block.write_bytes,
        net_rx_bytes = snapshot.net.rx_bytes,
        net_tx_bytes = snapshot.net.tx_bytes,
        vsock_rx_bytes = snapshot.vsock.rx_bytes,
        vsock_tx_bytes = snapshot.vsock.tx_bytes,
        "Scraped Firecracker hypervisor metrics"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_and_record_firecracker_metrics() {
        let json_line = r#"{
            "utc_timestamp_ms": 1725720000000,
            "vcpu": {
                "exit_io_in": 10,
                "exit_io_out": 20,
                "exit_mmio_read": 30,
                "exit_mmio_write": 40,
                "filter_skips": 0
            },
            "block": {
                "read_bytes": 1048576,
                "write_bytes": 524288,
                "read_count": 256,
                "write_count": 128,
                "rate_limiter_throttled_events": 0
            },
            "net": {
                "rx_bytes": 204800,
                "tx_bytes": 102400,
                "rx_packets": 1500,
                "tx_packets": 900,
                "rx_dropped_packets": 0
            },
            "vsock": {
                "rx_bytes": 65536,
                "tx_bytes": 32768,
                "rx_packets": 400,
                "tx_packets": 200
            }
        }"#;

        let snapshot = parse_firecracker_metrics(json_line).unwrap();
        assert_eq!(snapshot.utc_timestamp_ms, 1725720000000);
        assert_eq!(snapshot.vcpu.total_exits(), 100);
        assert_eq!(snapshot.block.read_bytes, 1048576);
        assert_eq!(snapshot.net.rx_bytes, 204800);
        assert_eq!(snapshot.vsock.tx_bytes, 32768);

        let registry = VamMetricsRegistry::new();
        record_firecracker_metrics("session-test-fc", &snapshot, &registry).unwrap();

        assert_eq!(
            registry.get_counter(METRIC_HYPERVISOR_VCPU_EXITS_TOTAL, &[("subsystem", "vcpu")]),
            Some(100)
        );
        assert_eq!(
            registry.get_counter(
                METRIC_HYPERVISOR_BLOCK_READ_BYTES,
                &[("direction", "read"), ("subsystem", "block")]
            ),
            Some(1048576)
        );
        assert_eq!(
            registry.get_counter(
                METRIC_HYPERVISOR_NET_RX_BYTES,
                &[("direction", "rx"), ("subsystem", "net")]
            ),
            Some(204800)
        );
        assert_eq!(
            registry.get_counter(
                METRIC_HYPERVISOR_VSOCK_TX_BYTES,
                &[("direction", "tx"), ("subsystem", "vsock")]
            ),
            Some(32768)
        );
    }
}
