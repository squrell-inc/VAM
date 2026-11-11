> Moved from the docs site on 2026-10-31: design intent, not built (a UDS-to-vsock gRPC bridge for external clients, not built).

---
title: Vsock gRPC Stream Router
description: Multiplexing host Unix sockets to guest vsock frames, handling stream correlation IDs, and enforcing bounded buffers.
---

:::caution[Alpha Stability Warning]
VAM orchestration features and subsystems (under the `vam-orchestrator` crate) are currently in **Alpha**. The host daemon process model, scheduling policies, and session routing layers are actively evolving, breaking changes are expected, and no production-readiness guarantees are provided at this stage. Do not deploy in untrusted production environments without custom hardening.
:::

Because external clients (SDKs, CLI tools, dashboards) speak standard TCP/IP gRPC protocols while guest microVMs communicate strictly over isolated `AF_VSOCK` channels, `vamd` must act as a high-speed transit bridge. This page documents the **Vsock gRPC Stream Router**.

---

## 1. UDS-to-Vsock Translation Pipeline

`vamd` listens on local Unix Domain Sockets (UDS) on the host, spawning dedicated stream routers to translate standard network payloads into VAM binary frames:

```text
 ┌────────────────────────────────────────────────────────┐
 │ Host: External gRPC Caller (SDK / CLI / Dashboard)     │
 └──────────────────────────┬─────────────────────────────┘
                            │ gRPC over UDS: `ExecuteTool()`
                            ▼
 ┌────────────────────────────────────────────────────────┐
 │ Host Daemon: `vamd` Stream Router                      │
 ├────────────────────────────────────────────────────────┤
 │ [ ] Match `session_id` to active Vsock CID `42`        │
 │ [ ] Wrap serialized gRPC payload into `CONTROL_GRPC`   │
 │ [ ] Assign unique Correlation ID `1102`                │
 └──────────────────────────┬─────────────────────────────┘
                            │
                            │ Translates and dispatches
                            ▼ (AF_VSOCK Port 52)
 ┌────────────────────────────────────────────────────────┐
 │ Guest: `vam-runtime` RPC Server                        │
 └────────────────────────────────────────────────────────┘
```

When a client calls a gRPC method (such as `ExecuteTool`), `vamd` captures the serialized protobuf bytes, wraps them in a VAM binary header (using `Frame Type 0x04` for `CONTROL_GRPC`), assigns a dynamic **Correlation ID**, and streams the payload over the vsock connection.

---

## 2. Bidirectional Stream Multiplexing & Correlation

To prevent a single heavy stream (such as a massive tool stdout log dump) from locking up or blocking other control-plane commands (like liveness pings), `vamd` multiplexes all traffic over a single vsock connection:
* **The Correlation ID**: Every frame sent carries a 4-byte big-endian Correlation ID.
* **Concurrent Demuxing**: A background read thread on the host continuously pulls packets from `AF_VSOCK` Port 52, checking the Correlation ID to route standard output blocks (`STREAM_STDOUT`, `STREAM_STDERR`) or RPC responses (`CONTROL_GRPC`) back to the respective caller's active UDS stream, enabling concurrent bidirectional communication over a single socket file-descriptor.

---

## 3. Bounded Stream Buffering & Backpressure (Decision O15)

In high-consequence environments, slow clients or network hiccups on the host can cause huge memory leaks if `vamd` attempts to buffer unread log streams indefinitely.

`vamd` implements strict **Bounded Buffer Controls**:
* **Buffer Limits**: Every active stream router is allocated a bounded in-memory FIFO queue (default **16MB** of buffered stdout/stderr bytes).
* **Backpressure Propagation**: If a client is slow to read logs, the host buffer fills up. Once the 16MB threshold is crossed, the host-side router:
  1. Pauses reading packets from the target VM's vsock channel.
  2. The guest `vam-runtime` detects the filled vsock socket buffer and pauses its local subprocess stdout stream.
  3. This feedback loop successfully throttles guest-side file dumps, safeguarding the host daemon from out-of-memory crashes.
* **Forced Disconnection**: If the buffer remains saturated beyond a strict timeout (default **30 seconds**), the host router drops the slow client and flushes the buffer to preserve host stability.

---

## 4. Next Steps

- Explore how we track CIDs and PIDs to configure these routing paths in [Durable Session State & SQLite Schema](/orchestrator/session/).
- To see how the guest decodes these multiplexed frames, read [Guest Vsock RPC Server](/runtime/vsock-server/).
- Review the complete latency and performance limits in [Orchestrator Telemetry & SLOs](/orchestrator/telemetry/).
