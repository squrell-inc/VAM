---
id: "urn:vam:skill:local.vam.dev/vam/jira-integration"
name: "Jira Integration Skill"
version: "1.0.0"
description: "Provides capability to search, create, transition, and comment on Jira issues via host-brokered secrets."
---

# Jira Integration Skill Instructions

When using this skill to manage team sprint boards, you must follow these operational rules:

1. **Query Before Action**: Always check the issue status via search before initiating a transition.
2. **Comment Format**: Comments added to tickets must contain a clear prefix identifying the automated system:
   - Example: `[VAM Agent Notification] Sprint Backlog Cleanup: ...`
3. **No Duplicate Issues**: Before creating any new task or bug, search Jira using a JQL summary filter to confirm a similar issue does not already exist.
4. **Error Handling**: If a transition or search fails due to permissions, log the failure and request manual approval.

Extra reference files under this skill directory are on the read-only mount — use `read_file` / `grep` after loading this skill; they are not auto-injected.
