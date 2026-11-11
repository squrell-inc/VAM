// Tests for the m365-mail tools against recorded Microsoft Graph answers. A local server plays the
// guest's egress proxy: it checks what the tool sent (target URL, placeholder, body) and answers
// with a recorded response, so no network and no token are involved.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const skill = join(import.meta.dir, "../../listings/microsoft-365/skills/m365-mail");
const recorded = (name: string) => JSON.parse(readFileSync(join(import.meta.dir, "recorded", name), "utf8"));

type Seen = { method: string; target: string; auth: string; prefer: string; body: any };
let seen: Seen[] = [];
let reply: { status: number; body: unknown } = { status: 200, body: {} };
let server: ReturnType<typeof Bun.serve>;

beforeAll(() => {
  server = Bun.serve({
    port: 0,
    async fetch(req) {
      const text = await req.text();
      seen.push({
        method: req.method,
        target: req.headers.get("x-target-url") ?? "",
        auth: req.headers.get("authorization") ?? "",
        prefer: req.headers.get("prefer") ?? "",
        body: text ? JSON.parse(text) : null,
      });
      const body = reply.body === "" ? "" : JSON.stringify(reply.body);
      return new Response(body, { status: reply.status, headers: { "Content-Type": "application/json" } });
    },
  });
  process.env.VAM_EGRESS_ENDPOINT = `http://127.0.0.1:${server.port}`;
});
afterAll(() => server.stop(true));

function answer(status: number, body: unknown) {
  seen = [];
  reply = { status, body };
}

const mod = (name: string) => import(join(skill, name));

describe("mail_search", () => {
  test("searches with $search and shapes the messages", async () => {
    answer(200, recorded("messages.json"));
    const { mailSearch } = await mod("mail_search.ts");
    const out = await mailSearch({ query: "quarterly planning", top: 5 });
    expect(seen).toHaveLength(1);
    const url = new URL(seen[0].target);
    expect(url.origin).toBe("https://graph.microsoft.com");
    expect(url.pathname).toBe("/v1.0/me/messages");
    expect(url.searchParams.get("$search")).toBe('"quarterly planning"');
    expect(url.searchParams.get("$top")).toBe("5");
    expect(seen[0].auth).toBe("vamsec_m365_graph");
    expect(out.messages[0]).toEqual({
      id: "AAMkAGI2",
      from: "Ana Silva <ana@e2e.example>",
      subject: "Quarterly planning notes",
      receivedAt: "2026-11-09T07:12:00Z",
      preview: "Can we plan the quarter on Thursday?",
      unread: true,
    });
    expect(out.messages).toHaveLength(2);
  });

  test("without a query it filters by time, newest first", async () => {
    answer(200, recorded("messages.json"));
    const { searchPath } = await mod("mail_search.ts");
    const p = new URL("https://x" + searchPath({ sinceHours: 12 }, new Date("2026-11-09T12:00:00Z")));
    expect(p.searchParams.get("$filter")).toBe("receivedDateTime ge 2026-11-09T00:00:00.000Z");
    expect(p.searchParams.get("$orderby")).toBe("receivedDateTime desc");
    expect(p.searchParams.has("$search")).toBe(false);
  });

  test("rejects a bad size", async () => {
    const { mailSearch } = await mod("mail_search.ts");
    await expect(mailSearch({ top: 500 })).rejects.toThrow("'top'");
  });
});

describe("mail_read", () => {
  test("reads one message as text", async () => {
    answer(200, recorded("message.json"));
    const { mailRead } = await mod("mail_read.ts");
    const out = await mailRead({ id: "AAMkAGI2=" });
    expect(new URL(seen[0].target).pathname).toBe("/v1.0/me/messages/AAMkAGI2=");
    expect(seen[0].prefer).toContain("text");
    expect(out.to).toEqual(["member@e2e.example"]);
    expect(out.body).toContain("plan the quarter");
  });

  test("an id cannot add a path segment", async () => {
    const { mailRead } = await mod("mail_read.ts");
    await expect(mailRead({ id: "x/send" })).rejects.toThrow("not a message id");
    await expect(mailRead({ id: "../me" })).rejects.toThrow("not a message id");
  });
});

describe("calendar_list and calendar_find_slots", () => {
  test("calendar_list asks for the window and shapes events", async () => {
    answer(200, recorded("calendarView.json"));
    const { calendarList } = await mod("calendar_list.ts");
    const out = await calendarList({ start: "2026-11-10T00:00:00", end: "2026-11-11T00:00:00" });
    const url = new URL(seen[0].target);
    expect(url.pathname).toBe("/v1.0/me/calendarView");
    expect(url.searchParams.get("startDateTime")).toBe("2026-11-10T00:00:00");
    expect(out.events[0].subject).toBe("Team sync");
  });

  test("calendar_find_slots posts the constraint and shapes the slots", async () => {
    answer(200, recorded("findMeetingTimes.json"));
    const { findSlots } = await mod("calendar_find_slots.ts");
    const out = await findSlots({ durationMinutes: 45, attendees: "ana@e2e.example, bo@e2e.example", start: "2026-11-10T08:00:00", end: "2026-11-10T18:00:00" });
    expect(seen[0].method).toBe("POST");
    expect(new URL(seen[0].target).pathname).toBe("/v1.0/me/findMeetingTimes");
    expect(seen[0].body.meetingDuration).toBe("PT45M");
    expect(seen[0].body.attendees).toHaveLength(2);
    expect(out.slots).toEqual([
      { start: "2026-11-10T10:00:00.0000000", end: "2026-11-10T10:45:00.0000000", timeZone: "UTC" },
      { start: "2026-11-10T14:00:00.0000000", end: "2026-11-10T14:45:00.0000000", timeZone: "UTC" },
    ]);
  });
});

describe("writes", () => {
  test("mail_draft posts one message and sends nothing", async () => {
    answer(201, { id: "AAMkDRAFT", subject: "Planning" });
    const { mailDraft } = await mod("mail_draft.ts");
    const out = await mailDraft({ to: "ana@e2e.example", subject: "Planning", body: "Thursday works." });
    expect(seen).toHaveLength(1);
    expect(new URL(seen[0].target).pathname).toBe("/v1.0/me/messages");
    expect(seen[0].body.toRecipients).toEqual([{ emailAddress: { address: "ana@e2e.example" } }]);
    expect(out).toEqual({ id: "AAMkDRAFT", subject: "Planning", saved: true });
  });

  test("mail_send posts sendMail once and handles the empty 202", async () => {
    answer(202, "");
    const { mailSend } = await mod("mail_send.ts");
    const out = await mailSend({ to: "ana@e2e.example; bo@e2e.example", subject: "Planning", body: "Thursday works." });
    expect(seen).toHaveLength(1);
    expect(new URL(seen[0].target).pathname).toBe("/v1.0/me/sendMail");
    expect(seen[0].body.message.toRecipients).toHaveLength(2);
    expect(seen[0].body.saveToSentItems).toBe(true);
    expect(out).toEqual({ sent: true, to: "ana@e2e.example, bo@e2e.example", subject: "Planning" });
  });

  test("mail_send refuses a bad address before any request", async () => {
    answer(202, "");
    const { mailSend } = await mod("mail_send.ts");
    await expect(mailSend({ to: "not-an-address", subject: "x", body: "y" })).rejects.toThrow("not an email address");
    expect(seen).toHaveLength(0);
  });

  test("calendar_create_event carries a transactionId derived from the arguments", async () => {
    const { createEvent, eventBody } = await mod("calendar_create_event.ts");
    const args = { subject: "Planning", start: "2026-11-10T10:00:00", end: "2026-11-10T10:30:00", attendees: "ana@e2e.example" };
    answer(201, { id: "AAMkEVENT" });
    const out = await createEvent(args);
    expect(new URL(seen[0].target).pathname).toBe("/v1.0/me/events");
    expect(seen[0].body.transactionId).toMatch(/^[0-9a-f]{32}$/);
    expect(seen[0].body.transactionId).toBe(eventBody(args).transactionId);
    expect(eventBody({ ...args, end: "2026-11-10T11:00:00" }).transactionId).not.toBe(seen[0].body.transactionId);
    expect(out).toMatchObject({ id: "AAMkEVENT", created: true, timeZone: "UTC" });
  });

  test("calendar_create_event refuses an end before the start", async () => {
    const { eventBody } = await mod("calendar_create_event.ts");
    expect(() => eventBody({ subject: "x", start: "2026-11-10T10:00:00", end: "2026-11-10T09:00:00" })).toThrow("'end' must be after 'start'");
  });
});

describe("failures", () => {
  test("a Graph error becomes a readable ToolError without a token", async () => {
    answer(401, { error: { code: "InvalidAuthenticationToken", message: "Access token has expired." } });
    const { mailSearch } = await mod("mail_search.ts");
    await expect(mailSearch({ query: "x" })).rejects.toThrow("answered 401 (InvalidAuthenticationToken: Access token has expired.)");
  });
});

describe("as a process", () => {
  test("a tool reads stdin, prints JSON and holds only the placeholder", async () => {
    answer(200, recorded("messages.json"));
    const proc = Bun.spawn(["bun", join(skill, "mail_search.ts")], {
      stdin: new Blob([JSON.stringify({ query: "planning" })]),
      stdout: "pipe",
      stderr: "pipe",
      env: { PATH: process.env.PATH ?? "", VAM_EGRESS_ENDPOINT: process.env.VAM_EGRESS_ENDPOINT ?? "" },
    });
    const out = await new Response(proc.stdout).text();
    expect(await proc.exited).toBe(0);
    expect(JSON.parse(out).messages).toHaveLength(2);
    expect(seen[0].auth).toBe("vamsec_m365_graph");
  });

  test("a failure exits 1 with the error on stderr", async () => {
    answer(500, { error: { code: "boom", message: "no" } });
    const proc = Bun.spawn(["bun", join(skill, "mail_read.ts")], {
      stdin: new Blob([JSON.stringify({ id: "abc" })]),
      stdout: "pipe",
      stderr: "pipe",
      env: { PATH: process.env.PATH ?? "", VAM_EGRESS_ENDPOINT: process.env.VAM_EGRESS_ENDPOINT ?? "" },
    });
    const err = await new Response(proc.stderr).text();
    expect(await proc.exited).toBe(1);
    expect(JSON.parse(err).error).toContain("answered 500");
  });
});
