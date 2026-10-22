#!/usr/bin/env python3
"""本地 mock OAuth2 提供方（M4-b e2e 用）。

用法：python3 mock_oauth.py [端口=18888] [状态文件=/tmp/mock-oauth.json]

- POST /devicecode → 设备码响应（interval=1s，方便测试快速轮询）
- POST /token grant_type=device_code → 前 2 次 400 authorization_pending，
  之后发 access_token="testpass"（与 dovecot static passdb 密码一致，
  供 mock_xoauth2_proxy 翻译成 LOGIN 用）+ refresh_token
- POST /token grant_type=refresh_token → 校验后发新 access_token="mock-access-2"
- 所有请求以 JSON lines 追加到状态文件，供 e2e 断言
"""
import http.server
import json
import sys
import urllib.parse

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18888
STATE = sys.argv[2] if len(sys.argv) > 2 else "/tmp/mock-oauth.json"
PENDING_BEFORE_GRANT = 2
polls = 0


def log(evt: dict) -> None:
    with open(STATE, "a") as f:
        f.write(json.dumps(evt, ensure_ascii=False) + "\n")


class H(http.server.BaseHTTPRequestHandler):
    def _json(self, code: int, obj: dict) -> None:
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self) -> None:
        global polls
        n = int(self.headers.get("Content-Length", 0))
        form = dict(urllib.parse.parse_qsl(self.rfile.read(n).decode()))
        log({"path": self.path, "form": form})

        if self.path == "/devicecode":
            self._json(
                200,
                {
                    "device_code": "mock-device-code",
                    "user_code": "MOK-1234",
                    "verification_uri": "https://mock.example/verify",
                    "interval": 1,
                    "expires_in": 600,
                },
            )
        elif self.path == "/token":
            gt = form.get("grant_type", "")
            if gt == "urn:ietf:params:oauth:grant-type:device_code":
                polls += 1
                if polls <= PENDING_BEFORE_GRANT:
                    self._json(400, {"error": "authorization_pending"})
                else:
                    self._json(
                        200,
                        {
                            "token_type": "Bearer",
                            "access_token": "testpass",
                            "refresh_token": "mock-refresh-1",
                            "expires_in": 3600,
                            "scope": "mail",
                        },
                    )
            elif gt == "refresh_token":
                if form.get("refresh_token") != "mock-refresh-1":
                    self._json(400, {"error": "invalid_grant"})
                else:
                    self._json(
                        200,
                        {
                            "token_type": "Bearer",
                            "access_token": "mock-access-2",
                            "refresh_token": "mock-refresh-1",
                            "expires_in": 3600,
                        },
                    )
            else:
                self._json(400, {"error": "unsupported_grant_type"})
        else:
            self._json(404, {"error": "not_found"})

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    print(f"mock-oauth listening on {PORT}, state -> {STATE}", flush=True)
    http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
