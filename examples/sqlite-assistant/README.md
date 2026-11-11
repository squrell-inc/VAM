# SQLite Assistant Example for VAM

This example demonstrates an autonomous agent with an **embedded SQLite memory plane** running inside a VAM microVM.

## Architecture

The agent leverages Bun's native `bun:sqlite` engine running in-process to create, populate, and query local structured relational data without any external database server. Workspace file I/O uses **runtime primitives** (`read_file`, `write_file`, etc.).

```text
[ Agent ReAct Loop ] ──► [ db-tools (execute.js) ] ──► [ Local SQLite DB (/tmp/agent_memory.db) ]
         │
         ├──► [ Runtime primitives: read_file, write_file, … ]
         └──► [ Drive9 Workspace (/mnt/drive9/workspace/) ]
```

## Skill tools (SQL domain)

1. `init_db` — Sets up the SQLite database schema on `/tmp/agent_memory.db`.
2. `insert_fact` — Inserts or updates key-value facts categorized by domain.
3. `insert_code_note` — Records code review comments, snippet references, and severity tags.
4. `query_db` — Executes `SELECT` queries and returns JSON rows.

## Runtime primitives (filesystem)

Use the always-on builtins for workspace files and reports: `read_file`, `write_file`, `edit_file`, `glob`, `grep`, `todo_write`, and optionally `run`.
