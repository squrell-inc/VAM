#!/usr/bin/env bun
/**
 * Chaos Tools Skill Executor
 * Designed to test timeouts, privilege containment, and environment scrubbing.
 */

import { readFileSync } from "fs";

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
      case "sleep_hang": {
        const seconds = inputData.seconds || 3600;
        // Sleep indefinitely / for requested seconds
        await new Promise((resolve) => setTimeout(resolve, seconds * 1000));
        console.log(JSON.stringify({ success: true, message: `Finished sleeping for ${seconds}s` }));
        break;
      }

      case "probe_sandbox": {
        const targetPath = inputData.target_path || "/etc/shadow";
        const uid = typeof process.getuid === "function" ? process.getuid() : -1;
        const gid = typeof process.getgid === "function" ? process.getgid() : -1;

        let pathAccessible = false;
        let readError = null;

        try {
          readFileSync(targetPath);
          pathAccessible = true;
        } catch (e) {
          readError = e.message;
        }

        console.log(JSON.stringify({
          success: true,
          uid,
          gid,
          is_unprivileged: uid !== 0,
          target_path: targetPath,
          path_accessible: pathAccessible,
          error: readError
        }));
        break;
      }

      case "check_env": {
        const envKeys = Object.keys(process.env);
        const forbiddenPatterns = ["SECRET", "KEY", "TOKEN", "VSOCK", "CID", "FIRECRACKER"];
        const leakedKeys = envKeys.filter((k) =>
          forbiddenPatterns.some((pattern) => k.toUpperCase().includes(pattern))
        );

        console.log(JSON.stringify({
          success: true,
          total_env_vars: envKeys.length,
          env_keys: envKeys,
          leaked_keys: leakedKeys,
          is_clean: leakedKeys.length === 0
        }));
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
