# s-mail

一个用 Rust 写的原生轻量邮件客户端。单二进制、离线优先、数据归你所有。

受够了臃肿的邮件客户端之后的作品：把“能自己编译、自己修、数据在自己磁盘上”当作底线，
性能与隐私当作品味。

## 特性

| 能力 | 状态 |
|---|---|
| 多账号 IMAP 收信（IDLE 实时推送、断线指数退避重连、CONDSTORE/QRESYNC 增量同步） | ✅ |
| 统一收件箱（多账号 INBOX 聚合 + 未读总数 + 快速过滤 未读/星标 + 排序切换） | ✅ |
| SMTP 发信（outbox 队列、Sent 归档、flags 双向同步、密送 Bcc、签名、一键退订） | ✅ |
| XOAUTH2 现代认证（Gmail / Outlook 设备流，token 自动刷新） | ✅ |
| 全文搜索（SQLite FTS5 + 中文 bigram，主题即搜 + 后台正文索引） | ✅ |
| 附件收发（rfd 选文件、保存到下载目录） | ✅ |
| HTML 邮件安全渲染（ammonia 消毒 → markdown 子集；内嵌图自动显示，远程图点击加载） | ✅ |
| 会话视图（In-Reply-To 线程聚合，可切平铺） | ✅ |
| 规则过滤（from/subject 匹配 → 批量自动已读/星标/移动） | ✅ |
| 手动移动/归档（读信区「移动到」/「🗃 归档」+ 批量操作 + e 键快捷归档） | ✅ |
| 批量操作（多选 → 批量已读/星标/移动/删除，搜索结果跨账号可用） | ✅ |
| 文件夹管理（侧栏新建/改名/删除，IMAP CREATE/RENAME/DELETE，中文 mUTF-7 名支持） | ✅ |
| 本地优先切换（索引即出列表 + 60s 重同步 TTL + ↻/F5 手动刷新） | ✅ |
| flags 双向同步（服务器已读状态回传 + 断网本地改动重连回放） | ✅ |
| 系统新邮件通知（macOS osascript / Linux notify-send，SMAIL_NO_NOTIFY 可关） | ✅ |
| 联系人自动补全 + 通讯录（收信自动收集，设置屏管理） | ✅ |
| 垃圾处理（标为垃圾/移回收件箱；v1 无自学习） | ✅ |
| PGP 收信（PGP/MIME 解密 + 签名验证，含明文签名；密钥环进程内缓存） | ✅ |
| PGP 发信（compose 一键签名/加密，encrypt-to-self） | ✅ |
| PGP 配置 UI（设置屏账号列表内联编辑，文件选择器，~/ 路径展开） | ✅ |
| 大文件夹分页（触底自动加载，每页 500 封） | ✅ |
| WKD 公钥自动发现（默认关，PGP 设置里开启） | ✅ |
| 密码入系统钥匙串（macOS Keychain / Linux secret-service，config 只存标记） | ✅ |
| 导出 .eml（读信区 💾，保留原始邮件字节） | ✅ |
| 键盘快捷键（↑↓ 导航 / r 回复 / a 全部回复 / f 转发 / s 星标 / e 归档 / Delete 删除 / F5 刷新 / Esc 退出链） | ✅ |
| 系统托盘（macOS：常驻图标 + 打开/退出菜单，左键点击激活） | ✅ |
| 深色模式（全套主题适配，含 HTML 邮件卡片随主题切换；随系统自动，顶栏 🌙/☀️ 可覆盖；手动选择记入 ui.json） | ✅ |
| 界面语言（中文默认，顶栏 EN/中 一键切换；特殊文件夹（收件箱/已发送/垃圾/归档）本地化显示；偏好持久化） | ✅ |
| AI 助手（✨摘要 / ✨起草 / ✨智能回复 / ✨整理建议；AI 只出建议，发送/删除等执行永远人工——编译期模块隔离 + 操作白名单 + 人工点击三层防线） | ✅ |
| 转发带附件 / 正文 mailto: 链接开写 / 图片+PDF(QuickLook) 附件预览 / 设置屏签名编辑 / 窗口尺寸位置记忆 | ✅ |
| 通知点击跳转（FIFO 队列逐封跳转；需 .app bundle） / 搜索正文片段（FTS5 snippet 高亮上下文） | ✅ |
| 删除=进废纸篓（识别/自动创建 Trash；篓内才永久删；清空废纸篓；确认文案分级） | ✅ |
| 发件箱（断网排队可见；失败明确展示+一键重发/删除；成功即时归零） | ✅ |
| 断连持续黄条提示（重连自动消除）/ 同步进度 / 离线启动直达缓存（重启不依赖服务器） | ✅ |
| 草稿箱、发件箱（断网自动重试；草稿云同步：上传/换版替换/删除清理 + **双向对账**——其他设备的编辑/删除自动回拉） | ✅ |

## 快速开始

```bash
cargo build --release
./target/release/s-mail        # 首次启动直接进入账号引导（QQ/163/Gmail/Outlook/自定义）
```

- QQ/163：填邮箱 + 授权码，点「测试连接」即时验证，保存即用
- Gmail/Outlook：填 client_id（Google Cloud / Entra ID 控制台建），保存后窗口里直接弹
  授权链接和验证码（设备流全自动，token 过期自动刷新）
- 手写配置仍然支持：`~/.s-mail/config.toml`（参考 `spikes/spike-melib/config.example.toml`），
  或终端跑 `./s-mail auth 账号名` 走 CLI 授权

数据都在 `~/.s-mail/<账号>/`：原始 EML 落盘 `bodies/`，SQLite 只是可重建的索引
（删掉 `index.db` 重新同步即恢复）。PGP 密钥放 `~/.s-mail/<账号>/pgp/`，
也可直接在设置屏（右上角「＋账号」→ 已有账号行「PGP」按钮）配置。

## 快捷键

| 键 | 动作 |
|---|---|
| ↑ ↓ | 上一篇/下一篇（自动打开 + 滚动跟随） |
| r / a / f | 回复 / 全部回复 / 转发 |
| s | 星标切换 |
| e | 归档（Gmail/Apple Mail 惯例） |
| Delete | 删除（带确认条） |
| F5 | 手动刷新当前文件夹 |
| Esc | 层级退出（写作→设置→确认→侧栏输入→搜索→清词） |

字母键在读信时可用；搜索框有内容或侧栏输入中不会误触发。

## 配置一瞥

```toml
[[accounts]]
name = "工作"
[accounts.imap]
host = "imap.example.com"
port = 993
username = "you@example.com"
password = "授权码"          # XOAUTH2 账号省略；或 "keychain:you@example.com/imap"
                             # （配合设置屏 🔐 按钮：真实密码入系统钥匙串，不落明文）
[accounts.smtp]
host = "smtp.example.com"
port = 465
username = "you@example.com"
password = "授权码"
tls = "ssl"
from = "you@example.com"

# 可选：发信签名（新邮件预填尾部，回复/转发插在引用前）
signature = """
张三
电话 138xxxx
"""

# 可选：规则过滤（v2 格式：条件组 + 谓词 + 多动作 + 命中链）
[[accounts.rules]]
all_of = [{ field = "from", predicate = "ends_with", value = "@corp.x" }]
any_of = [
  { field = "subject", predicate = "starts_with", value = "周报" },
  { field = "subject", predicate = "contains", value = "月报" },
]
actions = ["MarkSeen", "Flag"]        # 可多动作；移动用 { Move = "Archive" }
# folder = "INBOX"                    # 只在该文件夹应用（缺省=任意）
# continue_after = true               # 命中后继续试后续规则（默认 false=首条命中即用）

# 旧格式（v0）仍完全兼容——加载时自动转换为新格式，保存只写新格式：
[[accounts.rules]]
from = "noreply@"                      # 等价 all_of(from contains)
mark_seen = true

# 可选：PGP（收信解密/验签 + 发信签名/加密）
[accounts.pgp]
key_path = "~/.s-mail/work/pgp/secret.asc"
pubring_path = "~/.s-mail/work/pgp/pub.asc"
```

完整注释见 `spikes/spike-melib/config.example.toml`。

## 架构

```
┌────────────┐   tokio unbounded (事件)   ┌─────────────────────┐
│  smail-ui  │◄─────────────────────────── │     smail-core      │
│  (iced)    │──std mpsc (命令)───────────►│ 每账号一服务线程     │
└────────────┘                             │ ┌─────────────────┐ │
                                           │ │  smail-backend  │ │
                                           │ │ melib(IMAP) +   │ │
                                           │ │ lettre(SMTP)    │ │
                                           │ └─────────────────┘ │
                                           │ ┌─────────────────┐ │
                                           │ │  smail-store    │ │
                                           │ │ SQLite+FTS5     │ │
                                           │ └─────────────────┘ │
                                           └─────────────────────┘
```

- **存储纪律**：原始邮件（EML）在磁盘，SQLite 索引可随时重建；schema 用
  `PRAGMA user_version` 迁移
- **HTML 管线**：ammonia 白名单消毒 → scraper DOM → markdown 子集 → iced
  markdown widget；远程图片、script、追踪像素一律删除
- **PGP**：sequoia-openpgp；解密结果不进全文索引（安全默认）；发信缺收件人
  公钥直接拒绝（宁可不发不错发）

## 测试

每个里程碑都有 headless 端到端测试（本地 dovecot + mock 基建，`spikes/spike-melib/`）：

```bash
sh spikes/spike-melib/imap-test/up.sh      # 本地 dovecot（rootless docker，含 Drafts）
cargo test --workspace                     # 单元测试
cargo run -p smail-core --example m5       # 多账号 e2e（m2~m15 各验证一个里程碑）
```

m6（XOAUTH2）、m9/m10（PGP）自带 mock OAuth 服务器与 IMAP 翻译代理，全程本地可复现。
m14（移动/批量/文件夹/联系人）与 m15（草稿云同步）同样全链路 headless，
并已进入 CI e2e 矩阵：

```bash
sh spikes/spike-melib/imap-test/up.sh
SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m15 \
  cargo run -p smail-core --example m15 -- 60
```

## 性能（release 实测，Apple Silicon / macOS）

- 冷启动（进程→窗口+数据库就绪）：**~0.7s**
- 稳态内存：**~100MB**（对比 Thunderbird 数百 MB；iced/wgpu 运行时占大头）
- 二进制：~26MB（strip 后；CI 有体积与核心性能门禁，见 perf job）

GUI 冷启动/RSS 无法无头测量，保持手工记录；CI perf job 门禁核心指标
（万封：冷开/批量写入/FTS 检索/列表查询）与 strip 后体积。

## 许可

GPL-3.0-or-later（melib 为 EUPL/GPL 双许可，本项目选 GPL 保证合规）。

## 路线图

- [ ] Windows 构建验证进 e2e（CI 已有 check-windows 编译门禁）

设计与里程碑全记录见 [DESIGN.md](DESIGN.md)。

## 打包（macOS .app）

```sh
sh scripts/build-app.sh   # 产物 dist/s-mail.app（release + 图标 + ad-hoc 签名）
open dist/s-mail.app
```

打包解锁：Dock 图标与名称、通知归属本应用、Finder 双击启动与 .eml 文件关联。
