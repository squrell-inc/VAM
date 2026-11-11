// mail_send: POST /v1.0/me/sendMail. The call always asks the member first, with the
// send_approval card. Graph answers 202 with no body and has no idempotency key, so a send
// whose answer was lost must ask again rather than repeat (the host journal guards that).
import { graph, runTool, text, addresses, recipients } from "./lib/graph.ts";

export function sendBody(args: Record<string, any>) {
  return {
    message: {
      subject: text(args, "subject", true, 500),
      body: { contentType: "Text", content: text(args, "body", true) },
      toRecipients: recipients(addresses(args.to, "to", true)),
      ccRecipients: recipients(addresses(args.cc, "cc")),
    },
    saveToSentItems: true,
  };
}

export async function mailSend(args: Record<string, any>) {
  const body = sendBody(args);
  await graph("POST", "/v1.0/me/sendMail", body);
  return {
    sent: true,
    to: body.message.toRecipients.map((r) => r.emailAddress.address).join(", "),
    subject: body.message.subject,
  };
}

if (import.meta.main) await runTool(mailSend);
