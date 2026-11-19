---
id: "urn:vam:skill:connectors.community.example/community/ticketdesk-tickets"
name: "ticketdesk-tickets"
version: "1.0.0"
description: "Search and show tickets in the company's Ticketdesk; create one only with the member's approval"
---

# ticketdesk-tickets

Work with the company's Ticketdesk tickets as the member. You never see or handle a token.

- **ticket_search** searches tickets (`query`) or shows one (`id`).
- **ticket_create** creates a ticket (`subject`). It always asks the member first.

The company's host is in `.vam/connector.json` next to this file (`hosts.api`).
