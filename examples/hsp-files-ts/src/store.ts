/**
 * Local-disk blob store for Phase 0 file-attachment broker.
 * OCI (Phase 3) swaps this module; HSP tool names stay the same.
 */

import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";

export const ALLOWED_MEDIA_TYPES = [
  "image/png",
  "image/jpeg",
  "image/webp",
] as const;

export type AllowedMediaType = (typeof ALLOWED_MEDIA_TYPES)[number];

export type StoredFileMeta = {
  fileId: string;
  /** Client-facing handle stored in the broker; get/download resolve this to a locator. */
  placeholder: string;
  objectName: string;
  originalName: string;
  mediaType: AllowedMediaType;
  sizeBytes: number;
  relPath: string;
  sessionId: string;
  runId: string;
};

const EXT_BY_MEDIA: Record<AllowedMediaType, string> = {
  "image/png": "png",
  "image/jpeg": "jpg",
  "image/webp": "webp",
};

const MAGIC: Record<AllowedMediaType, (bytes: Buffer) => boolean> = {
  "image/png": (b) => b.length >= 8 && b.subarray(0, 8).equals(
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  ),
  "image/jpeg": (b) => b.length >= 3 && b[0] === 0xff && b[1] === 0xd8 && b[2] === 0xff,
  "image/webp": (b) =>
    b.length >= 12 &&
    b.subarray(0, 4).toString("ascii") === "RIFF" &&
    b.subarray(8, 12).toString("ascii") === "WEBP",
};

const MAX_BYTES = 2 * 1024 * 1024;

/** gRPC Download chunk size. Put stays capped at MAX_BYTES; get streams. */
export const STREAM_CHUNK_BYTES = 64 * 1024;

export function isAllowedMediaType(value: string): value is AllowedMediaType {
  return (ALLOWED_MEDIA_TYPES as readonly string[]).includes(value);
}

export function decodeBase64Payload(raw: string): Buffer {
  const compact = raw.replace(/\s/g, "");
  const buf = Buffer.from(compact, "base64");
  if (buf.length === 0 && compact.length > 0) {
    throw new Error("content_base64 is not valid base64");
  }
  if (buf.toString("base64").replace(/=+$/, "") !== compact.replace(/=+$/, "")) {
    throw new Error("content_base64 is not valid base64");
  }
  return buf;
}

export function assertImageBytes(mediaType: AllowedMediaType, bytes: Buffer): void {
  if (bytes.length === 0) {
    throw new Error("image payload is empty");
  }
  if (bytes.length > MAX_BYTES) {
    throw new Error(`image exceeds ${MAX_BYTES} byte Phase 0 limit`);
  }
  if (!MAGIC[mediaType](bytes)) {
    throw new Error(`bytes do not match media_type ${mediaType}`);
  }
}

export type PutInput = {
  sessionId: string;
  runId: string;
  originalName: string;
  mediaType: AllowedMediaType;
  bytes: Buffer;
  /** Optional override; default is sanitized `originalName`. */
  placeholder?: string;
};

export type StreamGetResult = {
  meta: StoredFileMeta;
  chunks: AsyncIterable<Buffer>;
};

export interface AttachmentStore {
  put(input: PutInput): StoredFileMeta | Promise<StoredFileMeta>;
  list(sessionId: string, runId: string): StoredFileMeta[] | Promise<StoredFileMeta[]>;
  get(
    sessionId: string,
    runId: string,
    fileId: string,
  ):
    | { meta: StoredFileMeta; bytes: Buffer }
    | Promise<{ meta: StoredFileMeta; bytes: Buffer }>;
  streamGet(
    sessionId: string,
    runId: string,
    fileId: string,
  ): Promise<StreamGetResult>;
}

export async function collectStream(chunks: AsyncIterable<Buffer>): Promise<Buffer> {
  const parts: Buffer[] = [];
  for await (const chunk of chunks) {
    parts.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(parts);
}

export async function* chunkBuffer(
  bytes: Buffer,
  size = STREAM_CHUNK_BYTES,
): AsyncGenerator<Buffer> {
  if (bytes.length === 0) {
    return;
  }
  for (let offset = 0; offset < bytes.length; offset += size) {
    yield bytes.subarray(offset, Math.min(offset + size, bytes.length));
  }
}

export function sanitizeSegment(value: string, fallback: string): string {
  const trimmed = value.trim();
  const cleaned = trimmed.replace(/[^a-zA-Z0-9._-]/g, "_");
  return cleaned.length > 0 ? cleaned.slice(0, 128) : fallback;
}

/** Placeholder stored on put; default is the sanitized original name. */
export function resolvePutPlaceholder(
  explicit: string | undefined,
  originalName: string,
  objectName: string,
): string {
  const raw = explicit?.trim() || originalName;
  return sanitizeSegment(raw, objectName);
}

/** Older meta without `placeholder` still resolves via originalName. */
export function filePlaceholder(meta: StoredFileMeta): string {
  if (typeof meta.placeholder === "string" && meta.placeholder.length > 0) {
    return meta.placeholder;
  }
  return meta.originalName;
}

export function matchesHandle(meta: StoredFileMeta, handle: string): boolean {
  const sanitized = sanitizeSegment(handle, handle);
  return (
    filePlaceholder(meta) === handle ||
    filePlaceholder(meta) === sanitized ||
    meta.fileId === handle ||
    meta.objectName === handle
  );
}

export function fileNotFoundMessage(
  handle: string,
  sessionId: string,
  runId: string,
  listed: StoredFileMeta[],
): string {
  const placeholders = listed.map((m) => filePlaceholder(m)).filter((p) => p.length > 0);
  if (placeholders.length === 0) {
    return `file not found: ${handle} (no files in session=${sessionId} run=${runId})`;
  }
  return `file not found: ${handle} (session=${sessionId} run=${runId}; known placeholders: ${placeholders.join(", ")})`;
}

export function lookupMeta(
  listed: StoredFileMeta[],
  handle: string,
  sessionId = "",
  runId = "",
): StoredFileMeta {
  const meta = listed.find((m) => matchesHandle(m, handle));
  if (!meta) {
    throw new Error(fileNotFoundMessage(handle, sessionId, runId, listed));
  }
  return meta;
}

export function assertPlaceholderAvailable(
  listed: StoredFileMeta[],
  placeholder: string,
): void {
  if (listed.some((m) => filePlaceholder(m) === placeholder)) {
    throw new Error(`placeholder already exists: ${placeholder}`);
  }
}

export function contentAddressedName(mediaType: AllowedMediaType, bytes: Buffer): {
  objectName: string;
  fileId: string;
  digest: string;
} {
  const digest = crypto.createHash("sha256").update(bytes).digest("hex");
  const ext = EXT_BY_MEDIA[mediaType];
  const objectName = `${digest}.${ext}`;
  return {
    objectName,
    fileId: `urn:vam:file:local.vam.dev/files-ts/${objectName}`,
    digest,
  };
}

export class LocalBlobStore implements AttachmentStore {
  constructor(public readonly dataDir: string) {
    fs.mkdirSync(this.dataDir, { recursive: true });
  }

  scopeDir(sessionId: string, runId: string): string {
    const sess = sanitizeSegment(sessionId, "_anon");
    const run = sanitizeSegment(runId, "_");
    return path.join(this.dataDir, sess, run);
  }

  put(input: PutInput): StoredFileMeta {
    assertImageBytes(input.mediaType, input.bytes);
    const { objectName, fileId } = contentAddressedName(input.mediaType, input.bytes);
    const placeholder = resolvePutPlaceholder(
      input.placeholder,
      input.originalName,
      objectName,
    );
    assertPlaceholderAvailable(this.list(input.sessionId, input.runId), placeholder);
    const dir = this.scopeDir(input.sessionId, input.runId);
    fs.mkdirSync(dir, { recursive: true });
    const absPath = path.join(dir, objectName);
    fs.writeFileSync(absPath, input.bytes);
    const relPath = path.relative(this.dataDir, absPath).split(path.sep).join("/");
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
    fs.writeFileSync(`${absPath}.meta.json`, JSON.stringify(meta, null, 2));
    return meta;
  }

  list(sessionId: string, runId: string): StoredFileMeta[] {
    const dir = this.scopeDir(sessionId, runId);
    if (!fs.existsSync(dir)) {
      return [];
    }
    const names = fs.readdirSync(dir).filter((n) => n.endsWith(".meta.json"));
    const out: StoredFileMeta[] = [];
    for (const name of names) {
      const raw = fs.readFileSync(path.join(dir, name), "utf8");
      out.push(JSON.parse(raw) as StoredFileMeta);
    }
    out.sort((a, b) => a.objectName.localeCompare(b.objectName));
    return out;
  }

  lookup(sessionId: string, runId: string, handle: string): StoredFileMeta {
    return lookupMeta(this.list(sessionId, runId), handle, sessionId, runId);
  }

  blobPath(meta: StoredFileMeta): string {
    const absPath = path.join(this.dataDir, ...meta.relPath.split("/"));
    if (!fs.existsSync(absPath)) {
      throw new Error(`blob missing on disk: ${meta.relPath}`);
    }
    return absPath;
  }

  get(sessionId: string, runId: string, fileId: string): { meta: StoredFileMeta; bytes: Buffer } {
    const meta = this.lookup(sessionId, runId, fileId);
    return { meta, bytes: fs.readFileSync(this.blobPath(meta)) };
  }

  async streamGet(
    sessionId: string,
    runId: string,
    fileId: string,
  ): Promise<StreamGetResult> {
    const meta = this.lookup(sessionId, runId, fileId);
    const stream = fs.createReadStream(this.blobPath(meta), {
      highWaterMark: STREAM_CHUNK_BYTES,
    });
    async function* chunks(): AsyncGenerator<Buffer> {
      for await (const part of stream) {
        yield Buffer.isBuffer(part) ? part : Buffer.from(part);
      }
    }
    return { meta, chunks: chunks() };
  }
}
