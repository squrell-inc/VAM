---
title: Tool Runtime Templates
description: Production-ready tool executor boilerplate and templates for Bun, Node.js, Python, and Bash in VAM.
---

VAM is language-agnostic for guest tools. Any interpreter available inside the guest microVM image (`bun`, `node`, `python3`, or `bash`) can be used as an executor runner.

This page provides copy-pasteable boilerplates and patterns for each supported runtime.

---

## 1. Bun / TypeScript (`execute.js`)

[Bun](https://bun.sh) is the default runtime used in VAM examples because of its fast startup time and built-in TypeScript support.

```javascript
#!/usr/bin/env bun
/**
 * VAM Tool Executor (Bun / TypeScript)
 * Reads JSON arguments from stdin, performs action, and writes JSON to stdout.
 */
import { readFileSync } from "fs";

async function main() {
  const action = process.argv[2];

  // 1. Parse JSON input from stdin
  let inputData = {};
  try {
    const rawInput = readFileSync(0, "utf-8").trim();
    if (rawInput) {
      inputData = JSON.parse(rawInput);
    }
  } catch (err) {
    console.error(JSON.stringify({ error: `Invalid JSON on stdin: ${err.message}` }));
    process.exit(1);
  }

  // 2. Dispatch action
  switch (action) {
    case "search":
      await handleSearch(inputData);
      break;
    case "transition":
      await handleTransition(inputData);
      break;
    default:
      console.error(JSON.stringify({ error: `Unknown action: ${action}` }));
      process.exit(1);
  }
}

async function handleSearch(input) {
  const { query, limit = 10 } = input;
  if (!query) {
    console.error(JSON.stringify({ error: "Missing required field: 'query'" }));
    process.exit(1);
  }

  // Mock search results for Acme project
  const results = {
    total: 1,
    items: [
      { id: "VAM-123", title: "Setup Zero-Trust Proxy", status: "In Progress" }
    ]
  };

  // Write JSON to stdout
  console.log(JSON.stringify(results));
  process.exit(0);
}

async function handleTransition(input) {
  const { issue_key, transition_id } = input;
  if (!issue_key || !transition_id) {
    console.error(JSON.stringify({ error: "Missing 'issue_key' or 'transition_id'" }));
    process.exit(1);
  }

  console.log(JSON.stringify({
    success: true,
    issue_key,
    new_status: transition_id,
    updated_at: new Date().toISOString()
  }));
  process.exit(0);
}

main();
```

---

## 2. Node.js (`execute.mjs` / CommonJS)

Standard Node.js implementation without external dependencies:

```javascript
#!/usr/bin/env node
/**
 * VAM Tool Executor (Node.js ESM)
 */
import fs from "node:fs";

async function main() {
  const action = process.argv[2];

  let input = {};
  try {
    const stdinContent = fs.readFileSync(0, "utf-8").trim();
    if (stdinContent) {
      input = JSON.parse(stdinContent);
    }
  } catch (err) {
    console.error(JSON.stringify({ error: `JSON parse error: ${err.message}` }));
    process.exit(1);
  }

  if (action === "inspect_file") {
    const { path } = input;
    if (!path) {
      console.error(JSON.stringify({ error: "Field 'path' is required" }));
      process.exit(1);
    }

    try {
      const stats = fs.statSync(path);
      console.log(JSON.stringify({
        path,
        size_bytes: stats.size,
        is_directory: stats.isDirectory()
      }));
      process.exit(0);
    } catch (err) {
      console.error(JSON.stringify({ error: `File access error: ${err.message}` }));
      process.exit(1);
    }
  } else {
    console.error(JSON.stringify({ error: `Unsupported action: ${action}` }));
    process.exit(1);
  }
}

main();
```

---

## 3. Python 3 (`execute.py`)

Python implementation using `sys.stdin`, `sys.stdout`, and `json`:

```python
#!/usr/bin/env python3
"""
VAM Tool Executor (Python 3)
"""
import sys
import json
from datetime import datetime

def main():
    if len(sys.argv) < 2:
        sys.stderr.write(json.dumps({"error": "No action argument provided"}))
        sys.exit(1)
        
    action = sys.argv[1]
    
    # 1. Parse JSON input from stdin
    input_data = {}
    try:
        raw_stdin = sys.stdin.read().strip()
        if raw_stdin:
            input_data = json.loads(raw_stdin)
    except json.JSONDecodeError as err:
        sys.stderr.write(json.dumps({"error": f"Failed to parse stdin JSON: {str(err)}"}))
        sys.exit(1)
        
    # 2. Dispatch
    if action == "query_metrics":
        handle_query(input_data)
    else:
        sys.stderr.write(json.dumps({"error": f"Unknown action: {action}"}))
        sys.exit(1)

def handle_query(params):
    metric_name = params.get("metric_name")
    if not metric_name:
        sys.stderr.write(json.dumps({"error": "Parameter 'metric_name' is required"}))
        sys.exit(1)
        
    response = {
        "metric": metric_name,
        "timestamp": datetime.utcnow().isoformat() + "Z",
        "datapoints": [
            {"time": "2026-08-22T05:00:00Z", "value": 42.0},
            {"time": "2026-08-22T05:05:00Z", "value": 45.2}
        ]
    }
    
    # Output JSON to stdout
    sys.stdout.write(json.dumps(response))
    sys.exit(0)

if __name__ == "__main__":
    main()
```

---

## 4. Bash / Shell (`execute.sh`)

Shell scripts can be used for tools that wrap CLI utilities like `jq`, `grep`, or `git`:

```bash
#!/usr/bin/env bash
# VAM Tool Executor (Bash + jq)
set -euo pipefail

ACTION="${1:-}"

if [[ -z "$ACTION" ]]; then
  echo '{"error": "No action argument specified"}' >&2
  exit 1
fi

# Read stdin into a variable
STDIN_INPUT=$(cat)

case "$ACTION" in
  count_lines)
    FILE_PATH=$(echo "$STDIN_INPUT" | jq -r '.path // empty')
    if [[ -z "$FILE_PATH" || ! -f "$FILE_PATH" ]]; then
      echo '{"error": "File does not exist or invalid path"}' >&2
      exit 1
    fi

    TOTAL_LINES=$(wc -l < "$FILE_PATH" | tr -d ' ')
    echo "{\"path\": \"$FILE_PATH\", \"line_count\": $TOTAL_LINES}"
    exit 0
    ;;

  *)
    echo "{\"error\": \"Unknown action: $ACTION\"}" >&2
    exit 1
    ;;
esac
```

---

## `SKILL.md` Binding Examples

To bind any of these templates in your `SKILL.md`:

```yaml
# For Bun:
exec:
  command: "bun"
  entrypoint: "./execute.js"
  default_args: ["search"]

# For Python:
exec:
  command: "python3"
  entrypoint: "./execute.py"
  default_args: ["query_metrics"]

# For Shell:
exec:
  command: "bash"
  entrypoint: "./execute.sh"
  default_args: ["count_lines"]
```

---

## Next Steps

- Learn how to connect tools to external APIs in [Zero-Trust Egress Proxy Deep-Dive](/integrations/egress-proxy-deepdive/).
- Walk through the real-world Jira integration in [Jira Assistant Walkthrough](/integrations/jira-assistant-walkthrough/).
