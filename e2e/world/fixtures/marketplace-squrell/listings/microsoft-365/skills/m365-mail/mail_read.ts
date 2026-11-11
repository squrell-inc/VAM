// mail_read: GET /v1.0/me/messages/{id} (a read).
import { graph, runTool, messageId, sender } from "./lib/graph.ts";

const MAX_BODY = 20000;

export function shape(m: any) {
  const body = String(m?.body?.content ?? "");
  return {
    id: m?.id ?? "",
    from: sender(m?.from),
    to: (m?.toRecipients ?? []).map((r: any) => r?.emailAddress?.address).filter(Boolean),
    cc: (m?.ccRecipients ?? []).map((r: any) => r?.emailAddress?.address).filter(Boolean),
    subject: m?.subject ?? "",
    receivedAt: m?.receivedDateTime ?? "",
    conversationId: m?.conversationId ?? "",
    body: body.slice(0, MAX_BODY),
    truncated: body.length > MAX_BODY,
  };
}

export async function mailRead(args: Record<string, any>) {
  const id = messageId(args);
  const select = "id,subject,from,toRecipients,ccRecipients,receivedDateTime,conversationId,body";
  const { json } = await graph("GET", `/v1.0/me/messages/${id}?$select=${select}`, undefined, {
    Prefer: 'outlook.body-content-type="text"',
  });
  return shape(json);
}

if (import.meta.main) await runTool(mailRead);
