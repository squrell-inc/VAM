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

- **Duplicates:** 0
- **Unknown:** 1
- **Counts:** confirmed 1, reconciled 1, failed 1, not sent 0, identical repeats 1, reads 2

- #1 echo_create_ticket (hsp, non_idempotent): reconciled, attempts 1, checked by verify, key sha256:1111111111111111111111111111111111111111111111111111111111111111, args sha256:2222222222222222222222222222222222222222222222222222222222222222, at 1700000100
- #3 echo_create_note (hsp, non_idempotent): unknown, attempts 1, key sha256:3333333333333333333333333333333333333333333333333333333333333333, args sha256:4444444444444444444444444444444444444444444444444444444444444444, at 1700000101
- #4 echo_create_note (hsp, non_idempotent): failed, attempts 0, key sha256:5555555555555555555555555555555555555555555555555555555555555555, args sha256:4444444444444444444444444444444444444444444444444444444444444444, at 1700000102, call id derived by the host
- #5 echo_create_note (hsp, non_idempotent): confirmed, attempts 1, repeat of #3, parent #1, key sha256:6666666666666666666666666666666666666666666666666666666666666666, args sha256:4444444444444444444444444444444444444444444444444444444444444444, result sha256:7777777777777777777777777777777777777777777777777777777777777777, at 1700000103

_Lists calls the host mediated and journaled. It cannot see traffic a guest process sends directly, or a vendor applying one request twice; the vendor's own log is the independent count._

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
