---
title: Users and roles
description: Create the first admin, use the operator bootstrap key, pick org admin, publisher, or member, and change a password.
status: shipped
proves:
  - crates/vam-orchestrator/tests/login_tests.rs
  - member-chats-with-a-released-agent
  - member-changes-own-password-and-signs-in-with-the-new-one
  - password-change-signs-out-every-session-and-keeps-api-keys
  - wrong-current-password-is-refused-and-attempts-are-limited
  - api-key-cannot-change-a-password
  - new-password-equal-to-current-is-refused
  - password-change-needs-an-idempotency-key
  - crates/vam-orchestrator/tests/api_tests.rs::change_my_password_success_writes_one_identity_audit_record_without_secrets
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
- `bootstrap` is reserved to that key. No user can be named `bootstrap` (any letter case), and no API key can be minted with the name `bootstrap` or the principal `User::"bootstrap"`; the node answers 400.

`admin` in conversation means **`org_admin`**, not operator.

## Four profiles

Exactly one role per user.

| Profile | Who | When to use |
|---|---|---|
| `operator` | Bootstrap key / seeded `operator` | Host, keys, brokers. Not a tenant persona. |
| `org_admin` | First `VAMD_BOOTSTRAP_ADMIN` user, later tenant admins | Create users, reset passwords, grant publish, org secrets, reassign catalog. Org settings in the VAM app. |
| `publisher` | Member who received `publish:grant` | Write catalog and **release**. Draft + released workspaces. |
| `member` | Default, including seeded `local` | Run **released** agents. Own sessions, runs, user-scope secrets. No catalog write. |

## Who can change a password

There are three ways, for three situations.

| You want | Who may do it | How | What it needs | What happens |
|---|---|---|---|---|
| Change your own password | Anyone signed in with a password session, any role, the operator too | `vam passwd`, or `POST /v1/me/password` | Your current password | Every session of yours is signed out. Your API keys keep working. Five wrong current passwords, then a wait. |
| Reset another user's password | An org admin (`users:admin`). Not the operator's password. | `POST /v1/users/{id}/password` | No current password | Their sessions are revoked. Their API keys keep working. |
| Reset any password, the operator's too | Whoever has a shell on the node host | `vamd --reset-password` | Host access | Sets the password from the host. |

An API key cannot change a password, not even the key of the same user. Run `vam login` first, then `vam passwd`.

A successful change of your own password is recorded in the audit ledger as `vam.v1.user.password_changed`, with no password in it (`crates/vam-orchestrator/tests/api_tests.rs::change_my_password_success_writes_one_identity_audit_record_without_secrets`). An admin reset (`POST /v1/users/{id}/password`) and `vamd --reset-password` are not audited yet.

### Change your own password

```bash
vam login --username admin --password-from-env OLD_PW
vam passwd
```

`vam passwd` asks for the current password, the new one, and the new one again. Nothing you type shows on screen. It prints:

```text
Current password:
New password:
Repeat new password:
password changed; every session was signed out. Run vam login.
```

A script names the two variables instead of typing. The password is never an argument:

```bash
vam passwd --current-password-from-env OLD_PW --new-password-from-env NEW_PW
```

After a change, sign in again with the new password. The node signs out every session of that user, this one included, and the CLI drops the stored session from its config file. An API key that was stored with `vam config set-key` stays.

What the node refuses, as `vam passwd` shows it:

```text
Error: Wrong: current password is incorrect.
Expected: your current password (five wrong tries, then a wait).
Fix: vam passwd.

Error: Wrong: new password must differ from the current password.

Error: Wrong: changing a password needs a password session (vam login); API keys cannot change a password.

Error: Wrong: too many wrong password attempts; retry after 33s.
```

The limit is five wrong current passwords per user. One more try comes back every 60 seconds. It is on even when `VAMD_RATE_LIMIT=off`. A node restart clears the count (no test proves that part).

Proved by `member-changes-own-password-and-signs-in-with-the-new-one`, `password-change-signs-out-every-session-and-keeps-api-keys` (sessions end, keys stay), `wrong-current-password-is-refused-and-attempts-are-limited` (the wrong password and the limit), `api-key-cannot-change-a-password`, `new-password-equal-to-current-is-refused`, and `password-change-needs-an-idempotency-key`. The route and its errors are on [Host API](/orchestrator/api/).

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
