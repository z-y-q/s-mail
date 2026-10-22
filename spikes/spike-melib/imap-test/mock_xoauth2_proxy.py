#!/usr/bin/env python3
"""IMAP XOAUTH2→LOGIN 翻译代理（M4-b e2e 用）。

用法：python3 mock_xoauth2_proxy.py <监听端口> <上游端口> <sasl日志>

- 上游（dovecot，PLAIN/LOGIN）不支持 XOAUTH2，本代理补齐 e2e 链路：
  1. 服务器→客户端方向：CAPABILITY 行的 AUTH=PLAIN 前注入 AUTH=XOAUTH2
     （melib 只有看到 AUTH=XOAUTH2 才走 XOAUTH2 分支；SASL-IR dovecot 已广播）
  2. 客户端→服务器方向：把 "TAG AUTHENTICATE XOAUTH2 <b64>" 解码出原始
     SASL 字节写入日志（e2e 断言线上格式），再翻译成
     "TAG LOGIN <user> testpass" 发上游（static passdb 密码）
  3. 其余字节双向逐行透传（split/join 同分隔符，无损，兼容 APPEND 字面量）
"""
import base64
import re
import socket
import sys
import threading

LISTEN = int(sys.argv[1])
UPSTREAM = int(sys.argv[2])
LOG = sys.argv[3]
UPSTREAM_PASS = "testpass"


def pump(src, dst, translate_auth, rewrite_cap, tag):
    trace = open("/tmp/proxy-trace.log", "ab")
    buf = b""
    try:
        while True:
            d = src.recv(65536)
            if not d:
                break
            buf += d
            while b"\r\n" in buf:
                line, buf = buf.split(b"\r\n", 1)
                trace.write(tag + line[:160] + b"\n")
                trace.flush()
                if translate_auth[0]:
                    m = re.match(rb"(\S+) AUTHENTICATE XOAUTH2 (\S+)$", line)
                    if m:
                        tag, b64 = m.group(1), m.group(2)
                        raw = base64.b64decode(b64)
                        with open(LOG, "ab") as f:
                            f.write(raw + b"\n")
                        mu = re.search(rb"user=([^\x01]+)", raw)
                        user = mu.group(1).decode() if mu else "test"
                        out = (
                            tag
                            + b' LOGIN "'
                            + user.encode()
                            + b'" "'
                            + UPSTREAM_PASS.encode()
                            + b'"'
                        )
                        dst.sendall(out + b"\r\n")
                        translate_auth[0] = False
                        continue
                if rewrite_cap and b"CAPABILITY" in line and b"AUTH=PLAIN" in line:
                    line = line.replace(b"AUTH=PLAIN", b"AUTH=XOAUTH2 AUTH=PLAIN", 1)
                if rewrite_cap and b" COMPRESS=DEFLATE" in line:
                    # melib 登录后会启用 DEFLATE，压缩流会打爆逐行翻译代理 → 剥掉
                    line = line.replace(b" COMPRESS=DEFLATE", b"")
                dst.sendall(line + b"\r\n")
    except OSError:
        pass
    finally:
        if buf:
            try:
                dst.sendall(buf)
            except OSError:
                pass
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle(cli: socket.socket) -> None:
    srv = socket.create_connection(("127.0.0.1", UPSTREAM))
    translate = [True]
    t1 = threading.Thread(target=pump, args=(cli, srv, translate, False, b"C: "))
    t2 = threading.Thread(target=pump, args=(srv, cli, [False], True, b"S: "))
    t1.start()
    t2.start()
    t1.join()
    t2.join()
    cli.close()
    srv.close()


if __name__ == "__main__":
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", LISTEN))
    s.listen(8)
    print(f"xoauth2-proxy {LISTEN} -> {UPSTREAM}, sasl log -> {LOG}", flush=True)
    while True:
        conn, _ = s.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()
