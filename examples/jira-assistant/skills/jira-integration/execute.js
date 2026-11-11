#!/usr/bin/env bun
/**
 * VAM Jira Integration Tool Executor
 * Runs inside the guest microVM under dropped privileges.
 * Interacts with Jira Cloud REST API v3 via host-brokered egress proxy.
 * Receives arguments via stdin as JSON, returns results via stdout as JSON.
 *
 * Guest never sees the real tenant URL or credential. Requests use scheme-less
 * placeholders (no https://) so Bun cannot treat them as a direct outbound call.
 * The in-guest proxy (unix socket or 127.0.0.1:3128) forwards X-Target-Url to
 * the host egress broker, which substitutes vault secrets.
 */

import { readFileSync } from "fs";
import { connect } from "node:net";

const JIRA_BASE_PLACEHOLDER = "vamsec_jira_base_url";
const JIRA_AUTH_PLACEHOLDER = "vamsec_jira_auth_token";
const PROXY_TCP = process.env.VAM_EGRESS_ENDPOINT || "http://127.0.0.1:3128";
const PROXY_UNIX = "/tmp/vam-egress.sock";

async function main() {
  const action = process.argv[2];

  let inputData = {};
  try {
    const stdinContent = readFileSync(0, "utf-8").trim();
    if (stdinContent) {
      inputData = JSON.parse(stdinContent);
    }
  } catch (err) {
    console.error(
      JSON.stringify({
        error: "Failed to parse JSON input from stdin: " + err.message,
      }),
    );
    process.exit(1);
  }

  if (!action) {
    console.error(
      JSON.stringify({ error: "No action argument provided to tool executor" }),
    );
    process.exit(1);
  }

  switch (action) {
    case "search":
      await handleSearch(inputData);
      break;
    case "create":
      await handleCreate(inputData);
      break;
    case "transition":
      await handleTransition(inputData);
      break;
    case "comment":
      await handleComment(inputData);
      break;
    default:
      console.error(JSON.stringify({ error: `Unknown action: ${action}` }));
      process.exit(1);
  }
}

/** Join a scheme-less placeholder host with a Jira REST path. Never emit https://. */
function jiraTarget(pathAndQuery) {
  const path = pathAndQuery.startsWith("/") ? pathAndQuery : `/${pathAndQuery}`;
  return `${JIRA_BASE_PLACEHOLDER}${path}`;
}

async function jiraFetch(pathAndQuery, options = {}) {
  const target = jiraTarget(pathAndQuery);
  const proxy = new URL(PROXY_TCP);
  const path = proxy.pathname === "/" ? pathAndQuery : `${proxy.pathname}${pathAndQuery}`;
  const headers = {
    Authorization: JIRA_AUTH_PLACEHOLDER,
    Accept: "application/json",
    "Content-Type": "application/json",
    "X-Target-Url": target,
    ...(options.headers || {}),
  };

  try {
    const response = await httpViaGuestProxy(
      options.method || "GET",
      proxy.hostname,
      Number(proxy.port || 80),
      path.startsWith("/") ? path : `/${path}`,
      headers,
      options.body,
    );
    let data;
    try {
      data = response.text ? JSON.parse(response.text) : {};
    } catch {
      data = { raw: response.text };
    }
    return { ok: response.ok, status: response.status, data };
  } catch (err) {
    return { ok: false, status: 0, error: err.message };
  }
}

function httpViaGuestProxy(method, host, port, path, headers, body) {
  return new Promise((resolve, reject) => {
    const send = (socket) => {
      const bodyBuf = body ? Buffer.from(String(body)) : Buffer.alloc(0);
      const hdrs = {
        ...headers,
        Host: `${host}:${port}`,
        Connection: "close",
      };
      if (bodyBuf.length) hdrs["Content-Length"] = String(bodyBuf.length);
      let req = `${method} ${path} HTTP/1.1\r\n`;
      for (const [k, v] of Object.entries(hdrs)) {
        if (v == null || v === "") continue;
        req += `${k}: ${v}\r\n`;
      }
      req += `\r\n`;
      socket.write(req);
      if (bodyBuf.length) socket.write(bodyBuf);
      const chunks = [];
      socket.on("data", (c) => chunks.push(c));
      socket.on("end", () => {
        const raw = Buffer.concat(chunks).toString("utf8");
        const sep = raw.indexOf("\r\n\r\n");
        const head = sep >= 0 ? raw.slice(0, sep) : raw;
        const text = sep >= 0 ? raw.slice(sep + 4) : "";
        const status =
          parseInt((head.split("\r\n")[0] || "").split(" ")[1], 10) || 0;
        resolve({ status, ok: status >= 200 && status < 300, text });
      });
      socket.on("error", reject);
      socket.setTimeout(20000, () => {
        socket.destroy();
        reject(new Error("egress proxy socket timeout"));
      });
    };

    const unix = connect({ path: PROXY_UNIX });
    unix.once("connect", () => send(unix));
    unix.once("error", () => {
      const tcp = connect({ host, port });
      tcp.once("connect", () => send(tcp));
      tcp.once("error", reject);
    });
  });
}

function extractAdfText(node) {
  if (!node) return "";
  if (typeof node === "string") return node;
  if (node.text) return node.text;
  if (Array.isArray(node.content)) {
    return node.content.map(extractAdfText).filter(Boolean).join(" ");
  }
  if (typeof node === "object") {
    let acc = [];
    for (const key of Object.keys(node)) {
      if (key === "content" && Array.isArray(node[key])) {
        acc.push(node[key].map(extractAdfText).filter(Boolean).join(" "));
      } else if (typeof node[key] === "object") {
        const sub = extractAdfText(node[key]);
        if (sub) acc.push(sub);
      }
    }
    return acc.join(" ");
  }
  return "";
}

async function handleSearch(input) {
  const { jql, max_results = 50 } = input;
  if (!jql) {
    console.log(
      JSON.stringify({ success: false, error: "JQL query is required" }),
    );
    return;
  }

  const requestedFields =
    "id,key,summary,status,assignee,updated,description,priority,issuetype,project";

  let path = `/rest/api/3/search/jql?jql=${encodeURIComponent(jql)}&maxResults=${max_results}&fields=${encodeURIComponent(requestedFields)}`;
  let res = await jiraFetch(path, { method: "GET" });

  if (!res.ok && (res.status === 404 || res.status === 405)) {
    path = `/rest/api/3/search?jql=${encodeURIComponent(jql)}&maxResults=${max_results}&fields=${encodeURIComponent(requestedFields)}`;
    res = await jiraFetch(path, { method: "GET" });
  }

  if (res.ok) {
    const rawIssues = res.data.issues || [];
    const issues = await Promise.all(
      rawIssues.map(async (issue) => {
        let f = issue.fields;
        let key = issue.key;
        if (!f && (issue.id || issue.key)) {
          const issueRes = await jiraFetch(
            `/rest/api/3/issue/${issue.id || issue.key}`,
            { method: "GET" },
          );
          if (issueRes.ok && issueRes.data) {
            f = issueRes.data.fields || {};
            key = issueRes.data.key || key;
          }
        }
        f = f || {};
        return {
          id: issue.id,
          key: key || issue.id,
          summary: f.summary || "",
          status: f.status?.name || "",
          issue_type: f.issuetype?.name || "",
          priority: f.priority?.name || "",
          assignee: f.assignee?.displayName || null,
          updated: f.updated || "",
          description: extractAdfText(f.description),
        };
      }),
    );

    console.log(
      JSON.stringify(
        {
          success: true,
          jql,
          total: res.data.total ?? issues.length,
          max_results,
          issues,
        },
        null,
        2,
      ),
    );
  } else {
    console.log(
      JSON.stringify({
        success: false,
        status: res.status,
        error:
          res.data?.errorMessages ||
          res.data?.errors ||
          res.error ||
          res.data ||
          "Jira search request failed",
      }),
    );
  }
}

async function handleCreate(input) {
  const { project_key, summary, description, issue_type = "Task", assignee_id } =
    input;
  if (!project_key || !summary || !description) {
    console.log(
      JSON.stringify({
        success: false,
        error:
          "Missing required fields (project_key, summary, description) for ticket creation",
      }),
    );
    return;
  }

  const bodyPayload = {
    fields: {
      project: { key: project_key },
      summary: summary,
      description: {
        type: "doc",
        version: 1,
        content: [
          {
            type: "paragraph",
            content: [{ type: "text", text: description }],
          },
        ],
      },
      issuetype: { name: issue_type },
    },
  };
  if (assignee_id) {
    bodyPayload.fields.assignee = { accountId: assignee_id };
  }

  const res = await jiraFetch("/rest/api/3/issue", {
    method: "POST",
    body: JSON.stringify(bodyPayload),
  });

  if (res.ok) {
    console.log(
      JSON.stringify({
        success: true,
        issue_key: res.data.key,
        id: res.data.id,
        self: res.data.self,
        summary,
        issue_type,
      }),
    );
  } else {
    console.log(
      JSON.stringify({
        success: false,
        status: res.status,
        error:
          res.data?.errorMessages ||
          res.data?.errors ||
          res.error ||
          "Jira issue creation failed",
      }),
    );
  }
}

async function handleTransition(input) {
  const { issue_key, transition_id } = input;
  if (!issue_key || !transition_id) {
    console.log(
      JSON.stringify({
        success: false,
        error: "Missing issue_key or transition_id",
      }),
    );
    return;
  }

  let targetTransitionId = transition_id;
  const transitionsPath = `/rest/api/3/issue/${encodeURIComponent(issue_key)}/transitions`;
  const listRes = await jiraFetch(transitionsPath, { method: "GET" });

  if (listRes.ok && Array.isArray(listRes.data?.transitions)) {
    const matched = listRes.data.transitions.find(
      (t) =>
        t.id === transition_id ||
        (t.name && t.name.toLowerCase() === transition_id.toLowerCase()) ||
        (t.to?.name && t.to.name.toLowerCase() === transition_id.toLowerCase()),
    );
    if (matched) {
      targetTransitionId = matched.id;
    }
  }

  const res = await jiraFetch(transitionsPath, {
    method: "POST",
    body: JSON.stringify({ transition: { id: targetTransitionId } }),
  });

  if (res.ok || res.status === 204) {
    console.log(
      JSON.stringify({
        success: true,
        issue_key,
        transitioned_to: targetTransitionId,
        timestamp: new Date().toISOString(),
      }),
    );
  } else {
    console.log(
      JSON.stringify({
        success: false,
        status: res.status,
        error:
          res.data?.errorMessages ||
          res.data?.errors ||
          res.error ||
          "Jira transition request failed",
      }),
    );
  }
}

async function handleComment(input) {
  const { issue_key, body } = input;
  if (!issue_key || !body) {
    console.log(
      JSON.stringify({
        success: false,
        error: "Missing issue_key or comment body",
      }),
    );
    return;
  }

  const bodyPayload = {
    body: {
      type: "doc",
      version: 1,
      content: [
        {
          type: "paragraph",
          content: [{ type: "text", text: body }],
        },
      ],
    },
  };

  const res = await jiraFetch(
    `/rest/api/3/issue/${encodeURIComponent(issue_key)}/comment`,
    {
      method: "POST",
      body: JSON.stringify(bodyPayload),
    },
  );

  if (res.ok || res.status === 201) {
    console.log(
      JSON.stringify({
        success: true,
        issue_key,
        comment_id: res.data?.id,
        body,
        timestamp: new Date().toISOString(),
      }),
    );
  } else {
    console.log(
      JSON.stringify({
        success: false,
        status: res.status,
        error:
          res.data?.errorMessages ||
          res.data?.errors ||
          res.error ||
          "Jira comment request failed",
      }),
    );
  }
}

main().catch((err) => {
  console.error(JSON.stringify({ error: err.stack }));
  process.exit(1);
});
