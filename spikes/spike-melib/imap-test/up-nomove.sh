#!/bin/sh
# 无 MOVE 能力的 IMAP 服务器仿真（QQ 类）：
# 1) 起常规容器 → 抓真实 CAPABILITY 行 → 去掉 MOVE 令牌
# 2) 写 dovecot-nomove.conf（imap_capability 整行覆盖）→ 原邮件目录重启
# 3) 自检：CAPABILITY 必须已无 MOVE，否则退出（配置写法失效要立刻知道）
# 邮件目录不重播种——m21 断言依赖确定性数量
set -e
cd "$(dirname "$0")"

sh down.sh >/dev/null 2>&1 || true
sh up.sh >/dev/null 2>&1

cap=$(python3 - <<'EOF'
import imaplib
m = imaplib.IMAP4('127.0.0.1', 1143)
m.login('test', 'testpass')
typ, data = m.capability()
caps = data[0]
caps = caps.decode() if isinstance(caps, bytes) else caps
print(caps.strip())
m.logout()
EOF
)
nomove=$(printf '%s' "$cap" | tr ' ' '\n' | grep -iv '^MOVE$' | tr '\n' ' ' | sed 's/ $//')

cp dovecot.conf dovecot-nomove.conf
printf '\n# m21：imap_capability 整行覆盖 = 去掉 MOVE（由 up-nomove.sh 从真实 CAPABILITY 派生）\nimap_capability = "%s"\n' "$nomove" >> dovecot-nomove.conf

docker rm -f smail-imap >/dev/null
docker run -d --name smail-imap -p 1143:31143 \
  -e USER_PASSWORD=testpass \
  -v "$PWD/dovecot-nomove.conf":/etc/dovecot/dovecot.conf:ro \
  -v "$PWD/mail":/var/mail \
  dovecot/dovecot:latest >/dev/null

i=0
while [ $i -lt 15 ]; do
  nc -z 127.0.0.1 1143 && break
  i=$((i + 1)); sleep 1
done

# 自检：服务器必须已不报 MOVE
cap2=$(python3 - <<'EOF'
import imaplib
m = imaplib.IMAP4('127.0.0.1', 1143)
m.login('test', 'testpass')
typ, data = m.capability()
caps = data[0]
caps = caps.decode() if isinstance(caps, bytes) else caps
print(caps.strip())
m.logout()
EOF
)
if printf '%s' "$cap2" | tr ' ' '\n' | grep -iq '^MOVE$'; then
  echo "!! imap_capability 覆盖未生效（CAPABILITY 仍含 MOVE）：$cap2"
  exit 1
fi
echo "== smail-imap-nomove 已启动（CAPABILITY 无 MOVE）=="
echo "   $cap2"
