#!/usr/bin/env python3
"""M5 会话聚合 e2e：注入 3 封回信链（A ← B ← C）到 test 账号。

host 侧直接写 Maildir/new 即可：dovecot 在 SELECT 时现读目录，
只有 IDLE 推送新信才依赖容器内写（inotify 不穿 virtiofs）。
"""
import base64
import os
import time

MAILDIR_NEW = os.path.join(
    os.path.dirname(__file__), "mail/test/Maildir/new"
)


def b64(s: str) -> str:
    return base64.b64encode(s.encode("utf-8")).decode("ascii")


def enc(name: str, addr: str) -> str:
    return f'=?UTF-8?B?{b64(name)}?= <{addr}>'


SUBJECT = "会话测试：服务器迁移方案"

MSGS = [
    # (mid, in_reply_to, references, date, from)
    ("m7-a@seed", "", "", "Sat, 18 Oct 2026 12:00:00 +0800", "架构师"),
    ("m7-b@seed", "m7-a@seed", "m7-a@seed", "Sat, 18 Oct 2026 12:10:00 +0800", "运维"),
    ("m7-c@seed", "m7-b@seed", "m7-a@seed m7-b@seed", "Sat, 18 Oct 2026 12:20:00 +0800", "老板"),
]

os.makedirs(MAILDIR_NEW, exist_ok=True)
now = int(time.time())
for i, (mid, irt, refs, date, name) in enumerate(MSGS):
    body = f"{SUBJECT} 第{i}封（{name}）\n"
    lines = [
        f"From: {enc(name, f'{name}@example.com')}",
        f"To: {enc('我', 'test@localhost')}",
        f"Subject: =?UTF-8?B?{b64(('Re: ' if i else '') + SUBJECT)}?=",
        f"Date: {date}",
        f"Message-ID: <{mid}>",
    ]
    if irt:
        lines.append(f"In-Reply-To: <{irt}>")
    if refs:
        lines.append("References: " + " ".join(f"<{x}>" for x in refs.split()))
    raw = "\r\n".join(lines) + "\r\n" + (
        "MIME-Version: 1.0\r\n"
        "Content-Type: text/plain; charset=utf-8\r\n"
        "Content-Transfer-Encoding: base64\r\n"
        "\r\n"
        f"{b64(body)}\r\n"
    )
    path = os.path.join(MAILDIR_NEW, f"{now}.M{10 + i}P1.local")
    with open(path, "w", newline="") as f:
        f.write(raw)
    print(f"seeded: {mid} -> {path}")
