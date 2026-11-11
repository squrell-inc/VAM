// Shared helpers for the m365-mail tools. Runs in the guest under Bun.
//
// The tools hold no token. Every request goes to the in-guest egress proxy with the
// destination in `X-Target-Url` and only the connector's placeholder as the credential;
// the host swaps in the member's access token and enforces each tool's declared shape.

export const PLACEHOLDER = "vamsec_m365_graph";
export const GRAPH_HOST = "graph.microsoft.com";

/** A failure the member or the model can act on. The message never holds a token. */
export class ToolError extends Error {}

export function proxyEndpoint(): string {
  return process.env.VAM_EGRESS_ENDPOINT || "http://127.0.0.1:3128";
}

export interface GraphReply {
  status: number;
  json: any;
}

/**
 * One call to Microsoft Graph through the egress proxy. `pathAndQuery` starts with `/v1.0/`.
 * A non-2xx answer throws a ToolError carrying Graph's own error code and message.
 */
export async function graph(
  method: "GET" | "POST",
  pathAndQuery: string,
  body?: unknown,
  headers: Record<string, string> = {},
): Promise<GraphReply> {
  const res = await fetch(proxyEndpoint(), {
    method,
    headers: {
      "X-Target-Url": `https://${GRAPH_HOST}${pathAndQuery}`,
      Authorization: PLACEHOLDER,
      Accept: "application/json",
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      ...headers,
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json: any = null;
  if (text.trim()) {
    try {
      json = JSON.parse(text);
    } catch {
      json = null;
    }
  }
  if (res.status < 200 || res.status >= 300) {
    const code = json?.error?.code ? `${json.error.code}: ` : "";
    const message = json?.error?.message ?? text.slice(0, 300);
    throw new ToolError(`Microsoft Graph answered ${res.status} (${code}${message})`.trim());
  }
  return { status: res.status, json };
}

/** Reads the tool's arguments: one JSON object on stdin. */
export async function readArgs(): Promise<Record<string, any>> {
  const raw = (await Bun.stdin.text()).trim();
  if (!raw) return {};
  try {
    const v = JSON.parse(raw);
    return v && typeof v === "object" && !Array.isArray(v) ? v : {};
  } catch {
    throw new ToolError("The arguments were not a JSON object.");
  }
}

/** Runs one tool: reads stdin, prints the result as JSON, or the failure on stderr with exit 1. */
export async function runTool(
  tool: (args: Record<string, any>) => Promise<unknown>,
): Promise<void> {
  try {
    const result = await tool(await readArgs());
    console.log(JSON.stringify(result));
  } catch (e) {
    const message = e instanceof ToolError ? e.message : `Unexpected failure: ${(e as Error)?.message ?? e}`;
    console.error(JSON.stringify({ error: message }));
    process.exit(1);
  }
}

// ---- argument helpers ----------------------------------------------------------------------

export function text(args: Record<string, any>, key: string, required = false, max = 20000): string {
  const v = args[key];
  if (v === undefined || v === null || v === "") {
    if (required) throw new ToolError(`'${key}' is required.`);
    return "";
  }
  if (typeof v !== "string") throw new ToolError(`'${key}' must be text.`);
  if (v.length > max) throw new ToolError(`'${key}' is longer than ${max} characters.`);
  return v;
}

export function whole(args: Record<string, any>, key: string, fallback: number, min: number, max: number): number {
  const v = args[key];
  if (v === undefined || v === null) return fallback;
  if (typeof v !== "number" || !Number.isInteger(v) || v < min || v > max) {
    throw new ToolError(`'${key}' must be a whole number from ${min} to ${max}.`);
  }
  return v;
}

/** An ISO 8601 date-time as Graph's dateTimeTimeZone wants it: any offset is dropped (the zone is a separate field). */
export function localDateTime(args: Record<string, any>, key: string, required = false): string {
  const v = text(args, key, required, 40);
  if (!v) return "";
  const bare = v.replace(/(Z|[+-]\d{2}:\d{2})$/, "");
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?$/.test(bare) || Number.isNaN(Date.parse(bare + "Z"))) {
    throw new ToolError(`'${key}' must be a date and time like 2026-11-10T10:00:00.`);
  }
  return bare;
}

/** `end` must come after `start` (both from localDateTime). */
export function ordered(start: string, end: string): void {
  if (Date.parse(end + "Z") <= Date.parse(start + "Z")) throw new ToolError("'end' must be after 'start'.");
}

const ADDRESS = /^[^\s@<>,;]+@[^\s@<>,;]+\.[^\s@<>,;]+$/;

/** One or more addresses, separated by commas or semicolons (or a list). */
export function addresses(value: unknown, key: string, required = false): string[] {
  if (value === undefined || value === null || value === "") {
    if (required) throw new ToolError(`'${key}' is required.`);
    return [];
  }
  const parts = (Array.isArray(value) ? value.map(String) : String(value).split(/[;,]/))
    .map((p) => p.trim())
    .filter(Boolean);
  if (required && parts.length === 0) throw new ToolError(`'${key}' is required.`);
  if (parts.length > 50) throw new ToolError(`'${key}' has more than 50 addresses.`);
  for (const p of parts) if (!ADDRESS.test(p)) throw new ToolError(`'${p}' is not an email address.`);
  return parts;
}

export function recipients(list: string[]) {
  return list.map((address) => ({ emailAddress: { address } }));
}

export function sender(from: any): string {
  const e = from?.emailAddress;
  if (!e) return "";
  return e.name && e.address ? `${e.name} <${e.address}>` : e.address || e.name || "";
}

/** Graph ids are URL-safe base64 with `=` and `-`; nothing else is let into a path. */
export function messageId(args: Record<string, any>): string {
  const id = text(args, "id", true, 400);
  if (!/^[A-Za-z0-9_=-]+$/.test(id)) throw new ToolError("'id' is not a message id.");
  return id;
}

/** The window both calendar tools use: from `start` (default now) for seven days. */
export function window(args: Record<string, any>, now: Date): { start: string; end: string } {
  const start = localDateTime(args, "start") || now.toISOString().slice(0, 19);
  const end = localDateTime(args, "end") || new Date(Date.parse(start + "Z") + 7 * 24 * 3600_000).toISOString().slice(0, 19);
  ordered(start, end);
  return { start, end };
}
