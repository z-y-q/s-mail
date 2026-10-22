#!/usr/bin/env python3
"""单连接迷你 SMTP 服务器（M2 发信 e2e 测试用）。
用法：python3 mini_smtp.py [端口=11525]
收到 DATA 后存 /tmp/mini-smtp-mail.eml 并退出。
支持 AUTH LOGIN（不校验凭据）。"""
import socket
import sys
import base64


def main(port: int = 11525) -> None:
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    s.listen(8)
    print(f"mini-smtp listening on {port}", flush=True)
    n = 0
    while True:
        conn, _ = s.accept()
        n += 1
        handle(conn, n)


def handle(conn: socket.socket, n: int) -> None:
    f = conn.makefile("rb")

    def send(x: str) -> None:
        conn.sendall(x.encode() + b"\r\n")

    send("220 mini ESMTP s-mail-test")
    mail: list[bytes] = []
    rcpts: list[str] = []
    auth_stage = 0
    while True:
        line = f.readline()
        if not line:
            break
        l = line.decode(errors="replace").rstrip("\r\n")
        print(f"C: {l[:90]}", flush=True)
        u = l.upper()
        if auth_stage == 1:
            send("334 " + base64.b64encode(b"Password:").decode())
            auth_stage = 2
        elif auth_stage == 2:
            send("235 ok")
            auth_stage = 0
        elif u.startswith("EHLO") or u.startswith("HELO"):
            send("250-mini greets you")
            send("250 AUTH LOGIN XOAUTH2")
        elif u.startswith("AUTH LOGIN"):
            send("334 " + base64.b64encode(b"Username:").decode())
            auth_stage = 1
        elif u.startswith("AUTH XOAUTH2"):
            # M4-b：XOAUTH2 初始响应即全部凭据，解码留痕供 e2e 断言
            parts = l.split(None, 2)
            raw = base64.b64decode(parts[2]) if len(parts) > 2 else b"(empty)"
            with open("/tmp/mini-smtp-auth.log", "ab") as fp:
                fp.write(raw + b"\n")
            send("235 ok")
        elif u.startswith("MAIL") or u.startswith("RCPT"):
            if u.startswith("RCPT"):
                rcpts.append(l.split(":", 1)[1].strip().strip("<>"))
            send("250 ok")
        elif u.startswith("DATA"):
            send("354 end with .")
            while True:
                dl = f.readline()
                if dl in (b".\r\n", b".\n") or dl == b".":
                    break
                mail.append(dl)
            print(f"DATA: {len(mail)} 行", flush=True)
            send("250 queued")
        elif u.startswith("QUIT"):
            send("221 bye")
            break
        else:
            send("250 ok")
    conn.close()
    data = b"".join(mail)
    # 每连接一个文件 + 兼容旧路径（M2 e2e 用固定名）
    with open(f"/tmp/mini-smtp-mail-{n}.eml", "wb") as fp:
        fp.write(data)
    with open("/tmp/mini-smtp-mail.eml", "wb") as fp:
        fp.write(data)
    # 狗粮演示：按收件人投递进本地 dovecot maildir（RCPT TO 的 local part）
    import os
    import time as _t
    base = os.path.join(os.path.dirname(__file__), "mail")
    for rcpt in rcpts:
        user = rcpt.split("@")[0].lower()
        dest = os.path.join(base, user, "Maildir", "new")
        if os.path.isdir(dest):
            path = os.path.join(dest, f"{int(_t.time())}.S{n}P1.local")
            with open(path, "wb") as fp:
                fp.write(data)
            print(f"delivered -> {user} ({path})", flush=True)
    print(f"saved /tmp/mini-smtp-mail-{n}.eml ({len(data)} B)", flush=True)


if __name__ == "__main__":
    main(int(sys.argv[1]) if len(sys.argv) > 1 else 11525)
