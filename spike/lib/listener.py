#!/usr/bin/env python3
"""Tiny host listener for the spike: answers every GET with a fixed token.

usage: listener.py BIND_ADDR PORT TOKEN
Logs each client address to stdout, so the notes can record which source
address the host sees for traffic from a box.
"""
import socket
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

bind, port, token = sys.argv[1], int(sys.argv[2]), sys.argv[3]


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = (token + "\n").encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print("request from %s" % self.client_address[0], flush=True)


class Server(HTTPServer):
    # "::" binds IPv6 (and, on macOS, IPv4-mapped) so item 9 can probe both.
    address_family = socket.AF_INET6 if ":" in bind else socket.AF_INET


try:
    server = Server((bind, port), Handler)
except OSError as exc:
    print("bind %s:%d failed: %s" % (bind, port, exc), flush=True)
    sys.exit(1)
print("listening on %s:%d" % (bind, port), flush=True)
server.serve_forever()
