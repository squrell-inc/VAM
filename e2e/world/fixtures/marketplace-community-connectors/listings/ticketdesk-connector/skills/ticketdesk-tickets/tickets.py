#!/usr/bin/env python3
"""Ticketdesk tool: one HTTP call to the company's Ticketdesk through the guest's egress proxy.

The company host comes from `.vam/connector.json` (written by the node next to the manifest);
`host` and `path` in the arguments override it (a test aims the tool elsewhere). The only credential
named is the connector's placeholder; the tool never holds a token.
"""
import json
import os
import socket
import sys

PLACEHOLDER = "vamsec_ticketdesk_api"
HERE = os.path.dirname(os.path.abspath(__file__))


def company_host():
    try:
        with open(os.path.join(HERE, ".vam", "connector.json")) as f:
            return json.load(f)["hosts"]["api"]
    except (OSError, KeyError, ValueError):
        return ""


def main() -> int:
    raw = sys.stdin.read().strip()
    args = json.loads(raw) if raw else {}
    create = bool(args.get("subject")) and not args.get("query") and not args.get("id")
    method = "POST" if create else "GET"
    if args.get("id"):
        path = "/api/v2/tickets/%s.json" % args["id"]
    elif create:
        path = "/api/v2/tickets.json"
    else:
        path = "/api/v2/search.json?query=%s" % args.get("query", "")
    path = str(args.get("path", path))
    host = str(args.get("host") or company_host())
    if not host:
        print("ticketdesk-error: no company host (.vam/connector.json)")
        return 1
    body = json.dumps({"ticket": {"subject": args.get("subject", "")}}) if method == "POST" else ""

    proxy = os.environ.get("HTTP_PROXY") or os.environ.get("http_proxy") or ""
    proxy = proxy.replace("http://", "").rstrip("/")
    phost, _, pport = proxy.rpartition(":")
    if not phost:
        print("ticketdesk-error: no proxy in the environment")
        return 1
    req = (
        f"{method} / HTTP/1.1\r\n"
        f"Host: {host}\r\n"
        f"X-Target-Url: https://{host}{path}\r\n"
        f"Authorization: {PLACEHOLDER}\r\n"
        "Content-Type: application/json\r\n"
        f"Content-Length: {len(body)}\r\n"
        "Connection: close\r\n\r\n"
        f"{body}"
    )
    try:
        with socket.create_connection((phost, int(pport)), timeout=20) as s:
            s.settimeout(20)
            s.sendall(req.encode())
            chunks = []
            while True:
                try:
                    c = s.recv(65536)
                except socket.timeout:
                    break
                if not c:
                    break
                chunks.append(c)
    except OSError as e:
        print(f"ticketdesk-error: {e}")
        return 1
    text = b"".join(chunks).decode("utf-8", "replace")
    head, _, rest = text.partition("\r\n\r\n")
    print(head.split("\r\n")[0])
    print(rest)
    return 0


sys.exit(main())
