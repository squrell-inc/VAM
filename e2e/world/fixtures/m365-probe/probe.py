#!/usr/bin/env python3
"""Probe tool: one HTTP call to Microsoft Graph through the guest's egress proxy.

Reads {"method": "GET", "path": "/v1.0/me/messages"} from stdin. The only credential it names
is the placeholder; it never holds a token. Prints the status line and the body.
"""
import json
import os
import socket
import sys

PLACEHOLDER = "vamsec_m365_graph"
HOST = "graph.microsoft.com"


def main() -> int:
    raw = sys.stdin.read().strip()
    args = json.loads(raw) if raw else {}
    method = str(args.get("method", "GET")).upper()
    path = str(args.get("path", "/v1.0/me/messages"))
    body = json.dumps(args.get("body", {})) if method == "POST" else ""

    proxy = os.environ.get("HTTP_PROXY") or os.environ.get("http_proxy") or ""
    proxy = proxy.replace("http://", "").rstrip("/")
    phost, _, pport = proxy.rpartition(":")
    if not phost:
        print("probe-error: no proxy in the environment")
        return 1

    req = (
        f"{method} / HTTP/1.1\r\n"
        f"Host: {HOST}\r\n"
        f"X-Target-Url: https://{HOST}{path}\r\n"
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
        print(f"probe-error: {e}")
        return 1
    text = b"".join(chunks).decode("utf-8", "replace")
    head, _, rest = text.partition("\r\n\r\n")
    print(head.split("\r\n")[0])
    print(rest)
    return 0


sys.exit(main())
