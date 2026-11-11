# VAM Host Orchestrator & `vamd` Development Invariants

This file details the strict coding and operational rules that every agent or developer must adhere to when working inside `crates/vam-orchestrator/`. For detailed design rationales, refer to [Orchestrator Invariants](/orchestrator/invariants/) in the VAM documentation site.

---

## 1. Idempotency over Mutating APIs
* **Enforce validation of client-generated idempotency keys**. Do not process session creation, pausing, resuming, or termination commands without verifying the `idempotency_key` string.
* **Match committed keys before allocating resources**. If an incoming key exists, immediately return the active session metadata (matching VM ID, PID, CID) instead of initiating new virtual hardware setups.

## 2. Durable State Separation & Startup Sequencing
* **Write state to SQLite before physical resource allocation**. Always write session intents and resource mappings to `/var/lib/vam/state.db` (configured in WAL mode) *before* configuring TAP devices, allocating CIDs, or spawning child processes.
* **Block API admission during startup reconciliation**. Complete the full adoption and orphan reaping sweep *before* binding the gRPC socket or notifying systemd (`READY=1`).
* **Avoid identifying processes by PID alone**. Always store and validate the `vmm_start_time` (parsed from `/proc/<pid>/stat`) to neutralize PID reuse vulnerabilities on host restarts.
* **Own workflow transitions**. Persist run, attempt, lease, approval, and
  outbox transitions before dispatching guest work. Reject results whose lease,
  generation, fencing token, or expiry is stale.

## 3. Stream Routing & Memory Safeguards
* **Validate lifecycle generations on stream frames**. Every session must track an auto-incrementing `lifecycle_generation` integer. Drop any incoming storage or control frame carrying an older generation value.
* **Enforce hard maximum limits on gRPC stream proxies**. Bounded stream proxy buffers must not exceed **16MB**. Implement active backpressure over vsock if the threshold is crossed, and terminate the client after a 30-second stall.

## 4. Hardware Scheduling & Capacity Barriers
* **Utilize cgroups v2 `cpuset` as the resource authority**. Pin guest vCPUs to physical cores, and restrict RAM allocations to corresponding NUMA nodes. Do not rely on raw `taskset` commands.
* **Enforce strict no-overcommit RAM barriers**. Reservable scheduling budgets must always account for a minimum host headroom (default **2GB RAM** and **2 CPU Cores**). Reject any admissions that exceed available limits.

## 5. Guest telemetry ingest
* **Observational only.** `StreamLogs` / `GuestStreamReceiver` must not change Cedar or workflow state. Guest `vam.v1.tool.completed` / `vam.v1.tool.failed` are outbox rows on `run_id`, not trajectory-authoritative.
* **Capture tier.** Read `VAMD_TELEMETRY_CAPTURE` on `DaemonConfig` (`none` / `digest` / `redacted-full`, default `digest`) and pass it into `spawn_session_brokers` → `GuestStreamReceiver`. Persist redacted stdout/stderr to Drive9 CAS when the tier allows; never use a guest digest as the CAS key.

## 6. Coding Standards & Error Handling
* **Zero tolerance for panics**. Report all errors using `thiserror` mapping to the spec's `VamErrorCode` payload. Never use `.unwrap()`, `.expect()`, or raw `panic!` blocks in orchestrator libraries.
* **Default to safe, degraded draining states**. When shutting down or upgrading, stop new admissions, drain active streams, flush Drive9 overlays to remote cloud stores, and release network locks before terminating physical hypervisor processes.
