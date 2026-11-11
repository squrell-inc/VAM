#!/usr/bin/env python3
"""
Chaos Tools Skill Executor (Python Implementation)
Executes unprivileged inside the guest VM or test harness.
"""

import sys
import json
import os
import time

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
        if action == "sleep_hang":
            seconds = input_data.get("seconds", 3600)
            time.sleep(seconds)
            print(json.dumps({"success": True, "message": f"Finished sleeping for {seconds}s"}))

        elif action == "probe_sandbox":
            target_path = input_data.get("target_path", "/etc/shadow")
            uid = os.getuid() if hasattr(os, "getuid") else -1
            gid = os.getgid() if hasattr(os, "getgid") else -1

            path_accessible = False
            read_error = None
            try:
                with open(target_path, "r") as f:
                    _ = f.read(10)
                path_accessible = True
            except Exception as e:
                read_error = str(e)

            print(json.dumps({
                "success": True,
                "uid": uid,
                "gid": gid,
                "is_unprivileged": uid != 0,
                "target_path": target_path,
                "path_accessible": path_accessible,
                "error": read_error
            }))

        elif action == "check_env":
            env_keys = list(os.environ.keys())
            forbidden_patterns = ["SECRET", "KEY", "TOKEN", "VSOCK", "CID", "FIRECRACKER"]
            leaked_keys = [
                k for k in env_keys
                if any(p in k.upper() for p in forbidden_patterns)
            ]
            print(json.dumps({
                "success": True,
                "total_env_vars": len(env_keys),
                "env_keys": env_keys,
                "leaked_keys": leaked_keys,
                "is_clean": len(leaked_keys) == 0
            }))

        else:
            print(json.dumps({"error": f"Unknown tool action: {action}"}), file=sys.stderr)
            sys.exit(1)

    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
