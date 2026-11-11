/**
 * Hermetic integration test: in-process client Register + Invoke over a temp UDS.
 * Does not require vamd.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as grpc from "@grpc/grpc-js";
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  BROKER_ID,
  HOOK_DISPOSITION_OK,
  TRUST_TIER_FIRST_PARTY,
  loadHostedBrokerPackage,
  parseW3CTraceparent,
  redactHspRequest,
  startEchoBroker,
  stopEchoBroker,
} from "./index";

function resolveProtoRoot(): string {
  const candidates = [
    path.resolve(__dirname, "../../../proto"),
    path.resolve(process.cwd(), "proto"),
    path.resolve(process.cwd(), "../../proto"),
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(path.join(candidate, "vam/v1/hosted_services.proto"))) {
      return candidate;
    }
  }
  throw new Error("proto/ not found for test");
}

type HostedBrokerClient = {
  register: (
    req: unknown,
    cb: (err: grpc.ServiceError | null, res: Record<string, unknown>) => void,
  ) => void;
  invoke: (
    req: unknown,
    cb: (err: grpc.ServiceError | null, res: Record<string, unknown>) => void,
  ) => void;
  listTools: (
    req: unknown,
    cb: (err: grpc.ServiceError | null, res: Record<string, unknown>) => void,
  ) => void;
  onRequest: (
    req: unknown,
    cb: (err: grpc.ServiceError | null, res: Record<string, unknown>) => void,
  ) => void;
  onComplete: (
    req: unknown,
    cb: (err: grpc.ServiceError | null, res: Record<string, unknown>) => void,
  ) => void;
  onResponse: (
    req: unknown,
    cb: (err: grpc.ServiceError | null, res: Record<string, unknown>) => void,
  ) => void;
  close: () => void;
};

function connectClient(socketPath: string, protoRoot: string): HostedBrokerClient {
  const packageDefinition = loadHostedBrokerPackage(protoRoot);
  const proto = grpc.loadPackageDefinition(packageDefinition) as unknown as {
    vam: {
      v1: {
        HostedBroker: new (
          address: string,
          creds: grpc.ChannelCredentials,
        ) => HostedBrokerClient;
      };
    };
  };
  return new proto.vam.v1.HostedBroker(
    `unix:${socketPath}`,
    grpc.credentials.createInsecure(),
  );
}

function unary<T>(
  fn: (
    req: unknown,
    cb: (err: grpc.ServiceError | null, res: T) => void,
  ) => void,
  req: unknown,
): Promise<T> {
  return new Promise((resolve, reject) => {
    fn(req, (err, res) => {
      if (err) reject(err);
      else resolve(res);
    });
  });
}

test("redactHspRequest logs secret keys only", () => {
  const redacted = redactHspRequest({
    toolName: "echo.ping",
    resolvedSecrets: { "api-token": "sk-super-secret" },
  }) as { resolvedSecrets: { keys: string[] } };
  assert.deepEqual(redacted.resolvedSecrets.keys, ["api-token"]);
  assert.equal(JSON.stringify(redacted).includes("sk-super-secret"), false);
});

test(
  "echo-ts Register + Invoke over temp UDS",
  { skip: process.platform === "win32" ? "UDS is not supported on Windows" : false },
  async () => {
    const protoRoot = resolveProtoRoot();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "hsp-echo-ts-"));
  const socketPath = path.join(dir, "echo.sock");

  const echo = await startEchoBroker({ socketPath, protoRoot });
  const client = connectClient(echo.socketPath, protoRoot);
  const logs: string[] = [];
  const originalLog = console.log;
  console.log = (...args: unknown[]) => {
    logs.push(args.map(String).join(" "));
  };

  try {
    const registerRes = await unary(client.register.bind(client), {
      brokerId: BROKER_ID,
      brokerVersion: "0.1.0",
      sessionId: "sess-test",
      manifest: {
        brokerId: BROKER_ID,
        trustTier: TRUST_TIER_FIRST_PARTY,
        tools: [{ toolName: "echo.ping", sideEffect: 0, requiredSecrets: [] }],
      },
    });
    assert.equal(registerRes.error ?? null, null);

    const listRes = await unary(client.listTools.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
    });
    const tools = listRes.tools as unknown[];
    assert.ok(Array.isArray(tools) && tools.length >= 1);

    const invokeRes = await unary(client.invoke.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
      toolName: "echo.ping",
      args: {
        fields: {
          message: { stringValue: "hello-ts" },
        },
      },
      resolvedSecrets: {},
    });

    const output = (invokeRes.result as { output?: { fields?: Record<string, unknown> } })
      ?.output?.fields;
    assert.ok(output);
    const toolName = output.tool_name as { stringValue?: string };
    assert.equal(toolName.stringValue, "echo.ping");

    const hookRes = await unary(client.onRequest.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
    });
    // proto-loader enums: String → enum name; numeric OK if enums configured otherwise.
    const disposition = String(hookRes.disposition ?? "");
    assert.ok(
      disposition === String(HOOK_DISPOSITION_OK) ||
        disposition === "HOOK_DISPOSITION_OK" ||
        disposition === "OK" ||
        disposition === "1",
      `unexpected disposition: ${disposition}`,
    );

    await unary(client.onComplete.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
    });
    await unary(client.onResponse.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
    });

    const joined = logs.join("\n");
    for (const rpc of [
      "register",
      "listTools",
      "invoke",
      "onRequest",
      "onComplete",
      "onResponse",
    ]) {
      assert.ok(joined.includes(`[hsp] ${rpc}`), `missing log for ${rpc}: ${joined}`);
    }
    assert.ok(joined.includes("echo.ping"));
    assert.ok(joined.includes("sess-test"));
  } finally {
    console.log = originalLog;
    client.close();
    stopEchoBroker(echo);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("parseW3CTraceparent validator", () => {
  const valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
  const parsed = parseW3CTraceparent(valid);
  assert.ok(parsed);
  assert.equal(parsed.version, "00");
  assert.equal(parsed.traceId, "4bf92f3577b34da6a3ce929d0e0e4736");
  assert.equal(parsed.parentId, "00f067aa0ba902b7");
  assert.equal(parsed.flags, "01");

  assert.equal(parseW3CTraceparent("ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"), null);
  assert.equal(parseW3CTraceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01"), null);
  assert.equal(parseW3CTraceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01"), null);
  assert.equal(parseW3CTraceparent("invalid"), null);
});

test(
  "echo-ts propagates and echoes TraceContext over UDS",
  { skip: process.platform === "win32" ? "UDS is not supported on Windows" : false },
  async () => {
    const protoRoot = resolveProtoRoot();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "hsp-echo-ts-trace-"));
  const socketPath = path.join(dir, "echo.sock");

  const echo = await startEchoBroker({ socketPath, protoRoot });
  const client = connectClient(echo.socketPath, protoRoot);
  const logs: string[] = [];
  const originalLog = console.log;
  console.log = (...args: unknown[]) => {
    logs.push(args.map(String).join(" "));
  };

  const testTraceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

  try {
    const invokeRes = await unary(client.invoke.bind(client), {
      sessionId: "sess-trace-test",
      runId: "run-trace-test",
      toolName: "echo.ping",
      args: { fields: {} },
      resolvedSecrets: {},
      traceContext: {
        traceparent: testTraceparent,
      },
    });

    const output = (invokeRes.result as { output?: { fields?: Record<string, unknown> } })
      ?.output?.fields;
    assert.ok(output);
    const traceparentField = output.traceparent as { stringValue?: string };
    assert.equal(traceparentField?.stringValue, testTraceparent);

    const hookRes = await unary(client.onRequest.bind(client), {
      sessionId: "sess-trace-test",
      runId: "run-trace-test",
      traceContext: {
        traceparent: testTraceparent,
      },
    });
    assert.ok(hookRes);

    const joined = logs.join("\n");
    assert.ok(joined.includes(testTraceparent), `logs must include propagated traceparent: ${joined}`);
  } finally {
    console.log = originalLog;
    client.close();
    stopEchoBroker(echo);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
