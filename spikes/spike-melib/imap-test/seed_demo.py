#!/usr/bin/env python3
"""给本地 dovecot 灌一套体面的演示邮件（IMAP APPEND，唯一 Message-ID）。
用法：python3 seed_demo.py [account=test]"""
import base64
import imaplib
import random
import sys
import time

ACCT = sys.argv[1] if len(sys.argv) > 1 else "test"
PW = "testpass"

def b64(s):
    return base64.b64encode(s.encode()).decode()

def enc(name, addr):
    return f"=?UTF-8?B?{b64(name)}?= <{addr}>"

def plain(name, addr, subject, body, date, mid):
    return (
        f"From: {enc(name, addr)}\r\n"
        f"To: {enc('我', ACCT + '@localhost')}\r\n"
        f"Subject: =?UTF-8?B?{b64(subject)}?=\r\n"
        f"Date: {date}\r\nMessage-ID: <{mid}>\r\n"
        "MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n"
        "Content-Transfer-Encoding: base64\r\n\r\n" + b64(body) + "\r\n"
    )

DEMO = [
    ("张三", "zhangsan@corp.cn", "Q3 产品路线图评审邀请",
     "各位：Q3 路线图定稿评审定于本周四 14:00，会议室 A / 线上同步。\n请提前过一遍附件中的草案，会上只讨论有争议的部分。", -10),
    ("李四", "lisi@corp.cn", "Re: Q3 产品路线图评审邀请",
     "收到。数据看板那页我会带新版口径，会上讲 5 分钟。", -9),
    ("王五", "wangwu@corp.cn", "Re: Q3 产品路线图评审邀请",
     "我没有异议，周四见。", -8),
    ("财务部", "finance@corp.cn", "九月差旅报销截止提醒",
     "九月差旅报销请于本月 25 日前提交，逾期并入下月处理。\n入口：OA → 财务 → 差旅报销。", -7),
    ("GitHub", "noreply@github.com", "[s-mail] PR #42 merged: feat(m6): in-app account setup",
     "coolajoe merged 1 commit into main from feat/m6-in-app-setup.\n+814 −62", -6),
    ("亚马逊云", "no-reply@aws.com", "您的账单已出：10 月 ¥ 1,284.00",
     "感谢使用 AWS。10 月账单已可下载。\n明细：EC2 ¥812 · S3 ¥96 · 其他 ¥376。", -5),
    ("周报机器人", "bot@team.io", "团队周报 W42：交付顺利",
     "本周完成 M6 账号引导与热重载；下周 QRESYNC。\n风险：无。", -4),
    ("赵六", "zhaoliu@old.com", "转发：那年今天的照片",
     "翻旧硬盘找到的，发你一份。", -3),
    ("机场快线", "service@rail.cn", "车票取票提醒：G1024 次",
     "您已购票成功。请于发车前 30 分钟取票，凭身份证直接进站。", -2),
    ("s-mail 团队", "hi@s-mail.local", "欢迎使用 s-mail",
     "这是一封本地测试邮件。试试：\n· ↑↓ 键盘翻信\n· 搜索框输「账单」回车\n· 顶栏 🌙 切深色\n· 点左上 ✎ 写一封信发给 test2@localhost", -1),
]

ts = int(time.time())
m = imaplib.IMAP4("127.0.0.1", 1143)
m.login(ACCT, PW)
n = 0
for name, addr, subject, body, off in DEMO:
    date = time.strftime("%a, %d %b %Y %H:%M:%S +0800", time.localtime(ts + off * 86400))
    mid = f"demo-{ACCT}-{ts}-{abs(off)}@seed"
    msg = plain(name, addr, subject, body, date, mid)
    m.append("INBOX", None, None, msg.encode())
    n += 1
    print(f"appended: {subject}")

# 一封 HTML（走渲染管线）
html_body = f"""<html><body><h2 style="color:#c00">限时 48 小时</h2>
<p>尊敬的用户，您的<b>会员权益</b>即将到期：</p>
<ul><li>全库文档无限下载</li><li>专属客服通道</li></ul>
<p><a href="https://example.com/renew">立即续费</a>（测试链接，点击会开浏览器）</p>
<script>alert('这行应该被消毒掉')</script></body></html>"""
html_mail = (
    f"From: {enc('会员中心', 'vip@shop.cn')}\r\nTo: {enc('我', ACCT + '@localhost')}\r\n"
    f"Subject: =?UTF-8?B?{b64('会员到期提醒（HTML 测试）')}?=\r\n"
    f"Date: {time.strftime('%a, %d %b %Y %H:%M:%S +0800', time.localtime(ts))}\r\n"
    f"Message-ID: <demo-html-{ACCT}-{ts}@seed>\r\nMIME-Version: 1.0\r\n"
    'Content-Type: text/html; charset=utf-8\r\n\r\n' + html_body + "\r\n"
)
m.append("INBOX", None, None, html_mail.encode())
print("appended: [HTML] 会员到期提醒")
m.logout()
print(f"done：{n + 1} 封 → {ACCT}")
