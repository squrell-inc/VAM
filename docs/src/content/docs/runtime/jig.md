---
title: Jig
description: The closed UI catalog, the IR every chat card is drawn from, and the five built-in jigs.
status: shipped
proves:
  - progress-shows-while-the-task-runs
  - question-waits-for-an-answer-and-resumes
---

The catalog, the IR and the host checker are built. There is no renderer yet (see the planned section at the
end). `post_message`, `show` and `ask_user` (below) are the tools that use the checker.

Proved: a `show` whose data breaks the schema is refused with a typed error
(`progress-shows-while-the-task-runs`), and a `vam.question` answer that breaks the result schema is refused
and the question stays open (`question-waits-for-an-answer-and-resumes`). The IR checker `check_ir` and the
form generator have no test or scenario outside `crates/vam-spec/src/jig/tests.rs`, so they are not proved.
`long-task-posts-progress` posts only plain messages and no card, so it does not exercise the jig and is not cited.

A **jig** is a small, declarative piece of UI made only from a closed catalog of
primitives. The model picks a built-in jig and fills data; it never writes UI.
`vamd` checks the data before anything is drawn and checks every answer before
the task resumes. The design is in `design/jig.md`; the files are in `spec/jig/`
and are embedded in `vam-spec` (`vam_spec::jig`).

## The IR

The checked form of a jig is its **IR**, plain JSON. The app paints only the IR.

```json
{
  "version": 1,
  "catalog": "vam/1",
  "jig": "vam.question",
  "root": "root",
  "state": { "answer": "" },
  "nodes": {
    "root": { "type": "Stack", "children": ["q", "free", "send"] },
    "q": { "type": "Text", "props": { "text": { "$data": "/question" } } },
    "free": { "type": "TextArea", "props": { "label": "Answer", "value": { "$state": "answer" } } },
    "send": { "type": "Button", "props": { "label": "Send", "action": { "event": "submit", "with": ["answer"] } } }
  }
}
```

- `catalog` names the catalog and its version. A host that does not know it
  refuses the IR.
- A prop is a literal or a binding: `{"$data": "/path"}` reads the jig's input
  data, `{"$state": "name"}` reads a state variable. Input elements bind only to
  state.
- A user action is a named event on a `Button`: `submit` ends an ask (the result
  holds every name in `set`, with its literal, and every name in `with`, with
  its current state value; a name in both takes the `set` literal) and `set`
  assigns a state variable. `Query` and `Mutation` are reserved and refused.
- A node may carry `visible` or `hidden`, a binding that shows or hides it by
  whether the bound value is empty.
- The schema is `spec/jig/ir.schema.json` (draft-07); the TypeScript types are
  `spec/jig/ir.d.ts`.

## The catalog (`vam/1`)

Stack, Text, Badge, KeyValue, ProgressBar, Input, TextArea, NumberInput, Toggle,
Select, MultiSelect, DateInput, FileInput and Button. `spec/jig/catalog.json`
lists each one's props in order, their types, the closed style tokens (`gap`,
`tone`) and the allowed actions. There is no free CSS and no HTML.

## The checker

| Function | Checks |
|---|---|
| `check_ir(ir)` | An IR against the catalog: unknown primitive or prop, wrong type, missing prop, bad token, a binding to a missing path or state, an unknown action, `Query` / `Mutation`, children that do not exist, cycles. Every error names a path. |
| `check_data(jig, data)` | The data of `show` / `ask_user` against the built-in's input schema: wrong type, extra field, oversize string, missing field. |
| `check_result(jig, data, result)` | A submitted answer against the result schema and the offered-items rule: a selection or choice can hold only what was offered. |

## Built-in jigs

| Jig | Input | Result |
|---|---|---|
| `vam.approve` | `tool`, `args_summary`, optional `reason` | `decision` (`approve`, `deny`, `always`, `always_deny`), optional `reason` |
| `vam.question` | `question`, optional `choices` (strings) | `answer` (a string; one of `choices` when given) |
| `vam.progress` | `label`, `done`, `total` (integers), optional `message`, optional `card_id` | none |
| `vam.playbook_start` | `playbook`, optional `description`, `inputs_schema` | the playbook's inputs, checked against `inputs_schema` |
| `vam.gate_default` | `question`, `response_schema` | the gate's response, checked against `response_schema` |

`card_id` lets a later `show` with the same id update the card in place. The
IR of `vam.gate_default` and `vam.playbook_start` depends on the schema, so the
file holds a frame and a generator rule: one form element per property
(required ones first, then by name), chosen from the property's type, `enum` and
`format`.

## Posting from a task: `post_message` and `show`

A task can tell the user how it is going while it works.

- `post_message {text}` posts a short plain-text progress message (at most 4 KiB).
  It is always offered. The system prompt asks the model to post one now and then
  on a long task.
- `show {jig, data}` shows a built-in card. The only one built is
  `vam.progress` (`{label, done, total, message?, card_id?}`, at most 16 KiB of
  data). It is offered only when the run turns the harness switch `human.jig` on
  (`harness: {human: {jig: true}}`). A later `show` with the same `card_id`
  updates that card.

Each call passes the argument check and the policy check like any builtin, then
goes to the host through the port-52 method `PostOutbox`. The host does not trust
the guest: it checks that the lease belongs to the run; that the capability token
is the one the policy check returned for exactly these arguments (single use); for
a card that `human.jig` is on, the size caps, that the jig is a built-in Show jig,
and the data with `check_data`; and the rate (60 accepted posts per run per
minute). A failure is a typed tool result and records nothing, for example
`invalid data for vam.progress: /done: ...` or `rate limited: ...`. A post that
passes emits `outbox.message {text}` or `outbox.card {jig, data, card_id?}` on the
run's events. Strings are data and are shown as text only.

**Permission.** A host overlay permits `post_message` and `show`, as it permits
`start_playbook`, so an agent whose tools default to deny keeps both. Two things
remove them: a rule that names the tool exactly (`builtin:post_message` or
`builtin:show` set to deny), and an org or source deny. A wildcard `tools:*` deny
does **not** remove them. Every call still gets its `policy.decided` event, and an
explicit `ask` rule still asks. (An agent's own Cedar `forbid` also wins on the
legacy policy path.)

## Asking the user: `ask_user`

`ask_user {jig, data}` asks the run's owner a question and waits. `vam.question`
(`{question, choices?}`, result `{answer}`) is always offered; the other built-in
Ask jig, `vam.approve`, only when the run has `human.jig` on. It passes the
argument check and the policy check like the other builtins (the same overlay
permits it, so an agent whose tools default to deny keeps it).

The host checks `data` with `check_data` first; bad data is a typed tool result
(`invalid data for vam.question: ...`) and no approval exists. Otherwise it creates an
approval of kind `question` (claims: tool `ask_user`, `requested_args {jig, data}`,
`approval_message` = the question, the jig's result schema as the response schema),
the task suspends exactly as for a tool approval, and the run is `PAUSED` with
`waiting_for: INPUT` (`APPROVAL` for a tool approval, `GATE` for a gate).

The owner answers with `POST /v1/approvals/{id}/decide`, decision `SUBMITTED` and a
`response`. The host refuses an answer over 16 KiB of JSON, then checks it with
`check_result` (an answer must be one of the offered `choices`); a refused answer is
a 4xx and the question stays pending. A valid answer resumes the task (warm or
cold) and is `ask_user`'s tool result. `REJECTED` answers "the user declined to
answer". A question takes no grant: `APPROVED`, `AMENDED` and the Always allow /
Always deny choices are refused, and a second `ask_user` asks again. A steer sent
while a question is pending is held and delivered after the answer. A stop cancels
the question ("stopped by user"). The run receipt lists the answer under `questions`
as a human decision of kind `question`.

A `submit` result is the union of the button action's `set` names (literals) and its
`with` names (state values), so `vam.approve` carries `decision` from `set`. Its
`decision` is `approve`, `deny`, `always` or `always_deny`.

## Planned

These are not built. Queries and mutations, lists and tables, composed components, packages, agent
written `.jig` files, and any A2UI adapter. `spec/jig/a2ui.md` maps the catalog
and IR to A2UI v0.9.1 on paper so the adapter can be written later without
changing the IR.
