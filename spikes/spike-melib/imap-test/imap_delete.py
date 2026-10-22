#!/usr/bin/env python3
"""用另一个 IMAP 客户端删信（m13 "他处删除" 场景模拟）。
用法：python3 imap_delete.py <Message-ID> [host] [port] [user] [pass]
"""
import imaplib
import sys

mid = sys.argv[1]
host = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
port = int(sys.argv[3]) if len(sys.argv) > 3 else 1143
user = sys.argv[4] if len(sys.argv) > 4 else "test"
pw = sys.argv[5] if len(sys.argv) > 5 else "testpass"

m = imaplib.IMAP4(host, port)
m.login(user, pw)
m.select("INBOX")
_, d = m.search(None, f'HEADER Message-ID "{mid}"')
nums = d[0].split()
assert nums, f"server 上找不到 {mid}"
for n in nums:
    m.store(n.decode(), "+FLAGS.SILENT", "(\\Deleted)")
m.expunge()
print("deleted", nums)
