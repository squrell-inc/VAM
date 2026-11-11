/**
 * S3 / MinIO backend. Credentials from Invoke `resolved_secrets` only.
 * Optional `s3-endpoint` selects path-style (MinIO); otherwise virtual-hosted AWS.
 */

import * as crypto from "node:crypto";
import * as https from "node:https";
import * as http from "node:http";
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

export const S3_SECRET_KEYS = [
  "s3-access-key",
  "s3-secret-key",
  "s3-region",
  "s3-bucket",
] as const;

export type S3Config = {
  accessKey: string;
  secretKey: string;
  region: string;
  bucket: string;
  prefix: string;
  endpoint?: string;
};

export type S3HttpResponse = {
  status: number;
  body: Buffer;
};

export type S3StreamResponse = {
  status: number;
  chunks: AsyncIterable<Buffer>;
};

export type S3Transport = {
  request(options: {
    method: string;
    url: URL;
    headers: Record<string, string>;
    body?: Buffer;
  }): Promise<S3HttpResponse>;
  stream(options: {
    method: string;
    url: URL;
    headers: Record<string, string>;
    body?: Buffer;
  }): Promise<S3StreamResponse>;
};

export function parseS3Config(
  secrets: Record<string, string>,
  prefix = process.env.HSP_S3_PREFIX ?? "vam/files-ts/",
): S3Config {
  const missing = S3_SECRET_KEYS.filter((key) => !secrets[key]?.trim());
  if (missing.length > 0) {
    throw new Error(`S3 backend missing secrets: ${missing.join(", ")}`);
  }
  const endpoint = secrets["s3-endpoint"]?.trim();
  return {
    accessKey: secrets["s3-access-key"].trim(),
    secretKey: secrets["s3-secret-key"].trim(),
    region: secrets["s3-region"].trim(),
    bucket: secrets["s3-bucket"].trim(),
    prefix: prefix.endsWith("/") ? prefix : `${prefix}/`,
    endpoint: endpoint && endpoint.length > 0 ? endpoint : undefined,
  };
}

function hmac(key: Buffer | string, data: string): Buffer {
  return crypto.createHmac("sha256", key).update(data, "utf8").digest();
}

function sha256Hex(data: Buffer | string): string {
  return crypto.createHash("sha256").update(data).digest("hex");
}

function encodeS3Key(key: string): string {
  return key
    .split("/")
    .map((part) => encodeURIComponent(part))
    .join("/");
}

function amzDate(now = new Date()): { amz: string; datestamp: string } {
  const iso = now.toISOString().replace(/[:-]|\.\d{3}/g, "");
  return { amz: iso, datestamp: iso.slice(0, 8) };
}

export function s3ObjectUrl(config: S3Config, key: string, query = ""): URL {
  const encoded = encodeS3Key(key);
  if (config.endpoint) {
    const base = config.endpoint.includes("://")
      ? config.endpoint
      : `https://${config.endpoint}`;
    return new URL(
      `/${encodeURIComponent(config.bucket)}/${encoded}${query}`,
      base.endsWith("/") ? base : `${base}/`,
    );
  }
  return new URL(`https://${config.bucket}.s3.${config.region}.amazonaws.com/${encoded}${query}`);
}

export function signS3Headers(input: {
  config: S3Config;
  method: string;
  url: URL;
  body?: Buffer;
  now?: Date;
}): Record<string, string> {
  const { amz, datestamp } = amzDate(input.now);
  const payloadHash = sha256Hex(input.body ?? Buffer.alloc(0));
  const headers: Record<string, string> = {
    host: input.url.host,
    "x-amz-date": amz,
    "x-amz-content-sha256": payloadHash,
  };
  if (input.body && input.method !== "GET") {
    headers["content-type"] = "application/octet-stream";
    headers["content-length"] = String(input.body.length);
  }
  const signedHeaderNames = ["host", "x-amz-content-sha256", "x-amz-date"];
  const canonicalHeaders = signedHeaderNames
    .map((n) => `${n}:${headers[n]}\n`)
    .join("");
  const canonicalQuery = [...input.url.searchParams.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
    .join("&");
  const canonicalRequest = [
    input.method,
    input.url.pathname,
    canonicalQuery,
    canonicalHeaders,
    signedHeaderNames.join(";"),
    payloadHash,
  ].join("\n");
  const scope = `${datestamp}/${input.config.region}/s3/aws4_request`;
  const stringToSign = [
    "AWS4-HMAC-SHA256",
    amz,
    scope,
    sha256Hex(canonicalRequest),
  ].join("\n");
  const kDate = hmac(`AWS4${input.config.secretKey}`, datestamp);
  const kRegion = hmac(kDate, input.config.region);
  const kService = hmac(kRegion, "s3");
  const kSigning = hmac(kService, "aws4_request");
  const signature = crypto
    .createHmac("sha256", kSigning)
    .update(stringToSign, "utf8")
    .digest("hex");
  headers.authorization = `AWS4-HMAC-SHA256 Credential=${input.config.accessKey}/${scope}, SignedHeaders=${signedHeaderNames.join(";")}, Signature=${signature}`;
  return headers;
}

export function httpsS3Transport(): S3Transport {
  return {
    request({ method, url, headers, body }) {
      const lib = url.protocol === "http:" ? http : https;
      return new Promise((resolve, reject) => {
        const req = lib.request(
          url,
          { method, headers },
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
    stream({ method, url, headers, body }) {
      const lib = url.protocol === "http:" ? http : https;
      return new Promise((resolve, reject) => {
        const req = lib.request(url, { method, headers }, (res) => {
          resolve({
            status: res.statusCode ?? 0,
            chunks: res as AsyncIterable<Buffer>,
          });
        });
        req.on("error", reject);
        if (body) {
          req.write(body);
        }
        req.end();
      });
    },
  };
}

export class S3BlobStore implements AttachmentStore {
  constructor(
    private readonly config: S3Config,
    private readonly transport: S3Transport = httpsS3Transport(),
  ) {}

  static fromSecrets(
    secrets: Record<string, string>,
    transport?: S3Transport,
  ): S3BlobStore {
    return new S3BlobStore(parseS3Config(secrets), transport ?? httpsS3Transport());
  }

  private scopePrefix(sessionId: string, runId: string): string {
    return `${this.config.prefix}${sanitizeSegment(sessionId, "_anon")}/${sanitizeSegment(runId, "_")}/`;
  }

  private async call(method: string, url: URL, body?: Buffer): Promise<S3HttpResponse> {
    const headers = signS3Headers({ config: this.config, method, url, body });
    const res = await this.transport.request({ method, url, headers, body });
    if (res.status < 200 || res.status >= 300) {
      const detail = res.body.toString("utf8").slice(0, 300);
      throw new Error(`S3 ${method} ${url.pathname} failed (${res.status}): ${detail}`);
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
    await this.call("PUT", s3ObjectUrl(this.config, relPath), input.bytes);
    await this.call(
      "PUT",
      s3ObjectUrl(this.config, `${relPath}.meta.json`),
      Buffer.from(JSON.stringify(meta), "utf8"),
    );
    return meta;
  }

  async list(sessionId: string, runId: string): Promise<StoredFileMeta[]> {
    const prefix = this.scopePrefix(sessionId, runId);
    const url = s3ObjectUrl(this.config, "", `?list-type=2&prefix=${encodeURIComponent(prefix)}`);
    const res = await this.call("GET", url);
    const xml = res.body.toString("utf8");
    const keys = [...xml.matchAll(/<Key>([^<]+)<\/Key>/g)].map((m) => m[1]);
    const metas = keys.filter((k) => k.endsWith(".meta.json"));
    const out: StoredFileMeta[] = [];
    for (const key of metas) {
      const metaRes = await this.call("GET", s3ObjectUrl(this.config, key));
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
    const listed = await this.list(sessionId, runId);
    const meta = lookupMeta(listed, fileId, sessionId, runId);
    const url = s3ObjectUrl(this.config, meta.relPath);
    const headers = signS3Headers({ config: this.config, method: "GET", url });
    const res = await this.transport.stream({ method: "GET", url, headers });
    if (res.status < 200 || res.status >= 300) {
      const detail = (await collectStream(res.chunks)).toString("utf8").slice(0, 300);
      throw new Error(`S3 GET ${url.pathname} failed (${res.status}): ${detail}`);
    }
    return { meta, chunks: res.chunks };
  }
}

function memoryObjectKey(url: URL): string {
  if (url.hostname.includes(".s3.")) {
    return decodeURIComponent(url.pathname.replace(/^\//, ""));
  }
  const parts = url.pathname.replace(/^\//, "").split("/");
  return decodeURIComponent(parts.slice(1).join("/"));
}

/** In-memory S3 stand-in for hermetic tests (no AWS). */
export function memoryS3Transport(): S3Transport & { objects: Map<string, Buffer> } {
  const objects = new Map<string, Buffer>();
  return {
    objects,
    async request({ method, url, body }) {
      if (method === "GET" && url.searchParams.get("list-type") === "2") {
        const prefix = url.searchParams.get("prefix") ?? "";
        const keys = [...objects.keys()].filter((k) => k.startsWith(prefix));
        const xml = `<ListBucketResult>${keys.map((k) => `<Key>${k}</Key>`).join("")}</ListBucketResult>`;
        return { status: 200, body: Buffer.from(xml, "utf8") };
      }
      const key = memoryObjectKey(url);
      if (method === "PUT") {
        objects.set(key, body ?? Buffer.alloc(0));
        return { status: 200, body: Buffer.alloc(0) };
      }
      if (method === "GET") {
        const found = objects.get(key);
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
