/**
 * TypeScript HSP file-attachment broker — Phase 0 sample.
 *
 * Loads `proto/vam/v1/hosted_services.proto` at runtime via @grpc/proto-loader
 * (no protoc codegen). Binds a Unix domain socket and implements HostedBroker.
 * Images default to local disk. Set HSP_FILES_BACKEND=s3|oci for object storage
 * with per-invoke vault secrets. Tool names do not name a cloud.
 *
 * Usage:
 *   node dist/index.js --socket /tmp/hsp-files-ts.sock [--data-dir /tmp/hsp-files-ts-data]
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";
import { storeForInvoke } from "./backend";
import { OCI_SECRET_KEYS } from "./oci";
import { S3_SECRET_KEYS } from "./s3";
import {
  ALLOWED_MEDIA_TYPES,
  LocalBlobStore,
  assertImageBytes,
  decodeBase64Payload,
  filePlaceholder,
  isAllowedMediaType,
  type AllowedMediaType,
  type AttachmentStore,
  type StoredFileMeta,
} from "./store";

/** TrustTier.TRUST_TIER_OPERATOR */
export const TRUST_TIER_OPERATOR = 3;

/** HookDisposition.HOOK_DISPOSITION_OK */
export const HOOK_DISPOSITION_OK = 1;

/** SideEffectClass.SIDE_EFFECT_CLASS_READ_ONLY */
export const SIDE_EFFECT_READ_ONLY = 1;

/** SideEffectClass.SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL */
export const SIDE_EFFECT_MUTABLE_EXTERNAL = 3;

export const BROKER_ID = "files-ts";

export { ALLOWED_MEDIA_TYPES, LocalBlobStore, OCI_SECRET_KEYS, S3_SECRET_KEYS };

const OBJECT_STORE_SECRETS = [...OCI_SECRET_KEYS, ...S3_SECRET_KEYS, "s3-endpoint"];

function fileTool(
  name: string,
  description: string,
  sideEffect: number,
  inputSchemaJson: string,
) {
  return {
    name,
    description,
    inputSchemaJson,
    hostExec: {
      adapterId: `hosted:${BROKER_ID}:${name}`,
      config: {},
    },
    requiredSecrets: OBJECT_STORE_SECRETS.map((logicalName) => ({
      name: logicalName,
      required: false,
    })),
    sideEffect,
    outputSchemaJson: "",
  };
}

export const FILE_TOOLS = [
  fileTool(
    "files.put",
    "Store a small image attachment (local disk, or S3/OCI when HSP_FILES_BACKEND is set)",
    SIDE_EFFECT_MUTABLE_EXTERNAL,
    JSON.stringify({
      type: "object",
      properties: {
        name: { type: "string" },
        media_type: { type: "string", enum: [...ALLOWED_MEDIA_TYPES] },
        content_base64: { type: "string" },
        placeholder: { type: "string" },
      },
      required: ["name", "media_type", "content_base64"],
    }),
  ),
  fileTool(
    "files.list",
    "List image attachments in the current session/run scope",
    SIDE_EFFECT_READ_ONLY,
    JSON.stringify({ type: "object", properties: {} }),
  ),
  fileTool(
    "files.get",
    "Return attachment metadata plus a byte buffer; look up by placeholder (or file_id)",
    SIDE_EFFECT_READ_ONLY,
    JSON.stringify({
      type: "object",
      properties: {
        placeholder: { type: "string" },
        name: { type: "string" },
        file_id: { type: "string" },
      },
    }),
  ),
];

export type FilesServer = {
  server: grpc.Server;
  /** Set when listening on UDS (product / Lima path). */
  socketPath?: string;
  /** Set when listening on TCP loopback (Windows / debug tests only). */
  tcpAddress?: string;
  dataDir: string;
  store: LocalBlobStore;
};

/** Address a gRPC client should dial (`unix:/path` or `127.0.0.1:port`). */
export function brokerDialAddress(server: FilesServer): string {
  if (server.tcpAddress) {
    return server.tcpAddress;
  }
  if (server.socketPath) {
    return `unix:${server.socketPath}`;
  }
  throw new Error("broker is not listening");
}

function resolveProtoRoot(explicit?: string): string {
  if (explicit) {
    return path.resolve(explicit);
  }
  if (process.env.HSP_PROTO_ROOT) {
    return path.resolve(process.env.HSP_PROTO_ROOT);
  }
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
      } else if (key === "content_base64" || key === "contentBase64") {
        const text = typeof nested === "string" ? nested : "";
        out[key] = text.length > 24 ? `${text.slice(0, 12)}…(${text.length} chars)` : nested;
      } else {
        out[key] = redactHspRequest(nested);
      }
    }
    return out;
  }
  return value;
}

export function logHspRpc(rpcName: string, request: unknown): void {
  console.log(`[hsp] ${rpcName} ${JSON.stringify(redactHspRequest(request))}`);
}

export function readArgString(args: unknown, key: string): string | undefined {
  if (!args || typeof args !== "object") {
    return undefined;
  }
  const rec = args as Record<string, unknown>;
  const camel = key.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase());
  for (const candidate of [key, camel]) {
    const direct = rec[candidate];
    if (typeof direct === "string" && direct.length > 0) {
      return direct;
    }
  }
  const fields = rec.fields as Record<string, { stringValue?: string }> | undefined;
  if (fields) {
    for (const candidate of [key, camel]) {
      const v = fields[candidate]?.stringValue;
      if (typeof v === "string" && v.length > 0) {
        return v;
      }
    }
  }
  return undefined;
}

function stringValue(value: string): { stringValue: string } {
  return { stringValue: value };
}

function numberValue(value: number): { numberValue: number } {
  return { numberValue: value };
}

function bufferValue(bytes: Buffer): {
  listValue: { values: { numberValue: number }[] };
} {
  return {
    listValue: {
      values: Array.from(bytes, (b) => ({ numberValue: b })),
    },
  };
}

function metaToStructFields(meta: StoredFileMeta): Record<string, unknown> {
  return {
    placeholder: stringValue(filePlaceholder(meta)),
    file_id: stringValue(meta.fileId),
    name: stringValue(meta.originalName),
    media_type: stringValue(meta.mediaType),
    size_bytes: numberValue(meta.sizeBytes),
  };
}

function okResult(
  fields: Record<string, unknown>,
  artifact?: { suggestedRelpath: string; bytes: Buffer },
): Record<string, unknown> {
  return {
    result: {
      output: { fields },
      error: null,
    },
    outputBlocks: [],
    artifact: artifact
      ? {
          suggestedRelpath: artifact.suggestedRelpath,
          bytes: artifact.bytes,
        }
      : null,
  };
}

function errResult(message: string): Record<string, unknown> {
  return {
    result: {
      output: null,
      error: { message, retryable: false },
    },
    outputBlocks: [],
    artifact: null,
  };
}

async function invokePut(
  store: AttachmentStore,
  sessionId: string,
  runId: string,
  args: unknown,
): Promise<Record<string, unknown>> {
  const name = readArgString(args, "name");
  const mediaTypeRaw = readArgString(args, "media_type");
  const content = readArgString(args, "content_base64");
  if (!name || !mediaTypeRaw || !content) {
    return errResult("files.put requires name, media_type, and content_base64");
  }
  if (!isAllowedMediaType(mediaTypeRaw)) {
    return errResult(
      `unsupported media_type (allowed: ${ALLOWED_MEDIA_TYPES.join(", ")})`,
    );
  }
  const mediaType: AllowedMediaType = mediaTypeRaw;
  try {
    const bytes = decodeBase64Payload(content);
    assertImageBytes(mediaType, bytes);
    const meta = await store.put({
      sessionId,
      runId,
      originalName: name,
      mediaType,
      bytes,
      placeholder: readArgString(args, "placeholder"),
    });
    return okResult(metaToStructFields(meta));
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return errResult(message);
  }
}

async function invokeList(
  store: AttachmentStore,
  sessionId: string,
  runId: string,
): Promise<Record<string, unknown>> {
  const files = await store.list(sessionId, runId);
  return okResult({
    count: numberValue(files.length),
    files: {
      listValue: {
        values: files.map((meta) => ({
          structValue: { fields: metaToStructFields(meta) },
        })),
      },
    },
  });
}

async function invokeGet(
  store: AttachmentStore,
  sessionId: string,
  runId: string,
  args: unknown,
): Promise<Record<string, unknown>> {
  const handle =
    readArgString(args, "placeholder") ??
    readArgString(args, "name") ??
    readArgString(args, "file_id");
  if (!handle) {
    return errResult("files.get requires placeholder, name, or file_id");
  }
  try {
    const { meta, bytes } = await store.get(sessionId, runId, handle);
    return okResult({
      ...metaToStructFields(meta),
      buffer: bufferValue(bytes),
    });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return errResult(message);
  }
}

function listedPlaceholders(files: StoredFileMeta[]): string[] {
  return files.map((meta) => filePlaceholder(meta));
}

function grpcError(code: grpc.status, message: string): grpc.ServiceError {
  return Object.assign(new Error(message), {
    code,
    details: message,
    metadata: new grpc.Metadata(),
  }) as grpc.ServiceError;
}

export function createFilesServiceHandlers(
  store: LocalBlobStore,
): grpc.UntypedServiceImplementation {
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
      callback(null, { tools: FILE_TOOLS });
    },
    invoke(
      call: grpc.ServerUnaryCall<
        {
          toolName?: string;
          sessionId?: string;
          runId?: string;
          args?: unknown;
          resolvedSecrets?: Record<string, string>;
        },
        unknown
      >,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      const req = call.request ?? {};
      logHspRpc("invoke", req);
      const sessionId = req.sessionId ?? "";
      const runId = req.runId ?? "";
      const toolName = req.toolName ?? "";
      const secrets = req.resolvedSecrets ?? {};
      const active = storeForInvoke(store, secrets);
      void (async () => {
        if (toolName === "files.put") {
          callback(null, await invokePut(active, sessionId, runId, req.args));
          return;
        }
        if (toolName === "files.list") {
          callback(null, await invokeList(active, sessionId, runId));
          return;
        }
        if (toolName === "files.get") {
          callback(null, await invokeGet(active, sessionId, runId, req.args));
          return;
        }
        callback(null, errResult(`unknown tool: ${toolName}`));
      })().catch((err) => {
        const message = err instanceof Error ? err.message : String(err);
        callback(null, errResult(message));
      });
    },
    onRequest(
      call: grpc.ServerUnaryCall<
        { sessionId?: string; runId?: string },
        unknown
      >,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      const req = call.request ?? {};
      logHspRpc("onRequest", req);
      const sessionId = req.sessionId ?? "";
      const runId = req.runId ?? "";
      void (async () => {
        const files = await store.list(sessionId, runId);
        const placeholders = listedPlaceholders(files);
        console.log(
          `[hsp] onRequest placeholders=${JSON.stringify(placeholders)}`,
        );
        callback(null, {
          error: null,
          injectedContext:
            placeholders.length === 0
              ? []
              : [
                  {
                    content: `files-ts placeholders: ${placeholders.join(", ")}`,
                  },
                ],
          disposition: HOOK_DISPOSITION_OK,
        });
      })().catch((err) => {
        const message = err instanceof Error ? err.message : String(err);
        callback(null, {
          error: { message, retryable: false },
          injectedContext: [],
          disposition: HOOK_DISPOSITION_OK,
        });
      });
    },
    onComplete(
      call: grpc.ServerUnaryCall<
        { sessionId?: string; runId?: string },
        unknown
      >,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      const req = call.request ?? {};
      logHspRpc("onComplete", req);
      const sessionId = req.sessionId ?? "";
      const runId = req.runId ?? "";
      void (async () => {
        const placeholders = listedPlaceholders(await store.list(sessionId, runId));
        console.log(
          `[hsp] onComplete placeholders=${JSON.stringify(placeholders)}`,
        );
        callback(null, {
          error: null,
          retentionLabels: placeholders.map((value) => ({
            key: "placeholder",
            value,
          })),
          disposition: HOOK_DISPOSITION_OK,
        });
      })().catch((err) => {
        const message = err instanceof Error ? err.message : String(err);
        callback(null, {
          error: { message, retryable: false },
          retentionLabels: [],
          disposition: HOOK_DISPOSITION_OK,
        });
      });
    },
    onResponse(
      call: grpc.ServerUnaryCall<
        { sessionId?: string; runId?: string },
        unknown
      >,
      callback: grpc.sendUnaryData<Record<string, unknown>>,
    ) {
      const req = call.request ?? {};
      logHspRpc("onResponse", req);
      const sessionId = req.sessionId ?? "";
      const runId = req.runId ?? "";
      void (async () => {
        const placeholders = listedPlaceholders(await store.list(sessionId, runId));
        console.log(
          `[hsp] onResponse placeholders=${JSON.stringify(placeholders)}`,
        );
        callback(null, {
          error: null,
          responseBlocks: placeholders.map((value) => ({
            text: { content: `placeholder=${value}` },
          })),
          disposition: HOOK_DISPOSITION_OK,
        });
      })().catch((err) => {
        const message = err instanceof Error ? err.message : String(err);
        callback(null, {
          error: { message, retryable: false },
          responseBlocks: [],
          disposition: HOOK_DISPOSITION_OK,
        });
      });
    },
    download(
      call: grpc.ServerWritableStream<
        {
          sessionId?: string;
          runId?: string;
          fileId?: string;
          resolvedSecrets?: Record<string, string>;
        },
        Record<string, unknown>
      >,
    ) {
      const req = call.request ?? {};
      logHspRpc("download", req);
      const sessionId = req.sessionId ?? "";
      const runId = req.runId ?? "";
      const fileId = req.fileId ?? "";
      const secrets = req.resolvedSecrets ?? {};
      const active = storeForInvoke(store, secrets);
      void (async () => {
        if (!fileId.trim()) {
          call.emit(
            "error",
            grpcError(grpc.status.INVALID_ARGUMENT, "placeholder or file_id is required"),
          );
          return;
        }
        const { meta, chunks } = await active.streamGet(sessionId, runId, fileId);
        let first = true;
        let chunkCount = 0;
        let streamedBytes = 0;
        for await (const data of chunks) {
          const msg: Record<string, unknown> = { data };
          if (first) {
            msg.mediaType = meta.mediaType;
            msg.fileName = meta.originalName;
            msg.sizeBytes = meta.sizeBytes;
            first = false;
          }
          chunkCount += 1;
          streamedBytes += data.length;
          call.write(msg);
        }
        if (first) {
          call.write({
            data: Buffer.alloc(0),
            mediaType: meta.mediaType,
            fileName: meta.originalName,
            sizeBytes: meta.sizeBytes,
          });
        }
        console.log(
          `[hsp] download stream ${JSON.stringify({
            handle: fileId,
            placeholder: filePlaceholder(meta),
            mediaType: meta.mediaType,
            chunks: chunkCount,
            bytes: streamedBytes,
          })}`,
        );
        call.end();
      })().catch((err) => {
        const message = err instanceof Error ? err.message : String(err);
        const code = message.includes("not found")
          ? grpc.status.NOT_FOUND
          : grpc.status.INTERNAL;
        call.emit("error", grpcError(code, message));
      });
    },
  };
}

function loadHostedBrokerService(protoRoot: string): grpc.ServiceDefinition {
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
  return proto.vam.v1.HostedBroker.service;
}

async function bindListen(
  server: grpc.Server,
  bindAddr: string,
): Promise<number> {
  return new Promise((resolve, reject) => {
    server.bindAsync(
      bindAddr,
      grpc.ServerCredentials.createInsecure(),
      (err, port) => {
        if (err) {
          reject(err);
          return;
        }
        resolve(port);
      },
    );
  });
}

/**
 * Bind HostedBroker. Product path is UDS (`socketPath`). TCP loopback is
 * tests/debug only (same split as `HspEndpoint` in vamd).
 */
export async function startFilesBroker(options: {
  socketPath?: string;
  /** e.g. `127.0.0.1:0` — Windows hermetic tests when UDS bind is denied. */
  tcpBind?: string;
  protoRoot?: string;
  dataDir?: string;
}): Promise<FilesServer> {
  if (!options.socketPath && !options.tcpBind) {
    throw new Error("startFilesBroker requires socketPath or tcpBind");
  }
  const protoRoot = resolveProtoRoot(options.protoRoot);
  const service = loadHostedBrokerService(protoRoot);

  let socketPath: string | undefined;
  if (options.socketPath) {
    socketPath = path.resolve(options.socketPath);
    if (fs.existsSync(socketPath)) {
      fs.unlinkSync(socketPath);
    }
    const parent = path.dirname(socketPath);
    if (parent && parent !== ".") {
      fs.mkdirSync(parent, { recursive: true });
    }
  }

  const dataDir = path.resolve(
    options.dataDir ??
      (socketPath
        ? path.join(path.dirname(socketPath), "hsp-files-ts-data")
        : path.join(os.tmpdir(), "hsp-files-ts-data")),
  );
  const store = new LocalBlobStore(dataDir);

  const server = new grpc.Server();
  server.addService(service, createFilesServiceHandlers(store));

  let tcpAddress: string | undefined;
  if (options.tcpBind) {
    const port = await bindListen(server, options.tcpBind);
    const host = options.tcpBind.split(":")[0] || "127.0.0.1";
    tcpAddress = `${host}:${port}`;
  } else if (socketPath) {
    await bindListen(server, `unix:${socketPath}`);
    try {
      fs.chmodSync(socketPath, 0o600);
    } catch (chmodErr) {
      if (process.platform !== "win32") {
        server.forceShutdown();
        throw chmodErr;
      }
    }
  }

  return { server, socketPath, tcpAddress, dataDir, store };
}

export function stopFilesBroker(files: FilesServer): void {
  files.server.forceShutdown();
  try {
    if (files.socketPath && fs.existsSync(files.socketPath)) {
      fs.unlinkSync(files.socketPath);
    }
  } catch {
    // best-effort cleanup
  }
}

export function parseArgs(argv: string[]): {
  socket: string;
  protoRoot?: string;
  dataDir?: string;
} {
  let socket: string | undefined;
  let protoRoot: string | undefined;
  let dataDir: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--socket") {
      socket = argv[++i];
    } else if (arg === "--proto-root") {
      protoRoot = argv[++i];
    } else if (arg === "--data-dir") {
      dataDir = argv[++i];
    } else if (arg === "--help" || arg === "-h") {
      console.log(
        "Usage: node dist/index.js --socket <path> [--proto-root <proto/>] [--data-dir <dir>]",
      );
      process.exit(0);
    }
  }
  if (!socket) {
    throw new Error("Missing required --socket <path>");
  }
  return { socket, protoRoot, dataDir };
}

async function main(): Promise<void> {
  const { socket, protoRoot, dataDir } = parseArgs(process.argv.slice(2));
  const files = await startFilesBroker({
    socketPath: socket,
    protoRoot,
    dataDir: dataDir ?? path.join(os.tmpdir(), "hsp-files-ts-data"),
  });
  console.error(
    `hsp-files-ts listening on unix:${files.socketPath} dataDir=${files.dataDir} (broker_id=${BROKER_ID})`,
  );

  const shutdown = () => {
    stopFilesBroker(files);
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
