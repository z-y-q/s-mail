#!/usr/bin/env python3
"""生成中文测试邮件（RFC2047 编码头），写入 Maildir/new。"""
import base64
import os
import sys
import time

ACCOUNT = sys.argv[1] if len(sys.argv) > 1 else "test"
MAILDIR_NEW = os.path.join(os.path.dirname(__file__), f"mail/{ACCOUNT}/Maildir/new")


def b64(s: str) -> str:
    return base64.b64encode(s.encode("utf-8")).decode("ascii")


def enc(name: str, addr: str) -> str:
    return f'=?UTF-8?B?{b64(name)}?= <{addr}>'


if ACCOUNT == "test2":
    MESSAGES = [
        ("赵六", "zhaoliu@example.com", "二账号周报",
         "u1@local", "Sat, 18 Oct 2026 11:00:00 +0800"),
        ("钱七", "qianqi@example.com", "二账号专属通知",
         "u2@local", "Sat, 18 Oct 2026 11:30:00 +0800"),
    ]
else:
    MESSAGES = [
        ("张三", "zhangsan@example.com", "五月账单与发票报销",
         "t1@local", "Sat, 18 Oct 2026 09:00:00 +0800"),
        ("李四", "lisi@example.com", "面试通知：第二轮技术面安排",
         "t2@local", "Sat, 18 Oct 2026 09:30:00 +0800"),
        ("王五", "wangwu@example.com", "Re: 项目周报 indexer 进度",
         "t3@local", "Sat, 18 Oct 2026 10:15:00 +0800"),
    ]

os.makedirs(MAILDIR_NEW, exist_ok=True)

for i, (name, addr, subject, mid, date) in enumerate(MESSAGES):
    body = f"{name}你好，\n\n这封邮件用于验证中文信封解码与检索。\n\n—— {name}\n"
    raw = (
        f"From: {enc(name, addr)}\r\n"
        f"To: {enc('我', 'test@localhost')}\r\n"
        f"Subject: =?UTF-8?B?{b64(subject)}?=\r\n"
        f"Date: {date}\r\n"
        f"Message-ID: <{mid}>\r\n"
        f"MIME-Version: 1.0\r\n"
        f"Content-Type: text/plain; charset=utf-8\r\n"
        f"Content-Transfer-Encoding: base64\r\n"
        f"\r\n"
        f"{base64.b64encode(body.encode('utf-8')).decode('ascii')}\r\n"
    )
    path = os.path.join(MAILDIR_NEW, f"{int(time.time())}.M{i}P1.local")
    with open(path, "w", newline="") as f:
        f.write(raw)
    print(f"seeded: {subject} -> {path}")
