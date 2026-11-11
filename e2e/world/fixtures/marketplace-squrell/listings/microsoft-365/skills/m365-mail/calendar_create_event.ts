// calendar_create_event: POST /v1.0/me/events. Always asks first (book_approval card).
// Graph's `transactionId` makes a repeated create with the same arguments return the same
// event instead of a second one, so the tool declares effect.class IDEMPOTENT. The id is a
// SHA-256 of the canonical arguments.
import { createHash } from "node:crypto";
import { graph, runTool, text, localDateTime, addresses, ordered, ToolError } from "./lib/graph.ts";

export function transactionId(parts: unknown): string {
  return createHash("sha256").update(JSON.stringify(parts)).digest("hex").slice(0, 32);
}

export function eventBody(args: Record<string, any>) {
  const subject = text(args, "subject", true, 500);
  const start = localDateTime(args, "start", true);
  const end = localDateTime(args, "end", true);
  ordered(start, end);
  const timeZone = text(args, "timeZone", false, 60) || "UTC";
  if (!/^[A-Za-z0-9_+\-/ ]+$/.test(timeZone)) throw new ToolError("'timeZone' is not a time zone name.");
  const attendees = addresses(args.attendees, "attendees");
  const location = text(args, "location", false, 300);
  const notes = text(args, "body", false);
  return {
    subject,
    start: { dateTime: start, timeZone },
    end: { dateTime: end, timeZone },
    ...(location ? { location: { displayName: location } } : {}),
    ...(notes ? { body: { contentType: "Text", content: notes } } : {}),
    attendees: attendees.map((address) => ({ type: "required", emailAddress: { address } })),
    transactionId: transactionId({ subject, start, end, timeZone, attendees, location, notes }),
  };
}

export async function createEvent(args: Record<string, any>) {
  const body = eventBody(args);
  const { json } = await graph("POST", "/v1.0/me/events", body);
  return {
    id: json?.id ?? "",
    subject: body.subject,
    start: body.start.dateTime,
    end: body.end.dateTime,
    timeZone: body.start.timeZone,
    created: true,
  };
}

if (import.meta.main) await runTool(createEvent);
