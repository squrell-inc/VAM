/**
 * OCI Object Storage backend (Phase 3). Credentials come from Invoke
 * `resolved_secrets` only — never from a committed .env.
 */

import * as crypto from "node:crypto";
import * as https from "node:https";
import {
  assertImageBytes,
  chunkBuffer,
  collectStream,
  contentAddressedName,
  lookupMeta,
  resolvePutPlaceholder,
  sanitizeSegment,
  assertPlaceholderAvailable,
  type AttachmentStore,
  type PutInput,
  type StoredFileMeta,
  type StreamGetResult,
} from "./store";

export const OCI_SECRET_KEYS = [
  "oci-tenancy",
  "oci-user",
  "oci-fingerprint",
  "oci-private-key",
  "oci-region",
  "oci-namespace",
  "oci-bucket",
] as const;

export type OciSecretKey = (typeof OCI_SECRET_KEYS)[number];

export type OciConfig = {
  tenancy: string;
  user: string;
  fingerprint: string;
  privateKeyPem: string;
  region: string;
  namespace: string;
  bucket: string;
  prefix: string;
};

export type OciHttpResponse = {
  status: number;
  body: Buffer;
};

export type OciStreamResponse = {
  status: number;
  chunks: AsyncIterable<Buffer>;
};

export type OciTransport = {
  request(options: {
    method: string;
    host: string;
    path: string;
    headers: Record<string, string>;
    body?: Buffer;
  }): Promise<OciHttpResponse>;
  stream(options: {
    method: string;
    host: string;
    path: string;
    headers: Record<string, string>;
    body?: Buffer;
  }): Promise<OciStreamResponse>;
};

export function objectStorageHost(region: string): string {
  return `objectstorage.${region}.oraclecloud.com`;
}

export function parseOciConfig(
  secrets: Record<string, string>,
  prefix = process.env.HSP_OCI_PREFIX ?? "vam/files-ts/",
): OciConfig {
  const missing = OCI_SECRET_KEYS.filter((key) => !secrets[key]?.trim());
  if (missing.length > 0) {
    throw new Error(`OCI backend missing secrets: ${missing.join(", ")}`);
  }
  const pem = secrets["oci-private-key"].replace(/\\n/g, "\n");
  return {
    tenancy: secrets["oci-tenancy"].trim(),
    user: secrets["oci-user"].trim(),
    fingerprint: secrets["oci-fingerprint"].trim(),
    privateKeyPem: pem,
    region: secrets["oci-region"].trim(),
    namespace: secrets["oci-namespace"].trim(),
    bucket: secrets["oci-bucket"].trim(),
    prefix: prefix.endsWith("/") ? prefix : `${prefix}/`,
  };
}

function encodeObjectName(name: string): string {
  return name
    .split("/")
    .map((part) => encodeURIComponent(part))
    .join("/");
}

export function signOciHeaders(input: {
  config: OciConfig;
  method: string;
  path: string;
  host: string;
  body?: Buffer;
}): Record<string, string> {
  const method = input.method.toLowerCase();
  const date = new Date().toUTCString();
  const headers: Record<string, string> = {
    date,
    host: input.host,
  };
  const signingNames = ["date", "(request-target)", "host"];
  if (input.body && input.body.length >= 0 && method !== "get" && method !== "head") {
    const digest = crypto.createHash("sha256").update(input.body).digest("base64");
    headers["x-content-sha256"] = digest;
    headers["content-type"] = headers["content-type"] ?? "application/octet-stream";
    headers["content-length"] = String(input.body.length);
    signingNames.push("x-content-sha256", "content-type", "content-length");
  }
  const requestTarget = `${method} ${input.path}`;
  const signingLines = signingNames.map((name) => {
    if (name === "(request-target)") {
      return `(request-target): ${requestTarget}`;
    }
    return `${name}: ${headers[name]}`;
  });
  const signingString = signingLines.join("\n");
  const signer = crypto.createSign("RSA-SHA256");
  signer.update(signingString);
  const signature = signer.sign(input.config.privateKeyPem, "base64");
  const keyId = `${input.config.tenancy}/${input.config.user}/${input.config.fingerprint}`;
  headers.authorization = `Signature version="1",keyId="${keyId}",algorithm="rsa-sha256",headers="${signingNames.join(" ")}",signature="${signature}"`;
  return headers;
}

export function httpsTransport(): OciTransport {
  return {
    request({ method, host, path: urlPath, headers, body }) {
      return new Promise((resolve, reject) => {
        const req = https.request(
          {
            hostname: host,
            method,
            path: urlPath,
            headers,
          },
          (res) => {
            const chunks: Buffer[] = [];
            res.on("data", (c) => chunks.push(c as Buffer));
            res.on("end", () => {
              resolve({
                status: res.statusCode ?? 0,
                body: Buffer.concat(chunks),
              });
            });
          },
        );
        req.on("error", reject);
        if (body) {
          req.write(body);
        }
        req.end();
      });
    },
    stream({ method, host, path: urlPath, headers, body }) {
      return new Promise((resolve, reject) => {
        const req = https.request(
          {
            hostname: host,
            method,
            path: urlPath,
            headers,
          },
          (res) => {
            resolve({
              status: res.statusCode ?? 0,
              chunks: res as AsyncIterable<Buffer>,
            });
          },
        );
        req.on("error", reject);
        if (body) {
          req.write(body);
        }
        req.end();
      });
    },
  };
}

export class OciBlobStore implements AttachmentStore {
  constructor(
    private readonly config: OciConfig,
    private readonly transport: OciTransport = httpsTransport(),
  ) {}

  static fromSecrets(
    secrets: Record<string, string>,
    transport?: OciTransport,
  ): OciBlobStore {
    return new OciBlobStore(parseOciConfig(secrets), transport ?? httpsTransport());
  }

  private host(): string {
    return objectStorageHost(this.config.region);
  }

  private objectPath(objectName: string): string {
    return `/n/${encodeURIComponent(this.config.namespace)}/b/${encodeURIComponent(this.config.bucket)}/o/${encodeObjectName(objectName)}`;
  }

  private scopePrefix(sessionId: string, runId: string): string {
    const sess = sanitizeSegment(sessionId, "_anon");
    const run = sanitizeSegment(runId, "_");
    return `${this.config.prefix}${sess}/${run}/`;
  }

  private async call(
    method: string,
    urlPath: string,
    body?: Buffer,
  ): Promise<OciHttpResponse> {
    const host = this.host();
    const headers = signOciHeaders({
      config: this.config,
      method,
      path: urlPath,
      host,
      body,
    });
    const res = await this.transport.request({
      method,
      host,
      path: urlPath,
      headers,
      body,
    });
    if (res.status < 200 || res.status >= 300) {
      const detail = res.body.toString("utf8").slice(0, 300);
      throw new Error(`OCI Object Storage ${method} ${urlPath} failed (${res.status}): ${detail}`);
    }
    return res;
  }

  async put(input: PutInput): Promise<StoredFileMeta> {
    assertImageBytes(input.mediaType, input.bytes);
    const { objectName, fileId } = contentAddressedName(input.mediaType, input.bytes);
    const placeholder = resolvePutPlaceholder(
      input.placeholder,
      input.originalName,
      objectName,
    );
    assertPlaceholderAvailable(await this.list(input.sessionId, input.runId), placeholder);
    const relPath = `${this.scopePrefix(input.sessionId, input.runId)}${objectName}`;
    const meta: StoredFileMeta = {
      fileId,
      placeholder,
      objectName,
      originalName: sanitizeSegment(input.originalName, objectName),
      mediaType: input.mediaType,
      sizeBytes: input.bytes.length,
      relPath,
      sessionId: sanitizeSegment(input.sessionId, "_anon"),
      runId: sanitizeSegment(input.runId, "_"),
    };
    await this.call("PUT", this.objectPath(relPath), input.bytes);
    await this.call(
      "PUT",
      this.objectPath(`${relPath}.meta.json`),
      Buffer.from(JSON.stringify(meta), "utf8"),
    );
    return meta;
  }

  async list(sessionId: string, runId: string): Promise<StoredFileMeta[]> {
    const prefix = this.scopePrefix(sessionId, runId);
    const urlPath = `/n/${encodeURIComponent(this.config.namespace)}/b/${encodeURIComponent(this.config.bucket)}/o?prefix=${encodeURIComponent(prefix)}&fields=name`;
    const res = await this.call("GET", urlPath);
    const parsed = JSON.parse(res.body.toString("utf8") || "{}") as {
      objects?: { name?: string }[];
    };
    const names = (parsed.objects ?? [])
      .map((o) => o.name ?? "")
      .filter((n) => n.endsWith(".meta.json"));
    const out: StoredFileMeta[] = [];
    for (const name of names) {
      const metaRes = await this.call("GET", this.objectPath(name));
      out.push(JSON.parse(metaRes.body.toString("utf8")) as StoredFileMeta);
    }
    out.sort((a, b) => a.objectName.localeCompare(b.objectName));
    return out;
  }

  async get(
    sessionId: string,
    runId: string,
    fileId: string,
  ): Promise<{ meta: StoredFileMeta; bytes: Buffer }> {
    const streamed = await this.streamGet(sessionId, runId, fileId);
    return { meta: streamed.meta, bytes: await collectStream(streamed.chunks) };
  }

  async streamGet(
    sessionId: string,
    runId: string,
    fileId: string,
  ): Promise<StreamGetResult> {
    const list = await this.list(sessionId, runId);
    const meta = lookupMeta(list, fileId, sessionId, runId);
    const host = this.host();
    const urlPath = this.objectPath(meta.relPath);
    const headers = signOciHeaders({
      config: this.config,
      method: "GET",
      path: urlPath,
      host,
    });
    const res = await this.transport.stream({
      method: "GET",
      host,
      path: urlPath,
      headers,
    });
    if (res.status < 200 || res.status >= 300) {
      const detail = (await collectStream(res.chunks)).toString("utf8").slice(0, 300);
      throw new Error(`OCI Object Storage GET ${urlPath} failed (${res.status}): ${detail}`);
    }
    return { meta, chunks: res.chunks };
  }
}

/** In-memory Object Storage stand-in for hermetic Phase 3 tests. */
export function memoryOciTransport(): OciTransport & { objects: Map<string, Buffer> } {
  const objects = new Map<string, Buffer>();
  return {
    objects,
    async request({ method, path: urlPath, body }) {
      const objectMatch = urlPath.match(/\/o\/(.+)$/);
      if (method === "GET" && urlPath.includes("/o?") && urlPath.includes("prefix=")) {
        const prefix = decodeURIComponent(
          (urlPath.split("prefix=")[1] ?? "").split("&")[0] ?? "",
        );
        const listed = [...objects.keys()]
          .filter((k) => k.startsWith(prefix))
          .map((name) => ({ name }));
        return {
          status: 200,
          body: Buffer.from(JSON.stringify({ objects: listed }), "utf8"),
        };
      }
      const name = objectMatch ? decodeURIComponent(objectMatch[1]) : "";
      if (method === "PUT") {
        objects.set(name, body ?? Buffer.alloc(0));
        return { status: 200, body: Buffer.alloc(0) };
      }
      if (method === "GET") {
        const found = objects.get(name);
        if (!found) {
          return { status: 404, body: Buffer.from("not found") };
        }
        return { status: 200, body: found };
      }
      return { status: 405, body: Buffer.from("unsupported") };
    },
    async stream(options) {
      const res = await this.request(options);
      return { status: res.status, chunks: chunkBuffer(res.body) };
    },
  };
}
