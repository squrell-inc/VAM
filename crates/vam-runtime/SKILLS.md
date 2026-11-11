# VAM Guest Runtime Invariants

This file details the strict coding and operational rules that every agent or developer must adhere to when working inside `crates/vam-runtime/`. For detailed design rationales, refer to [Guest Invariants](/runtime/invariants/) in the VAM documentation site.

---

## 1. System Initialization and PID 1
* **Never let PID 1 panic or return**. Under Linux, if PID 1 exits, the guest kernel panics instantly. Route all terminations to an explicit reboot poweroff call.
* **Actively reap orphaned processes**. Catch the `SIGCHLD` signal and loop `waitpid` to reclaim dead grandchildren, preventing zombie limits from exhausting guest resources.
* **Do not signal early readiness**. Do not trigger the `RegisterAgent` control handshake until filesystems, FUSE mounts, and the Port 52 VSOCK listener are fully listening and functional.

## 2. Sandbox Security boundaries
* **Never store credentials or API keys in guest memory**. Guest RAM is actively snapshotted to host disk files. All cloud and provider inference must route through host-side proxies.
* **Never sign HITL tokens in the guest VM**. The guest represents a compromised boundary; the host must control and sign all approval tokens.
* **Execute all user scripts unprivileged**. Switch tool subprocesses to unprivileged system users (`uid=1000`, `gid=100`) before running dynamic code.

## 3. Cognitive Context and Leased Task Execution
* **Enforce context eviction limits**. System instructions, active step prompts, and tool specifications are protected and must never be evicted during compaction.
* **Enforce mandatory execution timeouts**. Wrap every subprocess or local script inside an async timeout block to prevent runaway code loops from hanging.
* **No workflow authority**. Execute only host-issued leases and echo their
  fencing identity in outcomes. Do not select DAG successors, evaluate
  conditions, retry a step, or authorize HITL.
* **Tool telemetry order.** For `tool.exec`, record stdout/stderr logs first (attributes `tool.name` and `lease.id`), then the span, so a FIFO `StreamLogs` flush delivers bodies before the span. Host ingest is observational.
* **Keep assertions read-only**. Verification modules must remain strictly non-mutative and are prohibited from modifying files inside the active workspace directory.
