#!/usr/bin/env python3
"""{{name}} tool: one read through the guest's egress proxy.

The tool holds no credential. It names the connector's placeholder and the node puts the member's
credential in its place, for the host and path this skill declares (`manifest.json`, `egress`).
"""
import http.client
import json
import os
import sys
from urllib.parse import quote, urlparse

PLACEHOLDER = "vamsec_{{id}}_api"
HOST = "{{host}}"


def host():
    # A company address connector: the node writes the member's host beside this file.
    if "{subdomain}" not in HOST:
        return HOST
    here = os.path.dirname(os.path.abspath(__file__))
    try:
        with open(os.path.join(here, ".vam", "connector.json")) as f:
            return json.load(f)["hosts"]["api"]
    except (OSError, KeyError, ValueError):
        return ""


def endpoint():
    raw = os.environ.get("VAM_EGRESS_ENDPOINT") or os.environ.get("HTTP_PROXY") or os.environ.get("http_proxy") or ""
    url = urlparse(raw if "://" in raw else "http://" + raw)
    return url.hostname, url.port


def main() -> int:
    text = sys.stdin.read().strip()
    args = json.loads(text) if text else {}
    target_host = host()
    if not target_host:
        print("{{id}}-error: no host to call", file=sys.stderr)
        return 1
    proxy_host, proxy_port = endpoint()
    if not proxy_host or not proxy_port:
        print("{{id}}-error: no egress proxy in the environment", file=sys.stderr)
        return 1
    path = "/v1/items"
    if args.get("query"):
        path += "?query=" + quote(str(args["query"]))
    conn = http.client.HTTPConnection(proxy_host, proxy_port, timeout=20)
    try:
        conn.request(
            "GET",
            "/",
            headers={
                "Host": target_host,
                "X-Target-Url": "https://" + target_host + path,
                "Authorization": PLACEHOLDER,
                "Accept": "application/json",
            },
        )
        reply = conn.getresponse()
        body = reply.read().decode("utf-8", "replace")
    except OSError as e:
        print("{{id}}-error: %s" % e, file=sys.stderr)
        return 1
    finally:
        conn.close()
    if reply.status < 200 or reply.status >= 300:
        print("{{id}}-error: answered %d" % reply.status, file=sys.stderr)
        return 1
    print(body)
    return 0


sys.exit(main())
