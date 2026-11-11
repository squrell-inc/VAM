/**
 * TypeScript HSP echo broker — Phase 0 sample.
 *
 * Loads `proto/vam/v1/hosted_services.proto` at runtime via @grpc/proto-loader
 * (no protoc codegen). Binds a Unix domain socket and implements HostedBroker.
 *
 * Usage:
 *   node dist/index.js --socket /tmp/hsp-echo-ts.sock
 */

import * as fs from "node:fs";
import * as path from "node:path";
import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";

/** TrustTier.TRUST_TIER_FIRST_PARTY */
export const TRUST_TIER_FIRST_PARTY = 2;

/** HookDisposition.HOOK_DISPOSITION_OK */
export const HOOK_DISPOSITION_OK = 1;

export const BROKER_ID = "echo-ts";

export const ECHO_PING_TOOL = {
  name: "echo.ping",
  description: "Echo ping for HSP TypeScript transport tests",
  inputSchemaJson:
    '{"type":"object","properties":{"message":{"type":"string"}}}',
  hostExec: {
    adapterId: "broker-echo-ts",
    config: {},
  },
  requiredSecrets: [],
  sideEffect: 0,
  outputSchemaJson: "",
};

export type EchoServer = {
  server: grpc.Server;
  socketPath: string;
};

function resolveProtoRoot(explicit?: string): string {
  if (explicit) {
    return path.resolve(explicit);
  }
  if (process.env.HSP_PROTO_ROOT) {
    return path.resolve(process.env.HSP_PROTO_ROOT);
  }
  // Prefer sibling repo `proto/` when running from examples/hsp-echo-ts.
  const candidates = [
    path.resolve(__dirname, "../../../proto"),
    path.resolve(process.cwd(), "proto"),
    path.resolve(process.cwd(), "../../proto"),
  ];
  for (const candidate of candidates) {
    const hosted = path.join(candidate, "vam/v1/hosted_services.proto");
    if (fs.existsSync(hosted)) {
      return candidate;
    }
  }
  throw new Error(
    "Could not find proto/ root (set --proto-root or HSP_PROTO_ROOT)",
  );
}

export function loadHostedBrokerPackage(
  protoRoot: string,
): protoLoader.PackageDefinition {
  const hostedProto = path.join(protoRoot, "vam/v1/hosted_services.proto");
  return protoLoader.loadSync(hostedProto, {
    keepCase: false,
    longs: String,
    enums: String,
    defaults: true,
    oneofs: true,
    includeDirs: [protoRoot],
  });
}

function hookOkDisposition(): number {
  return HOOK_DISPOSITION_OK;
}

const SECRET_KEY_FIELDS = new Set(["resolvedSecrets", "resolved_secrets"]);

/** JSON-safe copy; secret map values are replaced with keys-only. */
export function redactHspRequest(value: unknown): unknown {
  if (value === null || value === undefined) {
    return value ?? {};
  }
  if (Array.isArray(value)) {
    return value.map(redactHspRequest);
  }
  if (typeof value === "object") {
    const obj = value as Record<string, unknown>;
    const out: Record<string, unknown> = {};
    for (const [key, nested] of Object.entries(obj)) {
      if (SECRET_KEY_FIELDS.has(key) && nested && typeof nested === "object") {
        out[key] = { keys: Object.keys(nested as Record<string, unknown>) };
      } else {
        out[key] = redactHspRequest(nested);
      }
    }
    return out;
  }
  return value;
}

export type W3CTraceContext = {
  traceparent?: string;
  tracestate?: string;
};

export type ParsedTraceparent = {
  version: string;
  traceId: string;
  parentId: string;
  flags: string;
};

export function parseW3CTraceparent(raw?: string): ParsedTraceparent | null {
  if (!raw || typeof raw !== "string") return null;
  const match = raw.trim().match(/^([0-9a-f]{2})-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$/);
  if (!match) return null;
  if (match[1] === "ff") return null;
  if (match[2] === "00000000000000000000000000000000") return null;
  if (match[3] === "0000000000000000") return null;
  return {
    version: match[1],
    traceId: match[2],
    parentId: match[3],
    flags: match[4],
  };
}

export function extractTraceContext(req: unknown): {
  raw?: string;
  parsed: ParsedTraceparent | null;
} {
  if (!req || typeof req !== "object") return { parsed: null };
  const r = req as Record<string, unknown>;
  const tc = (r.traceContext ?? r.trace_context) as W3CTraceContext | undefined;
  const raw = tc?.traceparent;
  return { raw, parsed: parseW3CTraceparent(raw) };
}

export function logHspRpc(rpcName: string, request: unknown): void {
  const { raw, parsed } = extractTraceContext(request);
  const tracePrefix = parsed ? `[traceparent=${raw}] ` : "";
  console.log(`[hsp] ${tracePrefix}${rpcName} ${JSON.stringify(redactHspRequest(request))}`);
}

export function createEchoServiceHandlers(): grpc.UntypedServiceImplementation {
  return {
    register(
      call: grpc.ServerUnaryCall<unknown, unknown>,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      logHspRpc("register", call.request);
      callback(null, {
        error: null,
        sessionPolicyVersion: "",
      });
    },
    listTools(
      call: grpc.ServerUnaryCall<unknown, unknown>,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      logHspRpc("listTools", call.request);
      callback(null, { tools: [ECHO_PING_TOOL] });
    },
    invoke(
      call: grpc.ServerUnaryCall<{ toolName?: string; args?: unknown; resolvedSecrets?: Record<string, string>; callerContext?: { userId?: string; tenantId?: string; agentUrn?: string }; caller_context?: { userId?: string; tenantId?: string; agentUrn?: string }; traceContext?: W3CTraceContext; trace_context?: W3CTraceContext }, unknown>,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      const req = call.request ?? {};
      const { raw, parsed } = extractTraceContext(req);
      logHspRpc("invoke", req);
      const fields: Record<string, unknown> = {
        tool_name: { stringValue: req.toolName ?? "" },
      };
      if (req.args) {
        fields.args = { structValue: req.args };
      }
      if (raw) {
        fields.traceparent = { stringValue: raw };
      }
      const secrets = req.resolvedSecrets ?? {};
      if (Object.keys(secrets).length > 0) {
        const secretFields: Record<string, unknown> = {};
        for (const [key, value] of Object.entries(secrets)) {
          secretFields[key] = { stringValue: value };
        }
        fields.resolved_secrets = { structValue: { fields: secretFields } };
      }
      const caller = req.callerContext ?? req.caller_context;
      if (caller?.userId) {
        fields.caller_user_id = { stringValue: caller.userId };
      }
      callback(null, {
        result: {
          output: { fields },
          error: null,
        },
        outputBlocks: [],
        artifact: null,
      });
    },
    onRequest(
      call: grpc.ServerUnaryCall<unknown, unknown>,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      logHspRpc("onRequest", call.request);
      callback(null, {
        error: null,
        injectedContext: [],
        disposition: hookOkDisposition(),
      });
    },
    onComplete(
      call: grpc.ServerUnaryCall<unknown, unknown>,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      logHspRpc("onComplete", call.request);
      callback(null, {
        error: null,
        retentionLabels: [],
        disposition: hookOkDisposition(),
      });
    },
    onResponse(
      call: grpc.ServerUnaryCall<unknown, unknown>,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      logHspRpc("onResponse", call.request);
      callback(null, {
        error: null,
        responseBlocks: [],
        disposition: hookOkDisposition(),
      });
    },
  };
}

/** Bind HostedBroker on a Unix domain socket (unlink stale, chmod 0o600). */
export async function startEchoBroker(options: {
  socketPath: string;
  protoRoot?: string;
}): Promise<EchoServer> {
  const protoRoot = resolveProtoRoot(options.protoRoot);
  const packageDefinition = loadHostedBrokerPackage(protoRoot);
  const proto = grpc.loadPackageDefinition(packageDefinition) as unknown as {
    vam: {
      v1: {
        HostedBroker: {
          service: grpc.ServiceDefinition;
        };
      };
    };
  };

  const socketPath = path.resolve(options.socketPath);
  if (fs.existsSync(socketPath)) {
    fs.unlinkSync(socketPath);
  }
  const parent = path.dirname(socketPath);
  if (parent && parent !== ".") {
    fs.mkdirSync(parent, { recursive: true });
  }

  const server = new grpc.Server();
  server.addService(
    proto.vam.v1.HostedBroker.service,
    createEchoServiceHandlers(),
  );

  await new Promise<void>((resolve, reject) => {
    server.bindAsync(
      `unix:${socketPath}`,
      grpc.ServerCredentials.createInsecure(),
      (err) => {
        if (err) {
          reject(err);
          return;
        }
        try {
          fs.chmodSync(socketPath, 0o600);
        } catch (chmodErr) {
          server.forceShutdown();
          reject(chmodErr);
          return;
        }
        resolve();
      },
    );
  });

  return { server, socketPath };
}

export function stopEchoBroker(echo: EchoServer): void {
  echo.server.forceShutdown();
  try {
    if (fs.existsSync(echo.socketPath)) {
      fs.unlinkSync(echo.socketPath);
    }
  } catch {
    // best-effort cleanup
  }
}

export function parseArgs(argv: string[]): {
  socket: string;
  protoRoot?: string;
} {
  let socket: string | undefined;
  let protoRoot: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--socket") {
      socket = argv[++i];
    } else if (arg === "--proto-root") {
      protoRoot = argv[++i];
    } else if (arg === "--help" || arg === "-h") {
      console.log(
        "Usage: node dist/index.js --socket <path> [--proto-root <proto/>]",
      );
      process.exit(0);
    }
  }
  if (!socket) {
    throw new Error("Missing required --socket <path>");
  }
  return { socket, protoRoot };
}

async function main(): Promise<void> {
  const { socket, protoRoot } = parseArgs(process.argv.slice(2));
  const echo = await startEchoBroker({ socketPath: socket, protoRoot });
  console.error(
    `hsp-echo-ts listening on unix:${echo.socketPath} (broker_id=${BROKER_ID})`,
  );

  const shutdown = () => {
    stopEchoBroker(echo);
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

if (require.main === module) {
  main().catch((err) => {
    console.error(err);
    process.exit(1);
  });
}
