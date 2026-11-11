> Moved from the docs site on 2026-10-30: design intent, not built.

---
title: Session Lifecycle & Execution Flow
description: The step-by-step journey of an agent session from admission to garbage collection.
---

:::caution[Alpha Stability Warning]
The VAM session lifecycle, timing targets, and state transitions represent planned design specifications.
:::

A session is VAM's primary logical and physical isolation unit. This page documents the canonical ten-phase lifecycle of an agent session, identifying the owning component, state transitions, failure boundaries, and emitted lifecycle events for each stage.

---

## 1. Unified Session Lifecycle Flow

The diagram below details how a single agent request moves through the system, transitioning from host control to in-VM guest execution and back to host-side teardown:

```text
       [ External API Client / SDK ]
                     │
                     ▼ (Phase 1: Admission & Auth)
 ┌───────────────────────────────────────┐
 │ `vamd` verifies client JWT & budget   │
 └───────────────────┬───────────────────┘
                     │
                     ▼ (Phase 2: Allocation)
 ┌───────────────────────────────────────┐
 │ `vam-storage` materializes Drive9     │
 │ Resolves agent `skill_refs` from the  │
 │ local catalog; overlays blobs on seed │
 │ TAP/IP only on Restricted Direct path │
 └───────────────────┬───────────────────┘
                     │
                     ▼ (Phase 3: VM Boot / Claim)
 ┌───────────────────────────────────────┐
 │ Golden restore (1 GiB) or cold boot   │
 │ `vam-core` configures Firecracker VMM │
 └───────────────────┬───────────────────┘
                     │
                     ▼ (Phase 4: Guest Registration)
 ┌───────────────────────────────────────┐
 │ Guest PID 1 init handshakes over vsock│
 │ Mounts `/mnt/drive9` via FUSE frontend│
 └───────────────────┬───────────────────┘
                     │
                     ▼ (Phase 5: Leased Task Execution)
 ┌───────────────────────────────────────┐
 │ Host schedules durable step attempts; │
 │ guest executes bounded task leases     │
 └──────────────────┬────────────────────┘
                    │
            ┌───────┴───────┐ (Phase 6: HITL Pause - Optional)
            ▼               ▼
      [ Safe Run ]    [ Trigger Hit ] ──► Checkpoint workspace, create durable
            │               │             approval, wait for authenticated resume.
            └───────┬───────┘
                    │
                    ▼ (Phase 7: Evaluation & Checkpoint)
 ┌───────────────────────────────────────┐
 │ Evaluate immutable evidence on trusted │
 │ boundary; sync final workspace chunks  │
 └───────────────────┬───────────────────┘
                     │
                     ▼ (Phase 8: Graceful Teardown & GC)
 ┌───────────────────────────────────────┐
 │ Terminate VMM PID, release TAP/IP,    │
 │ Sign audit ledger, clear SQLite index │
 └───────────────────────────────────────┘
```

---

## 2. Detailed Lifecycle Phases

| Phase | Owning Component | State Transition (SQLite) | Failure Boundary & Action | Emitted CloudEvent | Design Latency Target |
|---|---|---|---|---|---|
| **1. Admission & Auth** | `vam-orchestrator` / `vam-security` | `PENDING` | **Auth/Budget Fail**: Reject request immediately; do not allocate resources. | None | `< 2ms` |
| **2. Allocation** | `vam-storage` / `vam-network` | `ALLOCATED` | **Resource Fail**: Release partial networks; return exhaustion error to caller. | None | `< 5ms` |
| **3. VM Boot / Claim** | `vam-core` / `vam-orchestrator` | `VM_BOOTING` | **Boot Fail**: Kill process; retry allocation once or trigger fail-closed cleanup. | `vam.v1.vm.booted` | **~115 ms VM-ready, ~150 to 250 ms whole session start on the CI lane** (Tier 1 restore, measured), **~345–370 ms** (cold boot, measured); `< 10ms` Tier 2 claim (not shipped) |
| **4. Guest Init** | `vam-runtime` | `GUEST_READY` | **Init Fail**: If handshake misses within timeout (e.g. 500ms), terminate VM. | `vam.v1.agent.started` | `< 15ms` |
| **5. Playbook Run** | `vam-orchestrator` / `vam-runtime` | `RUNNING` | **Code Fail**: Guest reports an attempt result; host records retry, fallback, or terminal transition. | `vam.v1.step.attempted` | Variable (Inference + tool duration) |
| **6. HITL Pause** *(Opt)* | `vam-orchestrator` / `vam-security` | `SUSPENDING` / `PAUSED` | **Timeout/Reject Fail**: Commit fail-closed transition; a retained VM is optional, not required for resume. | `vam.v1.hitl.paused` | User/Operator dependent |
| **7. Checkpoint** | `vam-storage` / `vam-orchestrator` | `CHECKPOINTING` | **Sync Fail**: Do not publish approval; re-queue or fail the step according to policy. | `vam.v1.workspace.synced` | Asynchronous, `< 50ms` block time |
| **8. Teardown & GC** | `vam-orchestrator` / `vam-core` | `TERMINATED` | **GC Fail**: Force-kill VMM process; quarantine TAP socket; trigger cleanup cron. | `vam.v1.agent.stopped` | `< 50ms` |

---

## 3. Operational Rules & Guarantees

To ensure VAM operates with high reliability, contributors must enforce these rules:

* **Default path (Isolated / Brokered)**: No TAP device, no virtio-net — vsock-only host↔guest and egress proxy. Network allocation steps are skipped (`NetResourcePool` capacity 0).
* **Restricted Direct**: TAP and IPAM apply when the TAP pool has capacity (not the default create path today).
* **No Runnable Latency Promises**: Sub-10 ms claim and sub-50 ms UFFD restore targets are **design goals** — Tier 1 measured restore is ~115 ms VM-ready (~150 to 250 ms whole session start on the CI lane); Tier 2 standbys are not shipped. Physical network latency, S3 connection speed, and host disk degradation will impact times.
* **Idempotency Invariant**: Every transition step is designed to be re-run safely. If a request to boot VM `sess_abc` is executed twice due to network retries, the orchestrator identifies the active session ID, skips replication, and returns the existing handle.
* **Restore-or-cold boot**: When golden artifacts exist and the session Drive9 image is 1 GiB, create loads a paused snapshot, patches drive/vsock, resumes, and reseeds — otherwise cold boot + registration handshake.
* **Synchronous Cleanup**: Under no circumstances must a session's networks, sockets, or chroot folders be left allocated after transition to `TERMINATED`. If a graceful shutdown fails, `vam-core` executes a synchronous force-cleanup of TAP and socket links.

---

## Next Steps

* Study the physical virtualization boundaries and hardware setup in [Firecracker Isolation](/architecture/firecracker/).
* Read about Drive9's virtual workspace persistence in [Drive9 Storage](/architecture/drive9/).
* Review host-side control plane APIs in [Host Control API](/orchestrator/api/).
