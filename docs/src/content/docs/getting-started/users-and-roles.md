---
title: Users and roles
description: Create the first admin, use the operator bootstrap key, and pick org admin, publisher, or member.
---

Isolation is always on. Every durable row has an owner. This page is how to create people on a fresh node and which login to use next.

Architecture stays on [User Isolation](/orchestrator/user-isolation/). Role and scope tables stay on [Control plane security](/orchestrator/user-isolation/). Env names stay on [Daemon](/orchestrator/overview/).

## Create the first loginable admin

There is no invite email and no UI for the first user. Set a username and password **before** `vamd` starts:

```bash
export VAMD_BOOTSTRAP_ADMIN=admin
export VAMD_BOOTSTRAP_ADMIN_PASSWORD='…'
# daemon start, or:
vamd --bootstrap-admin admin   # still needs VAMD_BOOTSTRAP_ADMIN_PASSWORD
```

That user is **`org_admin`** unless the username is the seeded `operator`. The same username again is idempotent. A second distinct bootstrap admin is refused.

Then sign in:

```bash
vam login --username admin
```

The VAM app password sign-in uses the same user.

On [macOS + Lima](/getting-started/macos-lima/), export the two vars **before** `./scripts/start-vamd-lima.sh`. The script forwards them into the VM.

## Operator is not that admin

- Seeded user `operator`. There is one.
- Signs in with the **bootstrap API key** (`VAMD_BOOTSTRAP_API_KEY`, or the admin key `vam node init` shows once on the first run; `vamd` never logs it). That is not a password.
- Node owner: `keys:admin`, `brokers:admin`, plus user admin. Use it for the daemon, broker install, and CI.
- Do not use it as a tenant login in the VAM app day to day.

`admin` in conversation means **`org_admin`**, not operator.

## Four profiles

Exactly one role per user.

| Profile | Who | When to use |
|---|---|---|
| `operator` | Bootstrap key / seeded `operator` | Host, keys, brokers. Not a tenant persona. |
| `org_admin` | First `VAMD_BOOTSTRAP_ADMIN` user, later tenant admins | Create users, reset passwords, grant publish, org secrets, reassign catalog. Org settings in the VAM app. |
| `publisher` | Member who received `publish:grant` | Write catalog and **release**. Draft + released workspaces. |
| `member` | Default, including seeded `local` | Run **released** agents. Own sessions, runs, user-scope secrets. No catalog write. |

## Which page next

| You want | Open |
|---|---|
| Mac + Lima copy-paste | [macOS + Lima](/getting-started/macos-lima/) |
| Five-minute daemon start | [Quickstart](/getting-started/quickstart/) |
| Install CLI and `--bootstrap-admin` | [Installation](/getting-started/installation/) |
| `vam login` / keys | [CLI reference](/getting-started/cli/) |
| Full `VAMD_*` table | [Daemon](/orchestrator/overview/) |
| Role and scope tables | [Control plane security](/orchestrator/user-isolation/) |
| Isolation invariants | [User Isolation](/orchestrator/user-isolation/) |
