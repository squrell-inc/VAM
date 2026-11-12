# Jig: one UI protocol for agents and the people they work with

Shipped basics only; the larger E11 plan is in the backlog (`.cursor/workstreams/README.md`).

**Status:** the basic version is built (catalog, IR, checker, built-in jigs, `show` and `ask_user`, `List`, `ToolSpec.ui`).
The behaviour is in `docs/src/content/docs/runtime/jig.md`; the files are in `spec/jig/`.

## What shipped

A **jig** is a small, declarative piece of UI made only from a closed catalog of components. The app draws it.
The model picks a jig and fills data; it does not write UI. Two uses are built:

| Use | How it starts | What happens |
|---|---|---|
| **Ask** (human in the loop) | A playbook gate gets a generated default jig, a tool approval uses `vam.approve`, or the agent calls `ask_user(jig, data)` | The task waits. The person's submit is the decision. `vamd` checks it against the result schema and resumes the task. |
| **Show** | The agent calls `show(jig, data)`, or a skill tool names a `ToolSpec.ui.show` card | A card in the chat. Nothing blocks. |

Parts that exist:

- The closed primitive catalog `spec/jig/catalog.json` (`vam/1`): Stack, Text, List, Badge, KeyValue, ProgressBar,
  Input, TextArea, NumberInput, Toggle, Select, MultiSelect, DateInput, FileInput, Button. `List` is display only.
- The jig IR (`spec/jig/ir.schema.json`, `ir.d.ts`) and the host checker: `check_ir`, `check_data`, `check_result`.
- Built-in jigs: `vam.approve`, `vam.question`, `vam.progress`, `vam.playbook_start`, `vam.gate_default`.
- The tools `show` and `ask_user`, and skill cards through `ToolSpec.ui` (`proto/vam/v1/skills.proto`).

## Core rules

1. The primitive catalog is closed. Agents never add to it. No free CSS, no HTML, no script.
2. A checker turns a jig into an IR or a list of errors. The UI paints only the IR. An IR that does not match the
   catalog is not rendered.
3. `Query` and `Mutation` are reserved and refused.
4. `vamd` validates every submit against the result schema, and a selection may only contain items that were
   offered. The app is never trusted for the result. A `submit` result is the union of the button action's `set`
   names (literals) and `with` names (state values), so `vam.approve` carries `decision` from `set`.
5. Strings in data are shown as text only and never interpreted.
6. A click never sends a chat message, and typing stays in the page: only a button that submits reaches the host.
7. Data is checked before anything renders; a bad fill goes back to the model as a typed schema error.

## What we are not building

- Free-form HTML, CSS or script from agents or packages.
- A catalog agents can extend.
- Sending every keystroke into the guest.
- A second form library.
