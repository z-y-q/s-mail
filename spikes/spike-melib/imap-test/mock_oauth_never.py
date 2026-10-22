#!/usr/bin/env python3
"""永不授权的 OAuth mock（验证设备流可被会话关闭中断，m12 用）：
/devicecode → interval=1s；/token 永远 authorization_pending。"""
import http.server
import json
import sys
import urllib.parse

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18889


class H(http.server.BaseHTTPRequestHandler):
    def _json(self, code, obj):
        d = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(d)))
        self.end_headers()
        self.wfile.write(d)

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        self.rfile.read(n)
        if self.path == "/devicecode":
            self._json(200, {
                "device_code": "never", "user_code": "NEVER-1",
                "verification_uri": "https://never.example/verify",
                "interval": 1, "expires_in": 3600,
            })
        else:
            self._json(400, {"error": "authorization_pending"})

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
