# s-mail 设计方案（v0.2）

> **性能承诺修订（2026-10，release 实测）**：M0 期定的"冷启动 <100ms / 内存 <50MB"
> 产生于 GUI 框架选型之前。实际选型 iced+wgpu 后实测：冷启动 ~0.7s（进程→窗口+SQLite
> 就绪）、稳态 RSS ~100MB、strip 后二进制 20MB。修订目标为：冷启动 <1s、RSS <150MB。
> 参照系：Thunderbird 稳态数百 MB。如实记录，不粉饰。

一个用 Rust 编写的原生、轻量邮件客户端。设计目标：**单二进制、冷启动 < 100ms、常驻内存 < 50MB、离线优先、数据归用户所有**。

> v0.2 修订基于三轮深入分析（生态事实核查 → 架构深水区 → 收敛决策），过程与证据见 `docs/analysis/round-{1,2,3}-*.md`。所有 crates.io 数据为 2026-10-18 实测。

臃肿的根源不是功能多，而是：WebView 运行时 + 插件系统 + 强制云集成 + Electron。全部砍掉。

---

## 1. UI 层选型（待定，见文末问题）

| 方案 | 包体 | 内存 | HTML 邮件渲染 | CJK 排版 | 成熟度 |
|---|---|---|---|---|---|
| **iced**（推荐） | ~10MB | 低 | ❌ 需转文本/内嵌 webview | ✅ cosmic-text 好 | 高（System76 在用） |
| egui | ~8MB | 低 | ❌ | ⚠️ 长文排版一般 | 中 |
| Tauri | ~5MB + 系统 webview | 中（每窗 80MB+） | ✅ 完美 | ✅ | 高 |
| ratatati (TUI) | ~3MB | 极低 | ❌ 纯文本 | ✅ | 高（himalaya/meli 验证过） |
| GTK4-rs | 依赖 GTK 运行时 | 中 | 部分 | ✅ | macOS 上体验差 |

**推荐 iced 0.14（MIT，活跃）**，理由：
- 纯 Rust 生态闭环，无系统依赖捆绑问题（macOS 上 GTK 体验差）
- Elm 架构（Model → Message → Update → View）天然适合"收件流 + 状态机"这种 UI
- cosmic-text 的 CJK 断行、字体 fallback 处理好（中文邮件场景重要）
- 官方 `markdown` feature 可直接承接 HTML 邮件降级渲染（见 §5）

**⚠️ 两个一票否决项（M0 spike 前置，见 docs/analysis/round-3 §2）**：
1. **IME**：iced 的 CJK 输入法支持是历史弱项，`text_editor` 中文输入（候选跟随/preedit/上屏）必须在 S1 spike 实测通过，否则 fallback 到 egui，再不行认真考虑 gpui 0.2（Apache-2.0，Zed 已用海量中文用户验证，但文档缺失、API 不稳）
2. **虚拟列表**：iced scrollable 无自动回收，10 万行需按 scroll offset 手动窗口化（S2 spike）

## 2. 协议层：两条路线

### 路线 A：基于 melib（推荐，先跑起来）
[`melib`](https://crates.io/crates/melib) 0.8.13 是 TUI 客户端 meli 的底层库：
- ✅ IMAP / SMTP / maildir / JMAP（feature）后端已实现
- ✅ MIME 解析、charset 处理（含 GBK/GB2312→UTF-8）已踩过坑
- ✅ IMAP 同步降级链（QRESYNC→CONDSTORE→全量）与 provider 怪癖已被 meli 趟过
- ⚠️ **License：`EUPL-1.2 OR GPL-3.0-or-later`，双许可均为 copyleft ⇒ s-mail 必须以 GPL-3.0 开源**（个人使用/开源无成本；闭源则弃此路线）
- ⚠️ 文档少（crates.io 无 doc.rs 链接），API 变动跟随 meli 节奏；TLS 用 native-tls 非 rustls
- ⚠️ 角色定位：melib 是后端引擎（拥有 maildir 写权），SQLite 索引只是我们的视图层，勿双写状态

### 路线 B：自己组装（拒绝 copyleft 时启用，工期 +3~4 周）
- `async-imap` 0.12 — IMAP 客户端（MIT，Delta Chat 维护者维护，活跃；**必须 `default-features=false` + `runtime-tokio`，默认是 async-std**）
- `lettre` — SMTP 发送
- `mail-parser` / `mail-builder` — MIME 解析与构造
- `rustls` / `oauth2`
- ❌ 不要用 `imap` crate（jonhoo）：3.0 卡在 alpha 且 2025-02 后停滞，稳定版还是 2021 年的 2.4.1
- ❌ `himalaya-lib` 已废弃、全版本 yank
- 代价：UIDVALIDITY/QRESYNC/IDLE/provider 怪癖的状态机全部自己写（这是邮件客户端 90% 的坑）

**建议 A 起步，抽象出 trait 后可换 B。** 协议层全部藏在接口后面：

```rust
// backend trait —— 未来加 JMAP/POP3 不动上层
#[async_trait]
trait MailBackend: Send {
    async fn list_folders(&self) -> Result<Vec<Folder>>;
    async fn sync(&self, folder: &FolderId, mode: SyncMode) -> Result<SyncReport>;
    async fn idle(&self, folder: &FolderId) -> Result<Receiver<ServerEvent>>; // IDLE 推送
    async fn fetch_full(&self, id: &MessageId) -> Result<RawMessage>;
    async fn send(&self, msg: OutgoingMessage) -> Result<()>;
    async fn set_flags(&self, ids: &[MessageId], flags: FlagOp) -> Result<()>;
}
```

## 3. 存储设计：maildir + SQLite 双层

原则：**原始数据用开放格式，索引可随时重建**。

```
~/.s-mail/
├── config.toml          # 账号（不含密码）、UI 偏好
├── accounts/
│   └── <account_id>/
│       ├── maildir/     # 标准 maildir++ 格式，原始 EML
│       │   ├── INBOX/{cur,new,tmp}/
│       │   └── .Sent/{cur,new,tmp}/
│       └── index.db     # SQLite：元数据 + FTS5 全文索引
└── credentials/         # 实际密码存 macOS Keychain（keyring crate），不落盘
```

SQLite 表结构（核心）：

```sql
CREATE TABLE messages (
    imap_uid      INTEGER,          -- 联合主键：(folder, uid)
    folder        TEXT NOT NULL,
    message_id    TEXT,             -- RFC822 Message-ID，跨文件夹去重/串联
    in_reply_to   TEXT,             -- 会话视图
    subject       TEXT,
    from_addr     TEXT,
    to_addrs      TEXT,             -- JSON
    date          INTEGER,          -- 服务器时间戳
    internal_date INTEGER,
    snippet       TEXT,             -- 前 200 字符纯文本
    flags         INTEGER,          -- 位标志：\Seen \Answered \Flagged \Draft...
    size          INTEGER,
    maildir_path  TEXT,             -- 指向原始 EML
    fetched_full  INTEGER DEFAULT 0, -- 是否已拉全文（列表阶段只拉 header）
    PRIMARY KEY (folder, imap_uid)
);
CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject, body, content=''
);  -- 外部内容模式，正文懒索引
```

**CJK 检索方案（v0.2 修订，原"自定义 tokenizer"路线已废弃——该 crate 不存在）**：
- 索引与查询两侧共用一个 `tokenize()`：ASCII 词原样保留，CJK 连续段滑窗切二元并以空格连接（`"发票报销" → "发票 票报 报销"`），标准 unicode61 tokenizer 直接可用
- 零额外依赖；人名/新词 recall 最好；代价是索引约为原文 2 倍（可接受，附件不入索引）
- 高亮：bigram 记录原文 offset，命中后换算回原文区间
- FTS5 是否随 rusqlite `bundled` 编入 → M0 用一条 `CREATE VIRTUAL TABLE` 集成测试验证
- 不选 trigram（查询必须 ≥3 字符，"发票"这类高频双字词查不到）；tantivy 留作 M5 升级项

为什么不用 redb/sled：maildir 让用户可以用任何工具（mutt、grep、time machine）直接访问原始数据，**数据主权**是反臃肿客户端的道义基础。

## 4. 同步模型：离线优先

状态机：`本地 SQLite 是 UI 的唯一数据源`，IMAP 只是同步协议。

```
        ┌────────── UI 只读本地库 ──────────┐
        │                                   │
   ┌────▼─────┐    IDLE 推送          ┌─────┴──────┐
   │  SQLite  │◄──────────────────────│  IMAP 会话 │
   │ (真相源) │───flags 变更队列──────►│ (长连接x1) │
   └──────────┘   (含 tombstone)      └────────────┘
```

关键点：
1. **首屏快**：初始同步只拉 ENVELOPE + UID + FLAGS（一条邮件几百字节），正文懒加载（打开时 `fetch_full`）
2. **推送**：IMAP IDLE 长连接（每账号 1 条到 INBOX，其余文件夹轮询或 QQ/Gmail 支持的特殊通知）
3. **增量**：优先 `QRESYNC`/`CONDSTORE`（modseq），服务器不支持时降级 `UID SEARCH UNSEEN` + UIDVALIDITY 处理
4. **离线写**：删信/标记星标先写本地 + 操作队列，联网后回放；冲突以服务器为准（tombstone 记录本端删除意图）
5. **全文索引**：后台线程逐条解析 maildir 并喂 FTS5，空闲时进行，避免抢占 UI

## 5. HTML 邮件渲染（iced 的短板，需明确策略）

分层降级（v0.2 具体化）：
1. **默认**：`ammonia`（4.2.4，html5ever 白名单消毒：去 script/事件/危险协议）→ DOM 遍历产出结构化块（标题层级/段落/列表/引用层级/链接/表格→缩进块）→ **iced markdown widget 渲染**（0.14 官方 feature）。90% 正经邮件够用
2. **CID 内嵌图片**：从附件提取后用 iced image 渲染；**远程图片默认禁**（防跟踪像素），per-mail 开关
3. **正文卡片固定浅色背景**：HTML 邮件假定白底，不跟深色模式反转
4. **兜底**：复杂邮件（发票/表格类）提供"在浏览器打开导出的 HTML"，比内嵌 wry webview 便宜得多
5. 附件：点击 → 保存到磁盘 → 系统默认程序打开，不内嵌预览

## 6. 凭据与安全

- 密码/OAuth token：`keyring` crate → macOS Keychain（绝不明文落盘）
- TLS：rustls，拒绝明文协议
- Gmail/Outlook：XOAUTH2 流程（很多用户没有"应用专有密码"概念了）
- 防钓鱼基础：回信地址与显示名不一致时提示（不做过度安全功能）
- PGP：后期用 `sequoia`（先不进 MVP，避免范围蔓延）

## 7. 架构总览

```
┌────────────────────────────────────────────┐
│                 iced UI 层                  │  views: 邮件列表/会话/编辑器/设置
├────────────────────────────────────────────┤
│                App State                   │  Elm: update(Message) -> State
├──────────────┬─────────────────────────────┤
│  service 层   │  sync_mgr / composer /     │  tokio 任务，channel 与 UI 通信
│              │  search / account_mgr       │
├──────────────┴─────────────────────────────┤
│            backend trait (+melib)          │  IMAP/SMTP，可替换实现
├────────────────────────────────────────────┤
│        storage: maildir + SQLite(FTS5)     │  rusqlite
└────────────────────────────────────────────┘
```

crate 组织（workspace）：

```
s-mail/
├── Cargo.toml          # workspace
├── crates/
│   ├── smail-ui/       # iced 前端
│   ├── smail-core/     # service 层 + 状态机（不含 UI 依赖，可测试）
│   ├── smail-backend/  # trait + melib/imap 实现
│   └── smail-store/    # maildir + SQLite
```

核心依赖清单（刻意保持短）：
`iced` `melib`(或 async-imap+lettre) `rusqlite`(bundled+fts5) `tokio` `keyring` `serde`/`toml` `mail-parser`(若走路线B) `wry`(可选HTML)

## 8. 里程碑

| 阶段 | 内容 | 验收标准 |
|---|---|---|
| M0 | workspace + backend trait + maildir/SQLite 读写 | ✅ 完成（4 spike 全验：FTS5/bigram 自动测试、IME/列表 GUI 冒烟、melib+dovecot 全链路含 IDLE） |
| M1 | **单账号 IMAP 收信**：列表、读纯文本邮件、IDLE 收新信 | ✅ 代码完成：headless 端到端通过（连接→同步→开信→正文落盘/解析），GUI 冒烟通过；待真实账号日常使用检验 |
| M2 | 发信(SMTP) + 草稿 + flags 双向同步 + 断线重连 | ✅ 代码完成并本地 e2e 通过：发信→SMTP 250→Sent 归档→删除（examples/m2 PASS）；断线→指数退避→自动重同步（headless 实测）；outbox 每 6s 重试；打开即标已读。待真实账号吃狗粮 |
| M3 | FTS5 全文搜索 + 附件收发（多账号移至 M3-b） | ✅ 本地 e2e 通过（examples/m3）：带附件发信→中文文件名附件可见→保存内容一致→主题/正文 bigram 搜索全命中；后台索引器每 3s 批量补正文索引；GUI 冒烟通过。待真实账号吃狗粮 |
| M3-b | 多账号 | ✅ e2e PASS（examples/m5，config.multi.toml 双账号）：[[accounts]] 配置、每账号独立服务线程+数据目录（slug 去重、中文名回退 username）、TaggedEvent 多路复用、UI 账号切换、全账号联合搜索；内容隔离经 SQLite 逐库核验（test/ 无二号主题、test2/ 全为二号）。GUI 多账号冒烟 12s 通过 |
| M4 | HTML 渲染管线（ammonia 消毒 → scraper DOM → markdown 子集 → iced markdown widget；远程图/script 全禁） | ✅ e2e PASS（examples/m4 + inject_html_mail.py）：中文商业邮件的标题/加粗/列表/表格/链接全保留，script/追踪像素全删；5 个单元测试覆盖转换器。链接点击 → 默认浏览器（仅 http/https）。GUI 冒烟通过 |
| M4-b | XOAUTH2（Gmail/Outlook 现代认证）+ 删除二次确认 + HTML 深色模式适配 | ✅ e2e PASS（examples/m6 + mock_oauth.py + mock_xoauth2_proxy.py）：设备流（RFC 8628，pending→授权）→ melib use_oauth2（线上 SASL 串实证）→ 临期自动刷新（重连路径）→ lettre XOAUTH2 SMTP（线上 Bearer 实证）；token 落盘 oauth.json(0600)；CLI `s-mail auth <name>`；删除二次确认条；HTML 正文固定浅色卡片。gmail/microsoft/custom 三 provider，真实账号待吃狗粮 |
| M5-a | 会话视图聚合（In-Reply-To 线程化，JWZ 简化版） | ✅ e2e PASS（examples/m7 + seed_thread.py 三级回信链）：in_reply_to 落库（store schema v3）→ group_threads 聚合（5 单测：链/孤儿/归一化/环/自指）→ UI 会话根行（N 封徽章、最新活动排序、点击展开+开最新）、🧵/🔀 顶栏切换；99 封无丢失回归 |
| M5-b | 规则过滤（配置式：from/subject 匹配 → 标已读/星标/移动） | ✅ e2e PASS（examples/m8 + seed_rules.py 五封含对照组）：promo×2 自动已读、周报自动星标、moveme 服务器端 MOVE 到 Sent（INBOX 消失+Sent 出现）、对照组零副作用；规则只打新邮件（幂等）；自移动防护（dest==folder 跳过）；GUI 冒烟时独立数据目录二次验证 |
| M5-c | PGP 收信侧（sequoia：解密 PGP/MIME + 验签内嵌/detached） | ✅ e2e PASS（examples/m9，自生成密钥+自造加密/签名邮件经 dovecot 全链路）：解密后正文含机密内容且 HTML 管线复用；4 单测（加签解往返、篡改必败、MIME 拆解、密钥文件加载）。crypto-rust 后端为狗粮取舍（非常数时间），开源发布前应切 nettle/openssl。发信签名/加密待做（加密器代码已在 pgp.rs 备好） |
| M5-d | PGP 发信侧（compose 签名/加密开关 → build_outgoing → send_raw） | ✅ e2e PASS（examples/m10 + mini_smtp 多连接）：加密邮件 1505B（multipart/encrypted）、签名邮件 931B（multipart/signed）线上结构正确；**自我互操作**（自己发的加密信被自己的收信侧解密+验签）；Sent 归档服务器真相；加密自动含 encrypt-to-self；收件人缺公钥=永久错误标 failed 不进重试队列；5+1 单测 |

| M6 | 应用内账号配置（引导屏 + 预设 + 连接测试 + 热重载 + GUI 设备流） | ✅ e2e PASS（examples/m11）：save_accounts 写读往返、test_connection（dovecot 2 文件夹）、OAuth 账号无 token 起步 → OAuthPrompt 事件（GUI 横幅数据源）→ 服务线程自动设备流授权 → 同步成功；UI：首启直进引导、预设 QQ/163/Gmail/Outlook/自定义、保存即热重载会话（Drop join 旧线程保数据目录独占）、顶栏 ＋ 账号管理（删除）、Esc 层级退出扩展 |
| M6-b | 深挖修复（数据正确性轮） | ✅ e2e m12（设备流中断 0.83s，修复 UI 冻结）+ m13（他处删信本地自动清理，全量 diff 增删双向）；加密邮件密文不再进 FTS（index_pass 跳过）；切文件夹滚动重置；账号重载清草稿；测试基建：imap_delete.py（他处删除模拟）、m13 自播种唯一靶子 |

| M7 | 补齐操作层（手动移动/批量/文件夹管理/通知/联系人）+ 安全修复 | ✅ e2e m14（全链路 PASS）：①手动移动（backend.move_messages 首次暴露给 UI，读信区「移动到」面板 + 乐观更新 + 本地缓存清理）；②批量操作（☑多选 → 批量已读/未读/星标/移动/删除，按 folder 分组一次 IMAP 往返，二次确认；搜索结果跨账号可批量）；③文件夹管理（侧栏新建/删除，melib create/delete_mailbox；INBOX 禁删；删时同步清 messages+FTS+正文缓存）；④系统新邮件通知（INBOX 增量未读、初次全量不打扰，osascript/notify-send 参数化调用，SMAIL_NO_NOTIFY 可关）；⑤联系人补全（store.recent_senders 去重取最近，compose 收件人/抄送末段匹配、点击回填保留前缀）。安全：修复 MelibBackend::new 的 TOML 拼接注入（改 serde 序列化自动转义，密码含 "/换行不再破坏配置）；m13 判定竞态修复（本轮唯一触发信快照） |

## 9. 已识别风险（v0.2，详见 docs/analysis/round-3 §3）

1. **iced IME 不达标**（高，一票否决）→ S1 spike 前置；fallback：egui → gpui
2. **虚拟列表性能**（高）→ S2 spike；egui `show_rows` 为现成 fallback
3. **IMAP 服务器怪癖**（高）：QQ/163/Gmail 对 QRESYNC/IDLE 支持不一 + 163 需 IMAP `ID` 命令 + QQ 用授权码 + Gmail 标签去重 —— melib 吸收大部分，S3 实测三家
4. **charset**：中文邮件 GBK 仍大量存在，解析层必须有 GBK→UTF-8 的回归测试集
5. **melib API 不稳/文档少 + copyleft**：trait 隔离是技术保险；GPL 决策已定（§10）。**实证：melib 0.8.13 依赖 pre-release 链（imap-codec/imap-types alpha），必须精确钉 `=2.0.0-alpha.4` / `=2.0.0-alpha.3` 才能编译，钉子要带进 smail-backend**；独立使用需先 `is_online()`、extra 配置为顶层 flatten 字符串、watch 事件会重复到达（已验证，spike 注释有记录）
6. **范围蔓延**（单人项目自伤类）：里程碑门禁，M2 吃狗粮前不加任何 M3+ 功能

## 10. 决策状态（v0.2）

1. **✅ 已决策：接受 GPL-3.0**（自己用或开源）⇒ melib 路线锁定，license 定为 `GPL-3.0-or-later`
2. 主用邮箱：未答复 ⇒ S3 spike 为 config 驱动（`spikes/spike-melib/config.toml` 填哪家测哪家）

> 原问题 1/2/4（UI 框架、HTML 策略、协议路线）已收敛为"有前提的默认值 + M0 spike 验证"，见 `docs/analysis/`。spike 代码在 `spikes/`，验收标准见 `docs/analysis/round-3-conclusions.md` §2。
