#!/usr/bin/env python3
"""A stand-in for the LLM gateway, for e2e/run.sh. OpenAI-compatible:
/v1/models and a streamed /v1/chat/completions. Every Authorization header it
receives is appended to the file given as the third argument, so the check can
prove that the relay swapped in the real key."""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, ANSWER, SEEN = int(sys.argv[1]), sys.argv[2], sys.argv[3]
MODEL = "e2e-model"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def record(self):
        with open(SEEN, "a") as seen:
            seen.write(f"{self.command} {self.path} {self.headers.get('Authorization', '-')}\n")

    def send(self, status, content_type, body):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.record()
        if self.path == "/v1/models":
            body = json.dumps({"object": "list", "data": [{"id": MODEL, "object": "model"}]})
            self.send(200, "application/json", body.encode())
        else:
            self.send(404, "text/plain", b"not found")

    def do_POST(self):
        self.record()
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        # sandcastle's own instructions must be part of every chat.
        if b"inside a sandcastle box" in body:
            with open(SEEN, "a") as seen:
                seen.write("INSTRUCTIONS present\n")
        if self.path != "/v1/chat/completions":
            self.send(404, "text/plain", b"not found")
            return

        def chunk(delta, finish=None):
            data = {"id": "e2e", "object": "chat.completion.chunk", "created": 0, "model": MODEL,
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            return f"data: {json.dumps(data)}\n\n"

        usage = {"id": "e2e", "object": "chat.completion.chunk", "created": 0, "model": MODEL, "choices": [],
                 "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}
        body = (chunk({"role": "assistant", "content": ANSWER}) + chunk({}, "stop")
                + f"data: {json.dumps(usage)}\n\n" + "data: [DONE]\n\n").encode()
        self.send(200, "text/event-stream", body)


server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
print("listening", flush=True)
server.serve_forever()
