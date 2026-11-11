import { OciBlobStore } from "./oci";
import { S3BlobStore } from "./s3";
import type { AttachmentStore } from "./store";
import type { LocalBlobStore } from "./store";

export type StorageBackend = "local" | "s3" | "oci";

export function resolveStorageBackend(
  env: NodeJS.ProcessEnv = process.env,
): StorageBackend {
  const raw = (env.HSP_FILES_BACKEND ?? "local").trim().toLowerCase();
  if (raw === "s3" || raw === "oci") {
    return raw;
  }
  return "local";
}

/** Pick disk / S3 / OCI from env; cloud creds come from this invoke only. */
export function storeForInvoke(
  local: LocalBlobStore,
  secrets: Record<string, string>,
  env: NodeJS.ProcessEnv = process.env,
): AttachmentStore {
  switch (resolveStorageBackend(env)) {
    case "oci":
      return OciBlobStore.fromSecrets(secrets);
    case "s3":
      return S3BlobStore.fromSecrets(secrets);
    default:
      return local;
  }
}
