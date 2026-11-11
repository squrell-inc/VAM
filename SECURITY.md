# Security policy

## Reporting a vulnerability

**Do not report security issues in a public issue, discussion or pull request.**

Report them privately through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**. Include:

- what is affected (crate, binary, API route, guest or host);
- how to reproduce it, ideally a minimal scenario or test;
- the impact you expect.

We aim to acknowledge a report within three working days and to agree a
disclosure date with you. We will credit you in the advisory unless you ask us
not to.

## Supported versions

VAM is pre-release (0.1.0-dev). Only the latest commit on `main` receives
security fixes.

## Scope

In scope: everything in this repository, notably

- the guest boundary: anything that lets code in a guest microVM (or a
  process-mode guest) read host files, reach host sockets, obtain credentials,
  or act outside its session;
- `vamd`'s HTTP API: authentication, API keys, users and ownership between
  them;
- permission and egress enforcement: a tool call or connection the agent's
  rules should have refused;
- the secrets vault and injection proxy: a secret reaching the guest, a log or
  an event;
- run receipts: a receipt that verifies but misstates what happened;
- the first-party brokers in `brokers/`.

Out of scope:

- vulnerabilities in Firecracker, the Linux kernel, KVM or third-party
  dependencies themselves (report those upstream; tell us if VAM's use of them
  makes things worse);
- deployments that turn off documented defaults, for example `--insecure-dev`
  (plain HTTP) or an agent granted open internet access;
- hosted services, apps or marketplaces that are not in this repository;
- denial of service that needs an authenticated admin.
