---
title: Run Receipt
description: The signed record of what the host observed during a run, how to verify it offline, and the node signing keys.
status: shipped
proves:
  - receipt-separates-host-facts-from-model-text
  - receipt-verifies-offline-and-fails-on-tamper
  - old-receipt-still-verifies
  - receipt-lists-each-external-effect-once
  - receipt-counts-effects-after-a-crash
  - receipt-shows-an-effect-settled-by-a-check
  - receipt-counts-an-unknown-effect-it-could-not-check
  - receipt-names-the-isolation-the-run-had
  - node-reports-the-isolation-it-runs
  - crates/vam-cli/tests/receipt_cli.rs
  - crates/vam-orchestrator/tests/node_signing_keys_tests.rs
---

A run receipt is one signed JSON document the node issues when a run reaches a
terminal state. It records what the host observed. It does not certify
compliance with any standard, and it does not vouch for what the model wrote.

## Two tiers

* **Host tier (`receipt`).** Facts the host observed: pins (agent, playbook,
  other versions with digests), the model and broker endpoint host, the
  isolation the run had, workspace HEAD before and after, every denied or asked
  tool call with its deciding rule and scope, who decided each approval or gate
  and from which surface, the run's harness (profile, version, digest and the
  switches the caller changed), token metering, and the trajectory digest. Tool
  arguments are never copied, only their digest and side-effect class.
* **Model appendix (`modelAppendix`).** Text the model wrote: its final reply and
  step summaries. It is labelled "written by the model, not verified by the
  host". The host tier carries only its digest (`modelAppendixDigest`).

The signature covers the host tier, and through the digest it binds the appendix.

Tool calls that a playbook guard constrained on a gate (`args_within` or
`allowed_when`) name the gate in `gateStep`, whether the call was allowed or
denied. Effects come from the host's effect journal (see below).

## Effects

The `effects` object is built from the run's effect journal, the host's record of
every call that can change something outside the machine. It lists what the host
mediated and journaled. It does not see traffic a guest process sends directly, or
a vendor applying one request twice, and its `scope` sentence says so. The vendor's
own log stays the independent count.

```json
"effects": {
  "journaled": true,
  "scope": "Lists calls the host mediated and journaled. It cannot see traffic a guest process sends directly, or a vendor applying one request twice; the vendor's own log is the independent count.",
  "duplicates": 0,
  "unknown": 0,
  "counts": {"confirmed": 1, "failed": 0, "reconciled": 0, "notSent": 0, "identicalRepeats": 0, "reads": 1},
  "lines": [
    {"seq": 1, "tool": "echo_create_ticket", "adapter": "hsp", "class": "non_idempotent",
     "state": "confirmed", "attempts": 1,
     "keyDigest": "sha256:...", "argsDigest": "sha256:...", "resultDigest": "sha256:...", "at": 1700000100}
  ]
}
```

* **Lines.** One line per journal row that is not a read, oldest first, so an
  idempotent write is listed too. Reads (a verify read included) are only counted
  in `counts.reads`. A file written in the VM's own workspace is a turn commit, not
  a journal row, and is not listed. A line has no call id, no detail text, and no
  raw argument or result: `keyDigest` is the SHA-256 of the idempotency key, so a
  vendor log can be matched without printing the key. `seq` is the row's 1-based
  position in the journal (reads included), `parentSeq` names the calling step of a
  nested call, and `hostDerivedId: true` marks a call whose id the host made up
  because the guest sent none.
* **States.** `confirmed`; `reconciled` (confirmed by a check, `checkedBy:
  verify`); `failed` (a denial shows `attempts: 0`, it never left); `unknown` (the
  outcome is not known, and also a call recorded as sent with no answer when the run
  ended); `notSent` (decided and recorded, never left the host). A check that found
  nothing leaves the line `unknown` with `checkedBy: verify`. The counts are
  disjoint: `confirmed` does not include `reconciled`.
* **`duplicates`.** Sends beyond the first of one decided non-idempotent call (the
  sum of `attempts - 1`). The host re-sends only reads and idempotent calls, so this
  is 0 by construction; it is stated so it can be seen. An idempotent re-send under
  one key is not a duplicate.
* **`counts.identicalRepeats`.** Non-idempotent calls sent with the same tool and
  arguments digest as an earlier sent, not-failed call of the run (the model asked
  again, or a person allowed it after an unknown outcome). Each such line names the
  earlier one in `repeatOf`.
* **`unknown`.** Calls whose outcome was never settled. They are counted, never
  hidden.

A run with no journaled calls says `journaled: true` with zeros. A run created
before the node first started with this version keeps the earlier object,
`{"journaled": false, "statement": "effects not journaled on this version"}`; it
never claims a journal it cannot show. A receipt stored before this section existed
is never rebuilt and still verifies, because the signature covers the JSON as
stored. `vam receipt show` prints the headline numbers, the counts, one line per
effect and the scope sentence, or the old statement for an old receipt.

## The harness line

`receipt.harness` names the harness the run used: `profile` (`base`), `version`,
`digest`, and `overrides` when the caller changed a switch (for example
`{"loop": {"detect": true}}`; absent when none). The digest is the SHA-256 of the
canonical form of the profile, version and the full switch tree, which stays on
the run record, so a changed switch changes the digest. `vam receipt show` prints
`Harness: base v0.1.0 (overrides: loop.detect=true)`. The field is signed like the
rest of the host tier; a receipt issued before this field existed has no
`harness` key and still verifies.

## Show a receipt

```bash
vam receipt show <run-id>          # markdown
vam receipt show <run-id> --json   # the signed document, as stored
```

The receipt exists once the run has ended. While the run is live or parked, the
API answers 409. `vam evidence receipt` is an alias of `vam receipt show` and is
removed after 0.1.0.

## Verify a receipt offline

```bash
vam receipt show <run-id> --json > receipt.json
vam receipt verify receipt.json
```

`verify` needs no daemon, host or token. It checks the Ed25519 signature over
the host tier, that the appendix matches its digest, and prints the key id. Exit
code 0 means it passed; any change to the host tier or appendix fails with
exit code 9 and a message naming the signature.

## Trust

Without more input, `verify` checks the public key embedded in the receipt. That
proves the document is unchanged since that key signed it; it does not prove the
key belongs to your node, so the output says "key not pinned". To pin, save the
node's keys and pass them:

```bash
vam node keys > node-keys.json
vam receipt verify receipt.json --trusted-keys node-keys.json
```

With `--trusted-keys`, a receipt signed by any other key fails. Retired keys
stay listed by `vam node keys`, so receipts signed before a rotation still
verify.

## In the evidence pack

`vam evidence export <run-id> --out <dir>` includes `receipt.json` byte for byte
and, when the run has one, `attestation.envelope.json` (the signed attestation).
`vam evidence verify <dir>` verifies the receipt signature, that its run id and
trajectory digest match the pack, and that the attestation was signed by the
same node key. Attestations signed before the node key was introduced are
reported as "unverifiable" and do not fail the pack, and packs without a receipt
still verify.

## Node signing keys

Two secret files live in `<state dir>/signing/`, created on first start with mode 0600 (the
directory 0700) and read back on every later start (`crates/vam-orchestrator/src/signing.rs:1-20`):

* `node-ed25519.key` is the Ed25519 seed that signs receipts and run attestations. The key id is
  `ed25519:` plus the first 16 hex characters of the SHA-256 of the public key.
* `session-master.key` holds 32 random bytes. Each session's own Ed25519 key is derived from it with
  HMAC-SHA256. That key signs capability tokens and the audit ledger, so rotating the receipt key does
  not touch session tokens.

`vam node keys` prints the public keys only. There is no built-in or fallback key: with nothing
installed, the session does not start. To rotate, move `node-ed25519.key` into `signing/retired/`
and restart; the retired public half stays listed, so older receipts still verify by key id.

Proved by `crates/vam-orchestrator/tests/node_signing_keys_tests.rs` (the node status lists public
signing keys and never a secret).

## Proved by

| Claim | Proof |
|---|---|
| The host tier is kept apart from model text | `receipt-separates-host-facts-from-model-text` |
| A receipt verifies offline and a change fails | `receipt-verifies-offline-and-fails-on-tamper`, `crates/vam-cli/tests/receipt_cli.rs` |
| The receipt names the isolation the run had | `receipt-names-the-isolation-the-run-had` (microVM lane) |
| Public keys only are published | `crates/vam-orchestrator/tests/node_signing_keys_tests.rs` |

## Not built yet

Receipts aggregated per agent version and a release diff are planned.
