// calendar_list: GET /v1.0/me/calendarView (a read).
import { graph, runTool, whole, window } from "./lib/graph.ts";

export function viewPath(args: Record<string, any>, now: Date = new Date()): string {
  const { start, end } = window(args, now);
  const top = whole(args, "top", 25, 1, 100);
  const q = [
    `startDateTime=${encodeURIComponent(start)}`,
    `endDateTime=${encodeURIComponent(end)}`,
    `$select=${encodeURIComponent("id,subject,start,end,location,isAllDay,showAs,organizer")}`,
    `$orderby=${encodeURIComponent("start/dateTime")}`,
    `$top=${top}`,
  ];
  return `/v1.0/me/calendarView?${q.join("&")}`;
}

export function shape(json: any) {
  return {
    events: (json?.value ?? []).map((e: any) => ({
      id: e.id,
      subject: e.subject ?? "",
      start: e.start?.dateTime ?? "",
      end: e.end?.dateTime ?? "",
      timeZone: e.start?.timeZone ?? "UTC",
      location: e.location?.displayName ?? "",
      allDay: e.isAllDay === true,
      showAs: e.showAs ?? "",
      organizer: e.organizer?.emailAddress?.address ?? "",
    })),
  };
}

export async function calendarList(args: Record<string, any>) {
  const { json } = await graph("GET", viewPath(args), undefined, { Prefer: 'outlook.timezone="UTC"' });
  return shape(json);
}

if (import.meta.main) await runTool(calendarList);
