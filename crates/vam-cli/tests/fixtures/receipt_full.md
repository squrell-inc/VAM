# Run receipt

_Host-observed facts, signed by the node. Verify with `vam receipt verify`._

- **Run:** urn:vam:run:local.vam.dev/acme/demo
- **State:** SUCCEEDED
- **Requested by:** user-1
- **Request surface:** cli
- **Started (unix s):** 1700000000
- **Ended (unix s):** 1700000090

## Pins

- **Agent:** urn:vam:agent:local.vam.dev/acme/closer 1.0.0 sha256:aa
- **Playbook:** urn:vam:playbook:local.vam.dev/acme/close-prep 1.2.0 sha256:bb

## Model and endpoint

- mock-model, provider mock, endpoint broker.local, calls 3, prompt tokens 120, completion tokens 40

## Isolation

process guest, no microVM

## Workspace

- **HEAD before:** h0
- **HEAD after:** h1

## Tool calls

allowed 4, asked 1, denied 1

- #2 close_issue denied, by playbook_guard, scope playbook, rule args_within issue_key in steps.confirm.output.keys, gate step confirm, args sha256:cc
- #3 write_file asked, by cedar_rule, scope agent, rule builtin:write_file, resolution approved-once, approved by user-1, surface cli, approved at 1700000030, args sha256:dd
- #5 close_issue allowed, by cedar_rule, scope agent, rule builtin:close_issue, constraint args_within issue_key in steps.confirm.output.keys, gate step confirm, args sha256:ee

## Procedure

- confirm: completed, outcome decided
  - gate, decision SUBMITTED, decided by user-1, surface cli, at 1700000005, response {"option":"go"}

## Metering

- **Prompt tokens:** 120
- **Completion tokens:** 40
- **Approvals:** 2
- **Turns:** 3

## Effects

effects not journaled on this version

## Signature

- **Trajectory digest:** traj-1
- **Model appendix digest:** sha256:ff
- **Algorithm:** ed25519
- **Key id:** ed25519:0123456789abcdef
- **Receipt digest:** sha256:99

---

## Model appendix

_Written by the model, not verified by the host. Bound to the receipt by digest only._

**Final reply:** Closed the issue.

- confirm: Asked the user.
