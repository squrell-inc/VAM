#!/usr/bin/env python3
"""
SQLite DB Tools Skill Executor (Python Implementation)
Executes unprivileged inside the guest VM or host test harness.
Uses Python's standard library sqlite3 for in-process relational database operations.
"""

import sys
import json
import sqlite3
import os

WORKSPACE_DIR = "/mnt/drive9/workspace"
DEFAULT_DB_PATH = "/mnt/drive9/workspace/agent_memory.db"

def resolve_workspace_path(p):
    if not p:
        return WORKSPACE_DIR
    if p.startswith("/mnt/drive9/workspace/"):
        return p
    if p.startswith("/mnt/drive9/"):
        return p
    if p.startswith("/"):
        return p
    if p.startswith("workspace/"):
        return os.path.join(WORKSPACE_DIR, p[len("workspace/"):])
    if p.startswith("./"):
        return os.path.join(WORKSPACE_DIR, p[2:])
    return os.path.join(WORKSPACE_DIR, p)


def get_db(db_path=DEFAULT_DB_PATH):
    full_path = resolve_workspace_path(db_path)
    os.makedirs(os.path.dirname(full_path), exist_ok=True)
    conn = sqlite3.connect(full_path)
    conn.execute("PRAGMA journal_mode = WAL;")
    conn.execute("PRAGMA synchronous = NORMAL;")
    conn.execute("""
        CREATE TABLE IF NOT EXISTS facts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            category TEXT NOT NULL,
            key TEXT UNIQUE NOT NULL,
            value TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );
    """)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS code_notes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            filepath TEXT NOT NULL,
            snippet TEXT,
            analysis TEXT NOT NULL,
            severity TEXT DEFAULT 'info',
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );
    """)
    conn.commit()
    return conn

def close_db(conn):
    try:
        conn.execute("PRAGMA wal_checkpoint(FULL);")
    except Exception:
        pass
    conn.close()
    if os.environ.get("VAM_GUEST_SYNC") == "1":
        try:
            os.sync()
        except Exception:
            pass

def main():
    if len(sys.argv) < 2:
        print(json.dumps({"error": "No action argument provided to tool executor"}), file=sys.stderr)
        sys.exit(1)

    action = sys.argv[1]

    input_data = {}
    try:
        raw_input = sys.stdin.read().strip()
        if raw_input:
            input_data = json.loads(raw_input)
    except Exception as e:
        print(json.dumps({"error": f"Failed to parse JSON input from stdin: {e}"}), file=sys.stderr)
        sys.exit(1)

    try:
        if action == "init_db":
            db_path = input_data.get("db_path", DEFAULT_DB_PATH)
            conn = get_db(db_path)
            close_db(conn)
            print(json.dumps({"success": True, "message": f"Database initialized at {db_path}"}))

        elif action == "insert_fact":
            category = input_data.get("category")
            key = input_data.get("key")
            value = input_data.get("value")
            db_path = input_data.get("db_path", DEFAULT_DB_PATH)

            if not category or not key or value is None:
                raise ValueError("Missing 'category', 'key', or 'value' arguments")

            conn = get_db(db_path)
            conn.execute("""
                INSERT INTO facts (category, key, value)
                VALUES (?, ?, ?)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value, category=excluded.category;
            """, (category, key, str(value)))
            conn.commit()
            close_db(conn)
            print(json.dumps({"success": True, "message": f"Inserted fact '{key}' under category '{category}'"}))

        elif action == "insert_code_note":
            filepath = input_data.get("filepath")
            snippet = input_data.get("snippet", "")
            analysis = input_data.get("analysis")
            severity = input_data.get("severity", "info")
            db_path = input_data.get("db_path", DEFAULT_DB_PATH)

            if not filepath or not analysis:
                raise ValueError("Missing 'filepath' or 'analysis' arguments")

            conn = get_db(db_path)
            conn.execute("""
                INSERT INTO code_notes (filepath, snippet, analysis, severity)
                VALUES (?, ?, ?, ?);
            """, (filepath, snippet, analysis, severity))
            conn.commit()
            close_db(conn)
            print(json.dumps({"success": True, "message": f"Recorded code note for {filepath}"}))

        elif action == "query_db":
            query = input_data.get("query")
            db_path = input_data.get("db_path", DEFAULT_DB_PATH)

            if not query:
                raise ValueError("Missing 'query' argument")

            conn = get_db(db_path)
            conn.row_factory = sqlite3.Row
            cursor = conn.cursor()
            cursor.execute(query)
            rows = [dict(r) for r in cursor.fetchall()]
            close_db(conn)
            print(json.dumps({"success": True, "row_count": len(rows), "rows": rows}))

        elif action == "read_workspace_file":
            filepath = input_data.get("filepath")
            if not filepath:
                raise ValueError("Missing 'filepath' argument")
            full_path = resolve_workspace_path(filepath)
            if not os.path.exists(full_path):
                raise FileNotFoundError(f"File not found: {filepath}")
            with open(full_path, "r", encoding="utf-8") as f:
                content = f.read()
            print(json.dumps({"success": True, "filepath": filepath, "content": content}))

        elif action == "write_report":
            filename = input_data.get("filename")
            content = input_data.get("content")
            if not filename or content is None:
                raise ValueError("Missing 'filename' or 'content' arguments")
            full_path = resolve_workspace_path(filename)
            os.makedirs(os.path.dirname(full_path), exist_ok=True)
            with open(full_path, "w", encoding="utf-8") as f:
                f.write(content)
            if os.environ.get("VAM_GUEST_SYNC") == "1":
                try:
                    os.sync()
                except Exception:
                    pass
            print(json.dumps({"success": True, "message": f"Report written to {filename}"}))

        else:
            print(json.dumps({"error": f"Unknown tool action: {action}"}), file=sys.stderr)
            sys.exit(1)

    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
