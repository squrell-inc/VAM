// mail_draft: POST /v1.0/me/messages creates a draft in the member's Drafts folder. A draft
// reaches no one; sending it is a different tool that asks first.
import { graph, runTool, text, addresses, recipients } from "./lib/graph.ts";

export function draftBody(args: Record<string, any>) {
  return {
    subject: text(args, "subject", true, 500),
    body: { contentType: "Text", content: text(args, "body", true) },
    toRecipients: recipients(addresses(args.to, "to", true)),
    ccRecipients: recipients(addresses(args.cc, "cc")),
  };
}

export async function mailDraft(args: Record<string, any>) {
  const { json } = await graph("POST", "/v1.0/me/messages", draftBody(args));
  return { id: json?.id ?? "", subject: json?.subject ?? text(args, "subject"), saved: true };
}

if (import.meta.main) await runTool(mailDraft);
