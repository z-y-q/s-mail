#!/bin/sh
docker rm -f smail-imap >/dev/null 2>&1 || true
echo "s-mail 本地 IMAP 测试服务器已停止并移除"
