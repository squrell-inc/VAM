# Chaos & Boundary Testing Agent for VAM

This example agent tests the hardening boundaries of the VAM guest execution environment.

## Verified Security Invariants

1. **Unprivileged Execution (`uid=1000`)**: Tool processes cannot read privileged system resources (`/etc/shadow`, `/root`).
2. **Environment Scrubbing**: Environment variables passed to tool subprocesses are sanitized to prevent secret or CID exposure.
3. **Escalating Termination Timeouts**: Tools exceeding their time limit are terminated predictably (`SIGTERM` $\to$ `SIGKILL`).
