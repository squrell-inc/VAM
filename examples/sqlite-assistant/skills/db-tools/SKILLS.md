---
id: "urn:vam:skill:local.vam.dev/vam/db-tools"
name: "SQLite Database Tools"
version: "1.0.0"
description: "Embedded SQLite database tools for structured agent memory and SQL queries."
---

# SQLite Memory & Analysis Skills

This skill provides an embedded in-memory and local SQLite database engine for agents to autonomously record knowledge, maintain structured technical notes, query structured data with SQL, and save technical reports to the workspace.

Extra reference files (for example `SCHEMA.md`) live on the read-only skills mount — use `read_file` / `grep` after loading this skill; they are not auto-injected.
