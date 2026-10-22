#!/usr/bin/env python3
"""M5-b 规则过滤 e2e：注入 5 封邮件（2 命中 from 规则、1 命中 subject 规则、
1 命中 move 规则、1 对照组不命中任何规则）。"""
import base64
import os
import time

MAILDIR_NEW = os.path.join(os.path.dirname(__file__), "mail/test/Maildir/new")


def b64(s: str) -> str:
    return base64.b64encode(s.encode("utf-8")).decode("ascii")


def enc(name: str, addr: str) -> str:
    return f'=?UTF-8?B?{b64(name)}?= <{addr}>'


MSGS = [
    # (from, subject, date) —— 规则：promo@→已读；周报→星标；moveme@→移动 Sent
    ("promo@shop.io", "限时优惠第三波", "Sat, 18 Oct 2026 12:40:00 +0800"),
    ("promo@mall.cn", "错过再等一年", "Sat, 18 Oct 2026 12:41:00 +0800"),
    ("boss@corp.com", "Q4 周报汇总", "Sat, 18 Oct 2026 12:42:00 +0800"),
    ("moveme@x.org", "挪走我", "Sat, 18 Oct 2026 12:43:00 +0800"),
    ("friend@q.com", "普通邮件对照组", "Sat, 18 Oct 2026 12:44:00 +0800"),
]

os.makedirs(MAILDIR_NEW, exist_ok=True)
now = int(time.time())
for i, (frm, subject, date) in enumerate(MSGS):
    body = f"{subject} 的正文。\n"
    name, domain = frm.split("@")
    raw = (
        f"From: {enc(name, frm)}\r\n"
        f"To: {enc('我', 'test@localhost')}\r\n"
        f"Subject: =?UTF-8?B?{b64(subject)}?=\r\n"
        f"Date: {date}\r\n"
        f"Message-ID: <m8-{i}@{domain}>\r\n"
        "MIME-Version: 1.0\r\n"
        "Content-Type: text/plain; charset=utf-8\r\n"
        "Content-Transfer-Encoding: base64\r\n"
        "\r\n"
        f"{b64(body)}\r\n"
    )
    path = os.path.join(MAILDIR_NEW, f"{now}.M{20 + i}P1.local")
    with open(path, "w", newline="") as f:
        f.write(raw)
    print(f"seeded: {frm} / {subject} -> {path}")
