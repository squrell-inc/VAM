/**
 * Hermetic integration test: Register + files.put/list/get over a temp UDS.
 * Does not require vamd.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as grpc from "@grpc/grpc-js";
import { test } from "node:test";
import assert from "node:assert/strict";
import * as crypto from "node:crypto";
import {
  BROKER_ID,
  FILE_TOOLS,
  HOOK_DISPOSITION_OK,
  OCI_SECRET_KEYS,
  S3_SECRET_KEYS,
  LocalBlobStore,
  TRUST_TIER_OPERATOR,
  brokerDialAddress,
  loadHostedBrokerPackage,
  redactHspRequest,
  startFilesBroker,
  stopFilesBroker,
} from "./index";
import { resolveStorageBackend } from "./backend";
import { OciBlobStore, memoryOciTransport, parseOciConfig } from "./oci";
import { S3BlobStore, memoryS3Transport, parseS3Config } from "./s3";

/** 1×1 PNG */
const PNG_B64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

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
  download: (req: unknown) => grpc.ClientReadableStream<Record<string, unknown>>;
  close: () => void;
};

function connectClient(address: string, protoRoot: string): HostedBrokerClient {
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
    address,
    grpc.credentials.createInsecure(),
  );
}

function collectDownload(
  stream: grpc.ClientReadableStream<Record<string, unknown>>,
): Promise<{ bytes: Buffer; mediaType?: string; fileName?: string; sizeBytes?: number }> {
  return new Promise((resolve, reject) => {
    const parts: Buffer[] = [];
    let mediaType: string | undefined;
    let fileName: string | undefined;
    let sizeBytes: number | undefined;
    stream.on("data", (chunk) => {
      if (typeof chunk.mediaType === "string" && chunk.mediaType.length > 0) {
        mediaType = chunk.mediaType;
      }
      if (typeof chunk.fileName === "string" && chunk.fileName.length > 0) {
        fileName = chunk.fileName;
      }
      if (chunk.sizeBytes !== undefined && chunk.sizeBytes !== null) {
        sizeBytes = Number(chunk.sizeBytes);
      }
      if (chunk.data) {
        parts.push(Buffer.from(chunk.data as Buffer | Uint8Array));
      }
    });
    stream.on("error", reject);
    stream.on("end", () => {
      resolve({
        bytes: Buffer.concat(parts),
        mediaType,
        fileName,
        sizeBytes,
      });
    });
  });
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

function structArg(fields: Record<string, string>): {
  fields: Record<string, { stringValue: string }>;
} {
  const out: Record<string, { stringValue: string }> = {};
  for (const [key, value] of Object.entries(fields)) {
    out[key] = { stringValue: value };
  }
  return { fields: out };
}

test("redactHspRequest logs secret keys only", () => {
  const redacted = redactHspRequest({
    toolName: "files.put",
    resolvedSecrets: { "oci-private-key": "PEM-SUPER-SECRET" },
  }) as { resolvedSecrets: { keys: string[] } };
  assert.deepEqual(redacted.resolvedSecrets.keys, ["oci-private-key"]);
  assert.equal(JSON.stringify(redacted).includes("PEM-SUPER-SECRET"), false);
});

test("put-broker.json matches broker id, operator tier, and FILE_TOOLS", () => {
  const manifestPath = path.resolve(__dirname, "../put-broker.json");
  const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8")) as {
    brokerId: string;
    trustTier: number;
    tools: { toolName: string; sideEffect: number }[];
    hooks: { phase: number; required: boolean }[];
    secretBindings: { logicalName: string }[];
    egressDomains: string[];
  };
  assert.equal(manifest.brokerId, BROKER_ID);
  assert.equal(manifest.trustTier, TRUST_TIER_OPERATOR);
  assert.deepEqual(
    manifest.tools.map((t) => t.toolName).sort(),
    FILE_TOOLS.map((t) => t.name).sort(),
  );
  const expectedSecrets = [...OCI_SECRET_KEYS, ...S3_SECRET_KEYS, "s3-endpoint"].sort();
  assert.deepEqual(
    manifest.secretBindings.map((b) => b.logicalName).sort(),
    expectedSecrets,
  );
  assert.ok(manifest.egressDomains.includes("oraclecloud.com"));
  assert.ok(manifest.egressDomains.includes("amazonaws.com"));
  assert.deepEqual(
    manifest.hooks.map((h) => h.phase).sort(),
    [1, 2, 3],
  );
  assert.ok(manifest.hooks.every((h) => h.required === false));
});

test("LocalBlobStore put writes image bytes on disk", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "hsp-files-store-"));
  try {
    const store = new LocalBlobStore(dir);
    const bytes = Buffer.from(PNG_B64, "base64");
    const meta = store.put({
      sessionId: "sess-disk",
      runId: "run-disk",
      originalName: "pixel.png",
      mediaType: "image/png",
      bytes,
    });
    assert.equal(meta.placeholder, "pixel.png");
    const abs = path.join(dir, ...meta.relPath.split("/"));
    assert.ok(fs.existsSync(abs));
    assert.deepEqual(fs.readFileSync(abs), bytes);
    assert.equal(store.list("sess-disk", "run-disk").length, 1);
    const byName = store.get("sess-disk", "run-disk", "pixel.png");
    assert.deepEqual(byName.bytes, bytes);
    assert.throws(
      () =>
        store.put({
          sessionId: "sess-disk",
          runId: "run-disk",
          originalName: "pixel.png",
          mediaType: "image/png",
          bytes,
        }),
      /placeholder already exists/,
    );
    assert.throws(
      () => store.get("sess-disk", "run-disk", "missing.png"),
      /file not found/,
    );
    const aliased = store.put({
      sessionId: "sess-alias",
      runId: "run-alias",
      originalName: "pixel.png",
      mediaType: "image/png",
      bytes,
      placeholder: "invoice",
    });
    assert.equal(aliased.placeholder, "invoice");
    assert.deepEqual(store.get("sess-alias", "run-alias", "invoice").bytes, bytes);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("files-ts Register + put/list/get over temp UDS", async () => {
  const protoRoot = resolveProtoRoot();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "hsp-files-ts-"));
  const socketPath = path.join(dir, "files.sock");
  const dataDir = path.join(dir, "data");
  // Product path is UDS. grpc-js UDS bind is EACCES on many Windows hosts;
  // TCP loopback matches vamd's test HspEndpoint::Tcp.
  const listen =
    process.platform === "win32"
      ? { tcpBind: "127.0.0.1:0" as const }
      : { socketPath };

  const broker = await startFilesBroker({ ...listen, protoRoot, dataDir });
  const client = connectClient(brokerDialAddress(broker), protoRoot);
  const logs: string[] = [];
  const originalLog = console.log;
  console.log = (...args: unknown[]) => {
    logs.push(args.map(String).join(" "));
  };

  try {
    const registerRes = await unary(client.register.bind(client), {
      brokerId: BROKER_ID,
      brokerVersion: "0.1.0",
      manifest: {
        brokerId: BROKER_ID,
        trustTier: TRUST_TIER_OPERATOR,
        tools: FILE_TOOLS.map((t) => ({
          toolName: t.name,
          sideEffect: t.sideEffect,
          requiredSecrets: [],
        })),
      },
    });
    assert.equal(registerRes.error ?? null, null);

    const listRes = await unary(client.listTools.bind(client), {
      brokerId: BROKER_ID,
    });
    const tools = listRes.tools as { name?: string }[];
    const names = tools.map((t) => t.name);
    assert.deepEqual(names.sort(), ["files.get", "files.list", "files.put"].sort());

    const putRes = await unary(client.invoke.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
      toolName: "files.put",
      args: structArg({
        name: "pixel.png",
        media_type: "image/png",
        content_base64: PNG_B64,
      }),
      resolvedSecrets: {},
    });

    const putErr = (putRes.result as { error?: { message?: string } | null })?.error;
    assert.equal(putErr ?? null, null, `put error: ${JSON.stringify(putErr)}`);
    const putFields = (putRes.result as { output?: { fields?: Record<string, { stringValue?: string; numberValue?: number }> } })
      ?.output?.fields;
    assert.ok(putFields);
    const fileId = putFields.file_id?.stringValue ?? "";
    assert.ok(fileId.startsWith("urn:vam:file:local.vam.dev/files-ts/"));
    assert.equal(putFields.placeholder?.stringValue, "pixel.png");
    assert.equal(putFields.rel_path, undefined);
    const listedOnDisk = broker.store.list("sess-test", "run-test");
    assert.equal(listedOnDisk.length, 1);
    const absBlob = path.join(dataDir, ...listedOnDisk[0].relPath.split("/"));
    assert.ok(fs.existsSync(absBlob), `missing blob at ${absBlob}`);
    assert.ok(fs.statSync(absBlob).size > 0);

    const listFilesRes = await unary(client.invoke.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
      toolName: "files.list",
      args: { fields: {} },
    });
    const listFields = (listFilesRes.result as { output?: { fields?: Record<string, { numberValue?: number }> } })
      ?.output?.fields;
    assert.equal(listFields?.count?.numberValue, 1);

    const getRes = await unary(client.invoke.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
      toolName: "files.get",
      args: structArg({ placeholder: "pixel.png" }),
    });
    const getFields = (getRes.result as {
      output?: {
        fields?: Record<
          string,
          {
            stringValue?: string;
            listValue?: { values?: { numberValue?: number }[] };
          }
        >;
      };
    })?.output?.fields;
    assert.equal(getFields?.placeholder?.stringValue, "pixel.png");
    assert.equal(getFields?.content_base64, undefined);
    const bufferNums = (getFields?.buffer?.listValue?.values ?? []).map((v) => v.numberValue ?? 0);
    assert.deepEqual(Buffer.from(bufferNums), Buffer.from(PNG_B64, "base64"));

    const getByName = await unary(client.invoke.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
      toolName: "files.get",
      args: structArg({ name: "pixel.png" }),
    });
    const getByNameErr = (getByName.result as { error?: { message?: string } | null })?.error;
    assert.equal(getByNameErr ?? null, null);

    const getById = await unary(client.invoke.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
      toolName: "files.get",
      args: structArg({ file_id: fileId }),
    });
    const getByIdErr = (getById.result as { error?: { message?: string } | null })?.error;
    assert.equal(getByIdErr ?? null, null);

    const missing = await unary(client.invoke.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
      toolName: "files.get",
      args: structArg({ placeholder: "no-such.png" }),
    });
    const missingErr = (missing.result as { error?: { message?: string } | null })?.error;
    assert.ok(missingErr?.message?.includes("file not found"));

    const downloaded = await collectDownload(
      client.download({
        sessionId: "sess-test",
        runId: "run-test",
        fileId: "pixel.png",
        resolvedSecrets: {},
      }),
    );
    assert.equal(downloaded.mediaType, "image/png");
    assert.equal(downloaded.fileName, "pixel.png");
    assert.equal(downloaded.sizeBytes, Buffer.from(PNG_B64, "base64").length);
    assert.deepEqual(downloaded.bytes, Buffer.from(PNG_B64, "base64"));

    const hookRes = await unary(client.onRequest.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
    });
    const injected = hookRes.injectedContext as { content?: string }[] | undefined;
    assert.ok(
      (injected ?? []).some((block) => (block.content ?? "").includes("pixel.png")),
      `expected placeholder in onRequest context: ${JSON.stringify(injected)}`,
    );
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

    const unknown = await unary(client.invoke.bind(client), {
      sessionId: "sess-test",
      runId: "run-test",
      toolName: "files.delete",
      args: { fields: {} },
    });
    const unknownErr = (unknown.result as { error?: { message?: string } | null })?.error;
    assert.ok(unknownErr?.message?.includes("unknown tool"));

    const joined = logs.join("\n");
    for (const rpc of [
      "register",
      "listTools",
      "invoke",
      "onRequest",
      "onComplete",
      "onResponse",
      "download",
    ]) {
      assert.ok(joined.includes(`[hsp] ${rpc}`), `missing log for ${rpc}: ${joined}`);
    }
    assert.ok(joined.includes("files.put"));
    assert.match(joined, /\[hsp\] download stream .*chunks.:[1-9]/);
    assert.ok(joined.includes("sess-test"));
  } finally {
    console.log = originalLog;
    client.close();
    stopFilesBroker(broker);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("resolveStorageBackend reads HSP_FILES_BACKEND", () => {
  assert.equal(resolveStorageBackend({}), "local");
  assert.equal(resolveStorageBackend({ HSP_FILES_BACKEND: "s3" }), "s3");
  assert.equal(resolveStorageBackend({ HSP_FILES_BACKEND: "oci" }), "oci");
  assert.equal(resolveStorageBackend({ HSP_FILES_BACKEND: "disk" }), "local");
});

test("parseOciConfig / parseS3Config require cloud secrets", () => {
  assert.throws(() => parseOciConfig({}), /missing secrets/);
  assert.throws(() => parseS3Config({}), /missing secrets/);
});

test("OciBlobStore put/list/get against memory transport", async () => {
  const { privateKey } = crypto.generateKeyPairSync("rsa", { modulusLength: 2048 });
  const pem = privateKey.export({ type: "pkcs8", format: "pem" }).toString();
  const transport = memoryOciTransport();
  const store = OciBlobStore.fromSecrets(
    {
      "oci-tenancy": "ocid1.tenancy.oc1..test",
      "oci-user": "ocid1.user.oc1..test",
      "oci-fingerprint": "aa:bb",
      "oci-private-key": pem,
      "oci-region": "us-ashburn-1",
      "oci-namespace": "nspace",
      "oci-bucket": "attachments",
    },
    transport,
  );
  const bytes = Buffer.from(PNG_B64, "base64");
  const meta = await store.put({
    sessionId: "sess-oci",
    runId: "run-oci",
    originalName: "pixel.png",
    mediaType: "image/png",
    bytes,
  });
  assert.ok(meta.relPath.includes("sess-oci/run-oci/"));
  assert.equal(meta.placeholder, "pixel.png");
  const listed = await store.list("sess-oci", "run-oci");
  assert.equal(listed.length, 1);
  const got = await store.get("sess-oci", "run-oci", "pixel.png");
  assert.deepEqual(got.bytes, bytes);
  const streamed = await store.streamGet("sess-oci", "run-oci", meta.fileId);
  const parts: Buffer[] = [];
  for await (const chunk of streamed.chunks) {
    parts.push(chunk);
  }
  assert.deepEqual(Buffer.concat(parts), bytes);
});

test("S3BlobStore put/list/get against memory transport", async () => {
  const transport = memoryS3Transport();
  const store = S3BlobStore.fromSecrets(
    {
      "s3-access-key": "AKIAEXAMPLE",
      "s3-secret-key": "secret",
      "s3-region": "us-east-1",
      "s3-bucket": "attachments",
    },
    transport,
  );
  const bytes = Buffer.from(PNG_B64, "base64");
  const meta = await store.put({
    sessionId: "sess-s3",
    runId: "run-s3",
    originalName: "pixel.png",
    mediaType: "image/png",
    bytes,
  });
  assert.ok(meta.relPath.includes("sess-s3/run-s3/"));
  const listed = await store.list("sess-s3", "run-s3");
  assert.equal(listed.length, 1);
  assert.equal(meta.placeholder, "pixel.png");
  const got = await store.get("sess-s3", "run-s3", "pixel.png");
  assert.deepEqual(got.bytes, bytes);
  const streamed = await store.streamGet("sess-s3", "run-s3", meta.fileId);
  const parts: Buffer[] = [];
  for await (const chunk of streamed.chunks) {
    parts.push(chunk);
  }
  assert.deepEqual(Buffer.concat(parts), bytes);
});
