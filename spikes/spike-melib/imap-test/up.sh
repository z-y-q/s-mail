#!/bin/sh
# 启动本地 IMAP 测试服务器：宿主 1143 → 容器 143（纯文本），账号 test/testpass
set -e
cd "$(dirname "$0")"

mkdir -p mail/test/Maildir/new mail/test/Maildir/cur mail/test/Maildir/tmp
mkdir -p mail/test/Maildir/Sent/new mail/test/Maildir/Sent/cur mail/test/Maildir/Sent/tmp
mkdir -p mail/test/Maildir/Drafts/new mail/test/Maildir/Drafts/cur mail/test/Maildir/Drafts/tmp
mkdir -p mail/test2/Maildir/new mail/test2/Maildir/cur mail/test2/Maildir/tmp
python3 seed_mail.py test
python3 seed_mail.py test2

docker rm -f smail-imap >/dev/null 2>&1 || true
docker run -d --name smail-imap -p 1143:31143 \
  -e USER_PASSWORD=testpass \
  -v "$PWD/dovecot.conf":/etc/dovecot/dovecot.conf:ro \
  -v "$PWD/mail":/var/mail \
  dovecot/dovecot:latest >/dev/null

sleep 2
# 端口就绪探测（固定 sleep 在慢机/CI 不够；曾致 e2e 首连 Timed out）
i=0
while [ $i -lt 15 ]; do
  nc -z 127.0.0.1 1143 && break
  i=$((i + 1)); sleep 1
done
echo "== smail-imap 已启动（127.0.0.1:1143, test/testpass）=="
docker logs smail-imap 2>&1 | tail -3
