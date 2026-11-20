#!/usr/bin/env python3
"""Notesdesk tool: one HTTP call to Notesdesk through the guest's egress proxy.

`host` and `path` in the arguments override the defaults (a test aims the tool elsewhere). The only
credential named is the connector's placeholder; the tool never holds the member's key.
"""
import json
import os
import socket
import sys

PLACEHOLDER = "vamsec_notesdesk_api"
DEFAULT_HOST = "api.notesdesk.example"


def main() -> int:
    raw = sys.stdin.read().strip()
    args = json.loads(raw) if raw else {}
    create = bool(args.get("title")) and not args.get("query")
    method = "POST" if create else "GET"
    path = "/v1/notes" if create else "/v1/notes?query=%s" % args.get("query", "")
    path = str(args.get("path", path))
    host = str(args.get("host") or DEFAULT_HOST)
    body = json.dumps({"note": {"title": args.get("title", "")}}) if method == "POST" else ""

    proxy = os.environ.get("HTTP_PROXY") or os.environ.get("http_proxy") or ""
    proxy = proxy.replace("http://", "").rstrip("/")
    phost, _, pport = proxy.rpartition(":")
    if not phost:
        print("notesdesk-error: no proxy in the environment")
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
        print(f"notesdesk-error: {e}")
        return 1
    text = b"".join(chunks).decode("utf-8", "replace")
    head, _, rest = text.partition("\r\n\r\n")
    print(head.split("\r\n")[0])
    print(rest)
    return 0


sys.exit(main())
