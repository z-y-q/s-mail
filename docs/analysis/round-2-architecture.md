# 第二轮：架构深水区分析

第一轮核实了生态事实，这轮拆四个真正会淹死人的深水区：同步正确性、中文搜索、UI/IME、HTML 渲染，外加一个备选架构（mbsync 混合）的严肃评估。

## 1. IMAP 同步正确性：90% 的坑都在这

邮件客户端失败的典型方式不是"功能写不出来"，而是同步状态机在边界条件下丢信/复读/标记错乱。必须处理的最小集合：

### 1.1 UID 体系三定律
1. `(folder, UIDVALIDITY, uid)` 三元组才能全局定位一封邮件。**每次 SELECT 都要检查 UIDVALIDITY**，变了就作废该文件夹全部本地 UID、全量重拉。漏掉这条 = 换服务器目录重建后本地索引张冠李戴。
2. UID 是稀疏的：EXPUNGE 留洞，`max_uid ≠ 邮件数`，永远不要用序号换算。
3. UIDNEXT 只是预告：服务器可以合法地"跳号"。

### 1.2 增量同步降级链
```
QRESYNC (modseq 精确增量，连 EXPUNGE 都能补)
  └─ 不支持 → CONDSTORE (CHANGEDSINCE 拉 flags 变更，EXPUNGE 靠 UID 集合差)
       └─ 不支持 → UID SEARCH + FETCH FLAGS 全量比对（文件夹 ≤ 数千封时可接受）
```
- Gmail：QRESYNC ✅；QQ：IDLE ✅ / QRESYNC 大概率 ❌（按 1.2 降级）；163/126：IDLE ✅（有前置条件，见 1.4）/ QRESYNC ❌。
- 这正是 melib 路线的核心价值：这套降级链 + 各家怪癖 meli 已经趟过。

### 1.3 IDLE 的隐形陷阱
- 服务器会**静默掐掉**空闲连接（常见 ~30 分钟量级），必须周期性重发 IDLE 并把"任何响应/超时"都当作"该文件夹可能变了"来触发一次廉价同步。
- IDLE 只监视当前选中文件夹 —— 多文件夹推送要么多连接（Gmail 限制连接数），要么 INBOX 专用 IDLE + 其余轮询。
- IDLE 通知不带变更明细，醒来后还是要 `FETCH ... FLAGS` 或 QRESYNC 补差。

### 1.4 国内 provider 怪癖（凭社区经验，M0 实测复核）
- **163/126**：连接后必须先发 IMAP `ID` 命令报客户端身份，否则报 "Unsafe Login" 拒绝。melib 是否默认发 ID → S3 spike 验证。
- **QQ 邮箱**：用"授权码"而非登录密码（UI 引导要专门做）；IDLE 可用。
- **Gmail**：标签=文件夹，同一封信在 All Mail + INBOX 出现两次 → 按 Message-ID 去重；文件夹名 `[Gmail]/...` 会本地化 → 靠 SPECIAL-USE 属性识别而非名字；基本认证只剩"应用专用密码"（需两步验证）。
- **Outlook**：消费版 basic auth 已死，XOAUTH2 设备码流程是 M4+ 的事，先不做。

### 1.5 离线写队列（outbox 模式）
- 本地删信/标记/移动先写 SQLite 操作队列 + tombstone（记录删除意图），联网后按序回放；服务器为准。
- 发信用 outbox 表：SMTP 成功 → APPEND 到 Sent → 本地标记已发；失败 → 指数退避重试；**用自生成 Message-ID 做幂等键**，防止重试造成重复投递。

## 2. 存储与索引：修正后的设计

第一轮推翻了"自定义 FTS5 tokenizer"路线（crate 不存在），改为**应用层 bigram 预切分**：

### 2.1 CJK 搜索方案对比

| 方案 | 原理 | 优点 | 缺点 |
|---|---|---|---|
| **A. bigram 预切分**（推荐） | 索引时把中文切成二元词组用空格连接："发票报销"→`发票 票报 报销`；查询同样处理；unicode61 直接可用 | 零新依赖、人名/新词 recall 最好、确定性 | 索引约为原文 2 倍；高亮需 offset 换算 |
| B. jieba 分词预切分 | 词级切分后空格连接 | 索引小 | 新词/人名切不出 → 漏检；词典维护 |
| C. trigram tokenizer | FTS5 内建 | 零代码 | **查询必须 ≥3 字符**，"发票"这类双字词查不到 → 否决 |
| D. tantivy | 独立全文引擎 | BM25/增量索引好 | 多一套索引目录和依赖，M5 再评估 |

- 实现：一个 `tokenize_for_index(text) -> String`，ASCII 词原样保留、CJK 连续段滑窗切二元，索引/查询两侧共用。
- 高亮：bigram 记录原文 offset，命中后换算回原文区间再 substring。
- FTS5 本身是否在 rusqlite bundled 构建里启用 → M0 用一条 `CREATE VIRTUAL TABLE ... USING fts5` 集成测试验证（未验证前不写进承诺）。

### 2.2 melib 路线下的角色重划分
若走 melib：**melib 是后端引擎（IMAP ↔ maildir 同步 + watch 事件），SQLite 索引是我们的私有视图**（walk maildir + 订阅 melib 的 refresh 通知来增量更新）。不要让 melib 和我们的索引各自为政地去写两份状态。

### 2.3 SQLite 使用纪律
- WAL 模式；单写连接跑在专职线程，命令走 channel（避免锁竞争埋雷）。
- FTS 外部内容表（content=''，手动喂）避免双份存储。
- 迁移：`PRAGMA user_version` + 顺序迁移脚本。

## 3. HTML 邮件降级管线（最终版）

```
原始 text/html
  → ammonia 白名单消毒（删 script/事件/危险协议；保留结构标签）
  → DOM 遍历 → 结构化块（标题层级/段落/列表/引用层级/链接/表格→缩进块）
  → iced markdown widget 渲染（0.14 官方 feature）
```
- CID 内嵌图片：从附件提取后用 iced image 渲染；远程图片默认禁（防跟踪像素），per-mail 开关。
- 邮件假定白底：正文卡片固定浅色背景，不跟深色模式反转（否则一票 HTML 邮件配色崩坏）。
- 兜底："原文太复杂？[在浏览器打开导出的 HTML]"——比内嵌 webview 便宜得多。

## 4. UI 决战点：IME 与虚拟列表（两个一票否决项）

### 4.1 IME（中文输入法）
写中文邮件是日常主路径，IME 不可用 = 项目失败。事实：
- iced 的 CJK IME 历史上是弱项（text_editor 重写后改善，但 macOS 上 preedit 展示/候选交互必须实测）。
- egui 通过 winit 有 IME 事件，质量参差。
- gpui 在 Zed 里被海量中文用户日常验证过 —— 这是它唯一的决定性优势。

**S1 spike（M0 第一优先）**：iced 0.14 `text_editor` 里用拼音输入法写一段话，验证：候选框跟随光标、preedit 内联显示、回车上屏正确。不过关 → 换 egui 重测 → 再不过 → 认真考虑 gpui。

### 4.2 虚拟列表
- iced scrollable 不做自动回收，10 万封邮件全量构建 widget 必掉帧。方案：按 scroll offset 计算可见窗口，只构建 ±缓冲区行（iced 例子界有此模式，需自己写 ~150 行）。
- egui `ScrollArea::show_rows` 原生支持。
- **S2 spike**：10 万行模拟数据滚动 + 帧率记录。

### 4.3 egui 的隐性成本
MSRV 1.95 意味着每次升级可能要求最新工具链，发布节奏 6 周一版；长期维护要接受"追新"。iced MSRV 1.88 相对温和。

## 5. 备选架构：mbsync 混合（严肃评估后降级为备份）

思路：IMAP 同步外包给 isync/mbsync（C，最坚固的 IMAP 同步实现），s-mail 只做 UI + maildir 读取。

| | 结论 |
|---|---|
| 同步可靠性 | ✅ mbsync 处理 UIDVALIDITY/QRESYNC/怪癖最成熟 |
| 推送 | ❌ **isync 至今不支持 IDLE**（长期 open 的 feature request；据我所知，未实测复核）——推送要自己再写一个 IDLE 监听器触发 mbsync，复杂度只是转移不是消除 |
| 分发 | ❌ 外部二进制依赖（用户装 brew/apt，或我们捆绑 GPL 程序——聚合分发合规但麻烦） |
| 错误路径 | ❌ 解析 mbsync 输出/配置生成/进程管理都是胶水代码 |
| 结论 | melib 失败时的 Plan C，不是主线 |

## 6. 进程/线程模型

```
main thread        iced runtime (winit 事件循环, macOS 强制)
  │ mpsc
service 层         tokio 任务: account_mgr / sync / outbox_sender / indexer
  │                 └─ melib backend 线程 (maildir + IMAP 会话, IDLE)
  │ channel
storage 线程        SQLite 单写连接 (WAL)
```
- iced 开 `tokio` feature，服务任务返回值经 `Subscription` 回流 UI。
- 所有网络在服务层，UI 永不碰 IO —— Elm 边界干净也方便 smail-core 纯逻辑测试。
