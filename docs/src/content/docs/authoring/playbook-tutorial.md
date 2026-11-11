---
title: Playbook tutorial
description: Scaffold, explain, test, run and decide a host-tracked playbook from the vam CLI, using the three gate shapes in the examples.
---

This tutorial walks one playbook from an empty file to a decided run, using only the `vam` CLI. It uses three examples in the repository:

| Example | Gate shape | What it shows |
|---|---|---|
| `examples/jira-assistant/` | Selection gate (`choicesFrom`) | The host fills the gate's choices from an earlier step's output |
| `examples/pr-review/` | Approval gate (`approved`, `changes-requested`) | A guard denies the posting tool until a human approves |
| `examples/invoice-approval/` | Branch gate (`pay`, `reject`) | A payment step runs only when the decision is `pay` |

The commands below assume you are in the repository root and have `vamd` running for the commands that call it. `init`, `explain` and the local parts of `test` do not need a running host.

## 1. Scaffold a playbook

```bash
vam playbook init --out my-playbook.json
```

This writes one agentic task step and checks it against the spec before writing. An existing file is not overwritten unless you pass `--force`; the command then exits `5` and leaves the file alone.

## 2. Explain it

```bash
vam playbook explain my-playbook.json
vam playbook explain examples/pr-review/playbook.json
```

`explain` loads the file, migrates and validates it, then prints the steps, gates, guards and the mode counts. The ratio is `(bound + assisted) / task steps`; gates are not counted as task steps. A fresh scaffold has no gate, guard, binding or evidence, so `explain` lists the step-tracking-only warning. Add one of those four features to clear it. `--json` prints the same facts as one object.

An invalid file exits `6` and prints nothing.

## 3. Read the three examples

**Approval gate.** `examples/pr-review/playbook.json` reads a pull request, asks a human whether to post the review, and posts it only if the answer is `approved`:

- `review-change` must make a successful `read_pull_request` call. That call is the step's evidence.
- `approve` is a gate with two options. `changes-requested` is terminal, so choosing it ends the run without posting.
- `post-review` runs only after `approve:approved`. A guard denies `post_review_comment` until that gate is approved.

**Branch gate.** `examples/invoice-approval/playbook.json` extracts an invoice, then asks `pay` or `reject`. `schedule-payment` is `after: ["decide:pay"]`, and a guard denies `schedule_payment` unless `decide:pay` holds.

**Selection gate.** `examples/jira-assistant/playbook.json` pauses before it moves stale issues back to the backlog. Its gate uses `choicesFrom`, so the approver picks from the issues the earlier query returned.

## 4. Test it without a model

```bash
vam playbook test examples/pr-review/playbook.json \
  --scenario examples/pr-review/playbook-scenario.json \
  --repeat 3
```

A scenario is a list of `actions` and an `expect` block. The tool actions are mocked observations: nothing is dispatched to a guest, MCP server or model. The scenario for the pr-review example tries `post_review_comment` once before the gate, which the guard denies, then approves the gate and posts again.

The report is `passed`, `repeat` and `passK = (passed / repeat)^repeat`. All trials passing gives `passK` 1. A failing trial exits `1` and names the assertion that did not hold.

Scenario tool actions use proto side-effect names such as `SIDE_EFFECT_CLASS_READ_ONLY` and `SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL`.

## 5. Publish it and run it

```bash
vam playbook put examples/pr-review/playbook.json
vam run pr-review-assistant --playbook urn:vam:playbook:local.vam.dev/vam/pr-review \
  --input pr_url=https://github.com/example/app/pull/42
```

`put` prints the saved spec on stdout. If the playbook enforces only step tracking, the warning goes to stderr. Publish never rejects a tracking-only playbook.

`vam run` needs the agent to be published first. `examples/jira-assistant/agent.json` is the reference agent: skill URNs, playbook allowlist, and policy refs, with no versions or digests. The concepts those files follow are in [Authoring model](/authoring/model/). The `run.json` files in the examples carry the agent URN and inputs and can be passed with `vam run --file`.

## 6. Decide the gate

When the run reaches the gate it parks. Find the gate's request id:

```bash
vam approve ls
```

The id has the form `<run-id>/gate/<step-id>`. Decide it with the gate's response:

```bash
vam approve decide "<run-id>/gate/approve" --decision submitted --response '{"option": "approved"}'
```

For a branch gate, the response's `option` is one of the declared outcomes. For a gate with a response schema, the response must match that schema; the host rejects a response that does not, and the gate stays open. `--decision rejected` works only when the gate declares a `rejected` outcome.

## 7. Read the receipt

```bash
vam receipt show <run-id>
```

The receipt lists what the host observed: the pins, the model and endpoint, the isolation, each tool call that was denied, asked or gated, each step and gate decision with who decided and from where. The model's own text sits under a separate label at the end and is not verified. See [Run receipt](/security/run-receipt/) to verify it offline.

## Where to go next

- The field reference is in [Playbooks (playbook.json)](/authoring/playbook-spec/).
- Run specs and the `run.json` shape are in [Run Specs (run.json)](/authoring/run-spec/).
