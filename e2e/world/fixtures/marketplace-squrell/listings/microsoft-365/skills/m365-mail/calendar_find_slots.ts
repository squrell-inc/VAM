// calendar_find_slots: POST /v1.0/me/findMeetingTimes (READ_ONLY: it only computes suggestions).
import { graph, runTool, whole, addresses, window } from "./lib/graph.ts";

export function requestBody(args: Record<string, any>, now: Date = new Date()) {
  const minutes = whole(args, "durationMinutes", 30, 5, 480);
  const { start, end } = window(args, now);
  const attendees = addresses(args.attendees, "attendees");
  return {
    attendees: attendees.map((address) => ({ type: "required", emailAddress: { address } })),
    timeConstraint: {
      activityDomain: "work",
      timeSlots: [{ start: { dateTime: start, timeZone: "UTC" }, end: { dateTime: end, timeZone: "UTC" } }],
    },
    meetingDuration: `PT${minutes}M`,
    maxCandidates: whole(args, "maxCandidates", 5, 1, 20),
    returnSuggestionReasons: false,
  };
}

export function shape(json: any) {
  return {
    slots: (json?.meetingTimeSuggestions ?? [])
      .map((s: any) => s?.meetingTimeSlot)
      .filter(Boolean)
      .map((t: any) => ({ start: t.start?.dateTime ?? "", end: t.end?.dateTime ?? "", timeZone: t.start?.timeZone ?? "UTC" })),
    reason: json?.emptySuggestionsReason ?? "",
  };
}

export async function findSlots(args: Record<string, any>) {
  const { json } = await graph("POST", "/v1.0/me/findMeetingTimes", requestBody(args), {
    Prefer: 'outlook.timezone="UTC"',
  });
  return shape(json);
}

if (import.meta.main) await runTool(findSlots);
