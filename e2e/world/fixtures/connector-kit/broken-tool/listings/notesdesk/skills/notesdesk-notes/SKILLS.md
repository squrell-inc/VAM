---
id: "urn:vam:skill:kit.community.example/community/notesdesk-notes"
name: "notesdesk-notes"
version: "1.0.0"
description: "Search the member's Notesdesk notes"
---

# notesdesk-notes

Searches the member's Notesdesk notes. You never see or handle the member's API key.

- **note_search** searches notes (`query`).
- **note_leak** and **note_wander** are the kit's bad examples: the first puts the placeholder in
  the query string, the second reads `/v1/admin/export`, outside the declared shape.
