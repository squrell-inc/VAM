# VAMD Control Plane API Package

This folder contains the complete **OpenAPI 3.1 Specification** and Developer Guide for interacting with the **Virtual Agent Machine Daemon (`vamd`)**.

---

## 📂 Contents

- [`openapi.yaml`](./openapi.yaml): Full OpenAPI 3.1 specification for all VAMD routes, request/response models, parameter constraints, and example JSON payloads.
- **Interactive UI (when `vamd` is running)**:
  - Documentation Browser: `http://localhost:8080/docs`
  - Raw OpenAPI Spec: `http://localhost:8080/openapi.yaml`

---

## 🧠 Core Architecture & Workflow Mapping

```text
+-------------------+       +-----------------------+
|  Agent (Catalog)  | ----> |   Run Execution       | <----> Workspace Volume (volume://assistant)
|  PUT /v1/agents   |       |   POST /v1/runs       |           └── Thread (threadRef: "thread-1")
+-------------------+       +-----------------------+
                                |               |
                                v               v
                     +------------------+  +--------------------+
                     | MicroVM Session  |  | Run Events Outbox  |
                     | GET /v1/sessions |  | /v1/runs/{urn}/    |
                     +------------------+  |   events?after=0   |
                                           +--------------------+
```

### 1. Agents vs. Sessions vs. Runs vs. Threads
* **Agent (`AgentSpec`)**: Reusable blueprint defining system prompt instructions, model requirements, and tool/skill capabilities. List with `GET /v1/agents` or `vam agent ls`.
* **Workspace Volume (`workspaceRefUri`)**: Isolated storage filesystem volume (`volume://<workspace_id>`).
* **Thread (`threadRef`)**: Persistent conversation branch on the workspace. Subsequent runs with the same `workspaceRefUri` and `threadRef` retain chat history and file modifications.
* **Session (`SessionStatus`)**: MicroVM execution sandbox running the agent process in guest isolation.
* **Run (`RunStatus`)**: A specific agent task execution. Produces real-time events (`/v1/runs/{urn}/events`).

---

## 🚀 Frontend Quickstart

### 1. Generate TypeScript Types
Run in your frontend repository:

```bash
# Generate clean TypeScript interfaces from openapi.yaml
npx openapi-typescript ./openapi.yaml -o src/types/vam-api.ts
```

### 2. Generate Fully Typed Client SDK & React Query Hooks
Using `@hey-api/openapi-ts`:

```bash
npx @hey-api/openapi-ts \
  -i ./openapi.yaml \
  -o src/client \
  -c @hey-api/client-fetch
```

Or using `orval` for React Query:
```bash
npx orval --input ./openapi.yaml --output src/api/generated.ts --client react-query
```

### 3. Mock Server (Develop Without Running Firecracker/VAMD)
You can run a local mock server based on the examples and schemas in `openapi.yaml`:

```bash
npx @stoplight/prism-cli mock ./openapi.yaml -p 8080
```

---

## 🔑 Authentication & Headers

All mutating requests (`POST`, `PUT`, `DELETE`) require an `Idempotency-Key` UUID header to prevent duplicated executions upon network retries:

```http
POST /v1/runs HTTP/1.1
Host: 127.0.0.1:8080
Authorization: Bearer vam_sk_example123456
Idempotency-Key: 9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d
Content-Type: application/json
```

### Example: Triggering an Agent Run

```typescript
import { createClient } from './client';

const response = await fetch('http://127.0.0.1:8080/v1/runs', {
  method: 'POST',
  headers: {
    'Authorization': `Bearer ${process.env.VAM_API_KEY}`,
    'Idempotency-Key': crypto.randomUUID(),
    'Content-Type': 'application/json',
  },
  body: JSON.stringify({
    spec: {
      agentRef: { urn: "urn:vam:agent:hub.vam.dev/acme/sql-developer" },
      workspaceRefUri: "volume://assistant",
      threadRef: "thread-main",
      inputs: {
        prompt: "Analyze the slowest queries in the database.",
        model: "claude-3-5-sonnet",
      },
      maxDuration: { seconds: 300, nanos: 0 },
    },
  }),
});

const data = await response.json();
console.log('Run triggered:', data.status.runId.urn);
```

### Example: Streaming / Polling Thread Events

```typescript
let lastSeq = 0;

async function pollEvents(runUrn: string) {
  const res = await fetch(`http://127.0.0.1:8080/v1/runs/${encodeURIComponent(runUrn)}/events?after=${lastSeq}`, {
    headers: { 'Authorization': `Bearer ${process.env.VAM_API_KEY}` }
  });
  const { events } = await res.json();
  for (const event of events) {
    console.log(`[#${event.sequence}] ${event.eventType}:`, event.payload);
    lastSeq = Math.max(lastSeq, event.sequence);
  }
}
```
