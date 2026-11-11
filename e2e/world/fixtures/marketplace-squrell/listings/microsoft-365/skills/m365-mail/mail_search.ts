// mail_search: GET /v1.0/me/messages (a read; the card lists the messages).
import { graph, runTool, text, whole, sender } from "./lib/graph.ts";

export const SELECT = "id,subject,from,receivedDateTime,bodyPreview,isRead";

export function searchPath(args: Record<string, any>, now: Date = new Date()): string {
  const query = text(args, "query", false, 500).replace(/"/g, " ").trim();
  const top = whole(args, "top", 10, 1, 50);
  const since = whole(args, "sinceHours", 24, 1, 24 * 31);
  const q = [`$select=${SELECT}`, `$top=${top}`];
  if (query) {
    // Graph cannot combine $search with $filter or $orderby; results come back relevance-ranked.
    q.push(`$search=${encodeURIComponent(`"${query}"`)}`);
  } else {
    const from = new Date(now.getTime() - since * 3600_000).toISOString();
    q.push(`$filter=${encodeURIComponent(`receivedDateTime ge ${from}`)}`);
    q.push(`$orderby=${encodeURIComponent("receivedDateTime desc")}`);
  }
  return `/v1.0/me/messages?${q.join("&")}`;
}

export function shape(json: any) {
  return {
    messages: (json?.value ?? []).map((m: any) => ({
      id: m.id,
      from: sender(m.from),
      subject: m.subject ?? "",
      receivedAt: m.receivedDateTime ?? "",
      preview: (m.bodyPreview ?? "").slice(0, 280),
      unread: m.isRead === false,
    })),
  };
}

export async function mailSearch(args: Record<string, any>) {
  const { json } = await graph("GET", searchPath(args));
  return shape(json);
}

if (import.meta.main) await runTool(mailSearch);
