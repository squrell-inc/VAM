#!/usr/bin/env bun
/**
 * SQLite DB Tools Skill Executor
 * Executes unprivileged inside the guest VM.
 * Utilizes Bun's built-in SQLite engine (bun:sqlite) for in-process query execution.
 */

import { Database } from "bun:sqlite";
import { readFileSync, writeFileSync, openSync, fsyncSync, closeSync, existsSync, mkdirSync } from "fs";
import { dirname } from "path";


const WORKSPACE_DIR = "/mnt/drive9/workspace";
const DEFAULT_DB_PATH = "/mnt/drive9/workspace/agent_memory.db";

function resolveWorkspacePath(p) {
  if (!p) return WORKSPACE_DIR;
  if (p.startsWith("/mnt/drive9/workspace/")) return p;
  if (p.startsWith("/mnt/drive9/")) return p;
  if (p.startsWith("/")) return p;
  if (p.startsWith("workspace/")) return `${WORKSPACE_DIR}/${p.slice("workspace/".length)}`;
  if (p.startsWith("./")) return `${WORKSPACE_DIR}/${p.slice(2)}`;
  return `${WORKSPACE_DIR}/${p}`;
}


function getDb(dbPath = DEFAULT_DB_PATH) {
  const fullPath = resolveWorkspacePath(dbPath);
  try {
    mkdirSync(dirname(fullPath), { recursive: true });
  } catch (_) {}
  const db = new Database(fullPath, { create: true });
  db.run("PRAGMA journal_mode = WAL;");
  db.run("PRAGMA synchronous = NORMAL;");
  db.run(`
    CREATE TABLE IF NOT EXISTS facts (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      category TEXT NOT NULL,
      key TEXT UNIQUE NOT NULL,
      value TEXT NOT NULL,
      created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    );
  `);
  db.run(`
    CREATE TABLE IF NOT EXISTS code_notes (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      filepath TEXT NOT NULL,
      snippet TEXT,
      analysis TEXT NOT NULL,
      severity TEXT DEFAULT 'info',
      created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    );
  `);
  return db;
}

function closeDb(db) {
  try {
    db.run("PRAGMA wal_checkpoint(FULL);");
  } catch (_) {}
  db.close();
}

async function main() {
  const action = process.argv[2];

  let inputData = {};
  try {
    const stdinContent = readFileSync(0, "utf-8").trim();
    if (stdinContent) {
      inputData = JSON.parse(stdinContent);
    }
  } catch (err) {
    console.error(JSON.stringify({ error: "Failed to parse JSON input from stdin: " + err.message }));
    process.exit(1);
  }

  if (!action) {
    console.error(JSON.stringify({ error: "No action argument provided to tool executor" }));
    process.exit(1);
  }

  try {
    switch (action) {
      case "init_db": {
        const dbPath = inputData.db_path || DEFAULT_DB_PATH;
        const db = getDb(dbPath);
        closeDb(db);
        console.log(JSON.stringify({ success: true, message: `Database initialized at ${dbPath}` }));
        break;
      }

      case "insert_fact": {
        const { category, key, value, db_path } = inputData;
        if (!category || !key || value === undefined) {
          throw new Error("Missing 'category', 'key', or 'value' arguments");
        }
        const db = getDb(db_path);
        const stmt = db.prepare(`
          INSERT INTO facts (category, key, value)
          VALUES (?, ?, ?)
          ON CONFLICT(key) DO UPDATE SET value=excluded.value, category=excluded.category;
        `);
        stmt.run(category, key, String(value));
        closeDb(db);
        console.log(JSON.stringify({ success: true, message: `Inserted fact '${key}' under category '${category}'` }));
        break;
      }

      case "insert_code_note": {
        const { filepath, snippet, analysis, severity, db_path } = inputData;
        if (!filepath || !analysis) {
          throw new Error("Missing 'filepath' or 'analysis' arguments");
        }
        const db = getDb(db_path);
        const stmt = db.prepare(`
          INSERT INTO code_notes (filepath, snippet, analysis, severity)
          VALUES (?, ?, ?, ?);
        `);
        stmt.run(filepath, snippet || "", analysis, severity || "info");
        closeDb(db);
        console.log(JSON.stringify({ success: true, message: `Recorded code note for ${filepath}` }));
        break;
      }

      case "query_db": {
        const { query, db_path } = inputData;
        if (!query) {
          throw new Error("Missing 'query' argument");
        }
        const db = getDb(db_path);
        const rows = db.query(query).all();
        closeDb(db);
        console.log(JSON.stringify({ success: true, row_count: rows.length, rows }));
        break;
      }

      case "read_workspace_file": {
        const { filepath } = inputData;
        if (!filepath) {
          throw new Error("Missing 'filepath' argument");
        }
        const fullPath = resolveWorkspacePath(filepath);
        if (!existsSync(fullPath)) {
          throw new Error(`File not found: ${filepath}`);
        }
        const content = readFileSync(fullPath, "utf-8");
        console.log(JSON.stringify({ success: true, filepath, content }));
        break;
      }

      case "write_report": {
        const { filename, content } = inputData;
        if (!filename || content === undefined) {
          throw new Error("Missing 'filename' or 'content' arguments");
        }
        const fullPath = resolveWorkspacePath(filename);
        try {
          mkdirSync(dirname(fullPath), { recursive: true });
        } catch (_) {}
        const fd = openSync(fullPath, "w");
        writeFileSync(fd, content, "utf-8");
        try {
          fsyncSync(fd);
        } catch (_) {}
        closeSync(fd);
        console.log(JSON.stringify({ success: true, message: `Report written to ${filename}` }));
        break;
      }


      default: {
        console.error(JSON.stringify({ error: `Unknown tool action: ${action}` }));
        process.exit(1);
      }
    }
  } catch (error) {
    console.error(JSON.stringify({ error: error.message }));
    process.exit(1);
  }
}

main();
