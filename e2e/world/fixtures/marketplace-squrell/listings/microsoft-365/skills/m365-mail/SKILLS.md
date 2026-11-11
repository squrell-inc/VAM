---
id: "urn:vam:skill:hub.squrell.cloud/squrell/m365-mail"
name: "m365-mail"
version: "1.0.0"
description: "Read the member's Microsoft 365 mail and calendar; draft, send and book only with their approval"
---

# m365-mail

Work with the member's own Outlook mailbox and calendar through their Microsoft 365 connection.
Every call is made as the member with their access token; you never see or handle a token.

## When to use each tool

- **mail_search** finds mail. Give `query` to search (sender, subject and body words), or leave it
  out to list the newest mail from the last `sinceHours` (default 24). It returns an id, sender,
  subject, time and a short preview for each message. The result shows as a mail card.
- **mail_read** reads one whole message by the `id` mail_search returned. Read before you reply.
- **calendar_list** shows events between `start` and `end` (default: the next seven days).
- **calendar_find_slots** suggests free times for the member and optional `attendees` (email
  addresses). It only suggests; it books nothing. The result shows as a slot card.
- **mail_draft** saves a draft in the member's Drafts folder. Nobody receives it. Use it to
  show the wording you propose.
- **mail_send** sends a mail. **calendar_create_event** books an event and invites attendees.
  Both ask the member first and show the message or event on an approval card. Never send or book
  without that card: do not try to get around it, and do not repeat a send or booking the member
  denied.

## Rules

- Never send or book on your own initiative. Draft first when the wording matters, then ask.
- One mailbox: the member's. You cannot read anyone else's mail or calendar with this skill.
- Mail and invitations leave the member's account. Check `to`, `subject` and the time before
  you call mail_send or calendar_create_event.
- Microsoft Graph limits one app to a few concurrent requests per mailbox. Make calls one at a
  time, not in parallel. If a call fails with 429, wait and try once more.
- Dates are like `2026-11-10T10:00:00`; the time zone is UTC unless you pass `timeZone`.
- A result you were not shown (an error) is the truth: report it, do not guess what the mailbox holds.
