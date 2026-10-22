# 第三轮：收敛决策（v0.2）

两轮分析后的修订结论。与 DESIGN.md v0.1 的差异已标注。

## 1. 决策表

| 决策点 | v0.1 假设 | v0.2 结论 | 依据 |
|---|---|---|---|
| UI 框架 | iced（无前提） | **iced 0.14 为前提性主线，IME spike 一票否决**；fallback: egui → gpui | R1§3, R2§4 |
| 协议层 | melib（未查 license） | **melib 0.8.13，接受 EUPL/GPL-3.0 双许可 ⇒ s-mail 定为 GPL-3.0 开源**；若拒绝 copyleft 则换 async-imap 0.12 + lettre 自建（+3~4 周） | R1§1 |
| `imap` crate | 未评估 | 排除（alpha 卡三年） | R1§2 |
| himalaya-lib | 未评估 | 排除（已废弃全 yank） | R1§2 |
| 搜索 | "FTS5 + 自定义/jieba tokenizer"（含糊） | **应用层 bigram 预切分 + unicode61**，零额外依赖；tantivy 留 M5 | R1§4, R2§2 |
| HTML 渲染 | "降级为文本"（含糊） | **ammonia → 结构化块 → iced markdown**；远程图默认禁；浏览器打开做兜底 | R2§3 |
| mbsync 混合 | 未评估 | 降级为 Plan C（无 IDLE，复杂度转移而非消除） | R2§5 |
| TLS | rustls | melib 路线下是 native-tls（接受）；自建路线用 rustls | R1§1 |
| 存储 | maildir + SQLite(FTS5) | 不变；补纪律：WAL、单写线程、FTS 外部内容表、melib 为唯一 maildir 写者 | R2§2 |

## 2. M0 Spike 清单（每项 ≤1 天，全部通过才动 M1）

> 状态更新（2026-10-18，骨架已落盘 `spikes/`，workspace 全部编译通过）：
> - **S4 ✅ 已通过**：`cargo test -p spike-fts` 3/3 绿 —— FTS5 在 rusqlite bundled 里可用、bigram 中文双字词检索全命中
> - **S1/S2 ✅ 已编译**（iced 0.14 新 API 已适配：`application(boot,..)`/`on_action`/`Content::perform`/`absolute_offset()`），待人工运行验收
> - **S3 ✅ 本地全链路通过**（2026-10-18，spikes/spike-melib/imap-test 本地 dovecot 2.4）：
>   连接登录 ✅ / mailboxes ✅ / fetch 51 封且中文主题发件人未读状态解码正确 ✅ / 完整邮件 427B ✅ / **IDLE 推送 ✅**（Refresh(Create(Envelope…)) 带完整解析）
>   待真实账号补充：QQ 授权码 / 163 的 ID 命令（"Unsafe Login" 验证）/ Gmail 应用密码 + 标签去重
>   运行中发现的 melib 独立使用三坑（已写进 spike 注释）：① 操作不自动连接，必须先 `is_online()`；② AccountSettings.extra 是顶层 flatten 且值必须是字符串；③ watch 事件可能重复到达（连接池多路通知）→ 印证"事件只当信号、增量同步补差"设计

| # | 内容 | 通过标准 | 否决后果 |
|---|---|---|---|
| S1 | iced `text_editor` 中文输入（macOS 拼音） | 候选框跟随、preedit 内联、上屏正确 | 换 egui 复测 → gpui |
| S2 | iced 10 万行窗口化列表 | 滚动无明显掉帧（肉眼 + 记录帧时间） | 换 egui（show_rows 内建） |
| S3 | melib 连真实账号（QQ/163/Gmail 各拉 100 封 + IDLE 收新信） | 三家都能列表 + 163 无 "Unsafe Login" | melib 降级，启动 Plan B（async-imap 自建）|
| S4 | rusqlite bundled + FTS5 建表 + bigram 中文检索 POC | `fts5 MATCH '发票'` 命中 | 换 trigram/全量 LIKE/tantivy 评估 |
| S5（可选） | gpui hello-world + 中文输入 | 编译跑起来、IME 可用 | 仅作情报，不阻塞 |

S3 需要：三个测试账号（QQ 授权码 / 163 / Gmail 应用密码）。

## 3. 更新后的风险矩阵

| 风险 | 等级 | 缓解 |
|---|---|---|
| iced IME 不达标 | 高（一票否决） | S1 前置；fallback 链明确 |
| melib API 不稳/文档少 | 中 | 锁版本 + trait 隔离（backend 层可换 async-imap） |
| IMAP provider 怪癖 | 高但已被 melib 吸收大部分 | S3 实测三大家；per-provider 兼容表驱动测试 |
| CJK bigram 索引膨胀（~2x 文本） | 低 | 附件不入索引；索引可重建；磁盘预算内可接受 |
| bundled SQLite 无 FTS5 | 低 | S4 提前验证；备选路线明确 |
| HTML 邮件渲染丑（表格/发票类） | 中 | 兜底"浏览器打开"；接受主义者路线 |
| 单人项目范围蔓延（PGP/JMAP/规则引擎…） | 高（自伤类） | 里程碑门禁：M2 吃狗粮前不加任何 M3+ 功能 |
| **R8：melib 依赖 pre-release 链漂移**（本次实证：cargo 解析到新 alpha 后 melib 自身编译崩） | 中 | 精确钉 `imap-codec =2.0.0-alpha.4` + `imap-types =2.0.0-alpha.3`（已写入 spike-melib Cargo.toml，进 smail-backend 时必须携带）；melib 升级时同步重估钉子 |
| egui 路线 MSRV 1.95 追新 | 低（仅 fallback 时生效） | 锁 0.36.x LTS 式用法 |

## 4. 决策记录

1. **✅ 已决策（2026-10-18）：接受 GPL-3.0**（"自己用或开源"）⇒ **melib 路线锁定**，s-mail 以 GPL-3.0-or-later 发布。
2. 主用邮箱：未单独答复 ⇒ S3 spike 做成 config 驱动（`spikes/spike-melib/config.toml`），用户填哪家测哪家；QQ 需授权码、163 需 IMAP 服务开启，注意事项写在配置样例里。
