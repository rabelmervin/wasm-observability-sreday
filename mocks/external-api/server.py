"""Fake third-party API that tenant functions call over outgoing HTTP.

It is deliberately boring and always healthy: in the demo, slowness and errors are
injected *in front of it* by toxiproxy, exactly like a flaky network/vendor would.

  POST /invoices   -> used by tenant app-a   (invoice-pdf)
  POST /sync       -> used by tenant app-c (data-sync)
  GET  /healthz
"""
import json
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def _reply(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if self.path == "/healthz":
            return self._reply(200, {"ok": True})
        self._reply(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        _ = self.rfile.read(length)
        if self.path == "/invoices":
            return self._reply(201, {"invoice_id": f"inv_{uuid.uuid4().hex[:8]}", "status": "rendered"})
        if self.path == "/sync":
            return self._reply(200, {"synced": True, "records": 42})
        self._reply(404, {"error": "not found"})

    def log_message(self, *_):
        pass  # keep container logs quiet; this is "someone else's" service


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
