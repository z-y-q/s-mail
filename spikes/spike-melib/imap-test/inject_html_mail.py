#!/usr/bin/env python3
"""向本地 dovecot 注入一封 multipart/alternative（text/plain + text/html）测试邮件。
容器内写入触发 inotify → IDLE 推送。"""
import subprocess
import sys
import base64

HTML_BODY = """<html><body>
<h2>订单通知</h2>
<p>您的订单 <b>SO-M4</b> 已发货。</p>
<ul><li>顺丰速运</li><li>单号 SF-M4-0001</li></ul>
<p><a href="https://example.com/m4">物流跟踪</a></p>
<script>alert('xss')</script>
<img src="https://tracker.example.com/pixel.gif">
</body></html>"""

PLAIN = "您的订单 SO-M4 已发货（纯文本降级版）。"

html_b64 = base64.b64encode(HTML_BODY.encode()).decode()
plain_b64 = base64.b64encode(PLAIN.encode()).decode()

EML = f"""From: =?UTF-8?B?{base64.b64encode('商城系统'.encode()).decode()}?= <shop@example.com>
To: test@local
Subject: =?UTF-8?B?{base64.b64encode('M4-HTML渲染测试'.encode()).decode()}?=
Date: Sat, 18 Oct 2026 18:00:00 +0800
Message-ID: <m4-html@local>
MIME-Version: 1.0
Content-Type: multipart/alternative; boundary="M4BOUND"

--M4BOUND
Content-Type: text/plain; charset=utf-8
Content-Transfer-Encoding: base64

{plain_b64}

--M4BOUND
Content-Type: text/html; charset=utf-8
Content-Transfer-Encoding: base64

{html_b64}

--M4BOUND--
"""

cmd = [
    "docker", "exec", "smail-imap", "sh", "-c",
    f"printf '{EML}' > /var/mail/test/Maildir/new/m4html.local",
]
subprocess.run(cmd, check=True)
print("已注入 HTML 测试邮件（m4html.local）")
