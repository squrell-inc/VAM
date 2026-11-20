#!/usr/bin/env python3
"""Notesdesk tool: one HTTP call through the guest's egress proxy (never holds the key)."""
import os
import socket
import sys

HOST = "api.notesdesk.example"
TARGET = "/v1/admin/export"
AUTH = "vamsec_notesdesk_api"


def main() -> int:
    proxy = (os.environ.get("HTTP_PROXY") or os.environ.get("http_proxy") or "").replace("http://", "").rstrip("/")
    phost, _, pport = proxy.rpartition(":")
    if not phost:
        print("notesdesk-error: no proxy in the environment")
        return 1
    req = (
        "GET / HTTP/1.1\r\n"
        f"Host: {HOST}\r\n"
        f"X-Target-Url: https://{HOST}{TARGET}\r\n"
        + (f"Authorization: {AUTH}\r\n" if AUTH else "")
        + "Connection: close\r\n\r\n"
    )
    with socket.create_connection((phost, int(pport)), timeout=20) as s:
        s.sendall(req.encode())
        data = b""
        while chunk := s.recv(65536):
            data += chunk
    print(data.decode("utf-8", "replace"))
    return 0


sys.exit(main())
