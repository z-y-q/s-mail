# 吃狗粮手册（真实账号）

目标：用 s-mail 作为日常主力客户端，发现并修复真实世界问题。本文是准备清单 + 记录模板。

## 0. 前置检查（已完成 ✅）

- release 构建：`cargo build --release`（二进制 ~37MB / strip 后 ~26MB）
- 全部 e2e：m2~m18（其中 14 个进 CI 矩阵）+ 86 项单元/文档测试通过
- 性能实测：冷启动 ~0.7s、稳态 RSS ~100MB（详见 DESIGN.md 修订）

## 1. 账号接入清单

### 1.1 QQ / 163（最快上手，授权码模式）

1. 网页版开 IMAP/SMTP，生成授权码
2. `~/.s-mail/config.toml` 照 `config.example.toml` 填（QQ: 993/465 + ssl）
3. 启动即用。重点观察：中文信封解码、IDLE 推送及时性、断网重连

### 1.2 Gmail（XOAUTH2）

1. Google Cloud Console 建 OAuth 客户端（类型：桌面应用），拿 client_id/secret
2. OAuth 同意屏加 Test users（自己的邮箱），否则设备流报 403 Policy check failed
3. config 照 example 的 `[accounts.oauth]` provider="google" 段填
4. `./s-mail auth Gmail` → 浏览器开链接、输码、授权
5. 启动。重点观察：token 刷新（1 小时后是否无感续期）、Gmail 特有文件夹
   （`[Gmail]/All Mail` 同步量是否爆炸——必要时配 rules folder=INBOX 限定）

### 1.3 Outlook（XOAUTH2）

1. Entra ID 注册应用（平台：移动和桌面应用），开启 "Allow public client flows"
2. config 照 example 的 microsoft 段（tenant=consumers）
3. `./s-mail auth Outlook` → 设备流
4. 重点观察：office365.com 端点、IMAP scope 是否被租户策略拦

## 2. PGP 互操作（与 Thunderbird/gpg 对打）

1. 导入自己的既有私钥：`gpg --export-secret-keys --armor KEYID > ~/.s-mail/<slug>/pgp/secret.asc`
2. 把通信方公钥合并进 `pub.asc`（可多个证书 append）
3. 给装了 gpg 的另一台机器/Thunderbird 发加密+签名邮件，验证对方能解能验
4. 反向收对方的 PGP 邮件，看我们的徽章（这是单测没覆盖的真互操作）
5. 已知风险点：行尾规范化（我们内层全 base64 规避了）、armor 标签、micalg 参数

## 3. 记录模板（每天 5 分钟）

发现问题时记一行：

```
日期 | 场景（收/发/搜/附件/PGP） | 现象 | 复现步骤 | 严重度（挡用/烦人/ cosmetic）
```

挡用问题优先修。数据目录随手可查：`sqlite3 ~/.s-mail/<slug>/index.db`、
`ls ~/.s-mail/<slug>/bodies/ | head`。

## 4. 已知边界（则当 bug 报）

- 加密邮件正文不进全文索引（安全默认）
- 删除 = 进废纸篓（可恢复）；废纸篓内再删才是服务器永久删除
- PGP 发信失败不自动重试（安全取舍）
- HTML 邮件不加载远程图片（反追踪设计，可点击手动加载）
- 原生子视图（原样渲染的网页视图）浮在一切画面之上：切设置屏/弹全屏
  横幅应自动隐藏，若再见到「浮错屏」按 bug 报（2026-10-31 加了显隐
  不变式；历史教训：松手事件被子视图吞掉、切屏后残留）

历史边界（已修复，则再当 bug 报）：

- 服务器端他处删信：已自动清理本地行（全量 diff 以服务器真相为准，m13 e2e）
