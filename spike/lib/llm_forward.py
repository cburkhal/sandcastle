#!/usr/bin/env python3
"""Plain HTTP forwarder for spike item 3: the stand-in for the LLM relay.

usage: LITELLM_BASE_URL=https://<litellm-host> LITELLM_KEY=... \
       llm_forward.py BIND_ADDR PORT

Accepts plain HTTP, replaces the Authorization header with the real key and
forwards to LITELLM_BASE_URL over HTTPS. Responses are buffered (no streaming),
which is enough to prove the path. The key is read from the environment only
and never printed.
"""
import os
import sys
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

bind, port = sys.argv[1], int(sys.argv[2])
upstream = os.environ["LITELLM_BASE_URL"].rstrip("/")
key = os.environ["LITELLM_KEY"]


class Handler(BaseHTTPRequestHandler):
    def forward(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        req = urllib.request.Request(upstream + self.path, data=body, method=self.command)
        req.add_header("Authorization", "Bearer " + key)
        if self.headers.get("Content-Type"):
            req.add_header("Content-Type", self.headers["Content-Type"])
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                status, data, ctype = resp.status, resp.read(), resp.headers.get("Content-Type")
        except urllib.error.HTTPError as exc:
            status, data, ctype = exc.code, exc.read(), exc.headers.get("Content-Type")
        except Exception as exc:  # upstream unreachable
            status, data, ctype = 502, type(exc).__name__.encode(), "text/plain"
        self.send_response(status)
        if ctype:
            self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    do_GET = do_POST = forward

    def log_message(self, fmt, *args):
        print("%s %s from %s" % (self.command, self.path, self.client_address[0]), flush=True)


server = ThreadingHTTPServer((bind, port), Handler)
print("listening on %s:%d" % (bind, port), flush=True)
server.serve_forever()
