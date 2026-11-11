#!/usr/bin/env bun
/**
 * Bun Assistant Tools Skill Executor
 * Runs inside the guest microVM under dropped privileges.
 * Receives arguments via stdin as JSON, returns results via stdout as JSON.
 */

import { readFileSync, writeFileSync, readdirSync } from 'fs';

async function main() {
  // Read target action from command line arguments
  const action = process.argv[2];

  // Read inputs from stdin
  let inputData = {};
  try {
    const stdinContent = readFileSync(0, 'utf-8').trim();
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
      case "calculator": {
        const expression = inputData.expression;
        if (!expression) {
          throw new Error("Missing mathematical 'expression' argument");
        }
        // Clean expression and safely evaluate arithmetic
        const cleaned = expression.replace(/[^0-9+\-*/().\s]/g, '');
        const result = Function(`"use strict"; return (${cleaned})`)();
        console.log(JSON.stringify({ result }));
        break;
      }
      case "list_workspace": {
        const files = readdirSync('.');
        console.log(JSON.stringify({ files }));
        break;
      }
      case "write_note": {
        const filename = inputData.filename;
        const content = inputData.content;
        if (!filename || content === undefined) {
          throw new Error("Missing 'filename' or 'content' argument");
        }
        writeFileSync(filename, content, 'utf-8');
        console.log(JSON.stringify({ success: true, message: `Successfully wrote note to ${filename}` }));
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