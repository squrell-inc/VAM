# Jig IR to A2UI: mapping table

Paper only (owner decision F, AR-3.4). No adapter code exists in Phase 3; the
adapter is E11 Phase 6. This page keeps the IR shaped so that one can be
written without changing it.

**A2UI version: v0.9.1** (current production release; v1.0 is a release
candidate, v0.8 is legacy). Checked 2026-10-27 against `google/A2UI`,
`specification/v0_9_1/docs/a2ui_protocol.md` and
`specification/v0_9_1/catalogs/basic/catalog.json`. Rows marked *unverified*
rest on files not read in that check.

## Concepts

| Jig IR / `show` / `ask_user` | A2UI v0.9.1 | Notes |
|---|---|---|
| `catalog: "vam/1"` | `catalogId` in `createSurface` (fixed for the life of the surface) | Ours is a name and version; A2UI uses a URI-shaped id that need not resolve. The adapter maps `vam/1` to one such URI. |
| A card shown by `show` / asked by `ask_user` | A surface: `createSurface` | One surface per card. |
| Card id (`card_id` in `show` data) | `surfaceId` (required in all four server messages) | A later `show` with the same id is `updateComponents` and `updateDataModel` on that surface, not a new `createSurface`. No id: the host mints one. |
| `nodes` (id to `{type, props, children}`) | `updateComponents`: a flat list of `{id, component, ...props}` | Both are adjacency lists keyed by id. A2UI needs a component with id `root`; the IR's `root` node is emitted under that id. |
| `children: [ids]` | `children` (static array of ids) | Same. A2UI also has template children (`path` + `componentId`); the IR has none until `List` arrives (E11 Phase 1). |
| Input data (`{"$data": "/path"}`) | Data model, bound by JSON Pointer path (`updateDataModel`) | Same pointer syntax. The adapter loads the checked `data` under the surface's data model. |
| State variable (`{"$state": "name"}`) | Data model path `/state/name` | The adapter puts state defaults under a reserved `/state` subtree so they never clash with `data`. |
| `visible` / `hidden` on a node | No direct equivalent in the basic catalog | Adapter: omit the component when the bound value decides it at export time, or map to a catalog function (*unverified*). |
| Action `{event, with, set}` on a Button | A button's `action`; the client answers with `action {name, surfaceId, sourceComponentId, timestamp, context}` | `event` becomes `name`; `context` holds the names in `set` (their literals) and the names in `with` (their state values). A result is the union of both. The declaration shape of `action` on the component lives in `common_types.json` (*unverified*). |
| `submit` event | The `action` message the host receives | The host validates `context` as the result (jig decision 14). |
| `set` event | Client-side data model write (*unverified*) | Runs in the renderer today (jig decision 9). |
| `Query` / `Mutation` | Not mapped | Reserved and refused in Phase 3. |
| `inputSchema` / `resultSchema` | None in A2UI | Stay on our side; the host validates before export and on every answer. |

## Primitives

| Primitive | A2UI basic-catalog component (verified against `catalogs/basic/catalog.json`) | Notes |
|---|---|---|
| Stack (vertical) | `Column` | `gap` has no A2UI prop; the adapter drops it or maps it to spacing in the host app. |
| Stack (horizontal) | `Row` | Same. |
| Text | `Text` (`text`, `variant`) | `emphasis: strong` maps to a heading or body `variant`; `tone` has no equivalent. |
| Badge | `Text` (variant `caption`) | No badge component. The `tone` is lost. |
| KeyValue | `Row` of two `Text` | No key-value component. |
| ProgressBar | None | The basic catalog has no progress component. Needs a custom catalog entry, or `Text` such as "3 of 5". Not exportable as is. |
| Input | `TextField` (`label`, `value`, variant `shortText`) | |
| TextArea | `TextField` (variant `longText`) | |
| NumberInput | `TextField` (variant `number`) | `min` / `max` dropped; `Slider` fits a bounded number. |
| Toggle | `CheckBox` (`label`, `value`) | |
| Select | `ChoicePicker` (variant `mutuallyExclusive`, `options` of `{label, value}`, `value`) | String options are expanded to `{label: s, value: s}`. |
| MultiSelect | `ChoicePicker` (variant `multipleSelection`) | |
| DateInput | `DateTimeInput` (`enableDate: true`) | |
| FileInput | None | A workspace path has no A2UI counterpart. Not exportable. |
| Button | `Button` (`child`, `action`, `variant`) | A2UI takes a child component for the label: the adapter adds a `Text`. `tone: success` maps to `primary`. |

## Import direction

An A2UI payload imports only when every component maps back to a row above. A
`Card`, `Image`, `Tabs`, `Modal`, `Divider`, `Slider`, `Video`, `AudioPlayer`
or `Icon` has no primitive in `vam/1` and is refused with the component's name.
