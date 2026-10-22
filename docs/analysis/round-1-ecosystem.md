# 第一轮：生态事实核查（crates.io API，2026-10-18 拉取）

所有数据来自 crates.io API 实测，非记忆推断。

## 结论速览

| crate | 最新版 | 更新时间 | License | 状态 | 对 s-mail 的影响 |
|---|---|---|---|---|---|
| **melib** | 0.8.13 | 2026-01 | **EUPL-1.2 OR GPL-3.0+** | 活跃（epilys） | ⚠️ copyleft 传染，见 §1 |
| async-imap | 0.12.0 | 2026-09 | MIT/Apache | 活跃（Delta Chat 维护者） | 自建路线的首选 |
| imap (jonhoo) | 3.0.0-alpha.15 | 2025-02 后停滞 | MIT/Apache | 休眠 | ❌ 排除 |
| himalaya-lib | — | 已废弃 | — | 全版本 yank | ❌ 排除 |
| iced | 0.14.0 | 2025-12 | MIT | 活跃 | UI 主线候选 |
| egui | 0.37.0 | 2026-10 | MIT/Apache | 极活跃 | 备选；MSRV 1.95 激进 |
| gpui | 0.2.2 | 2025-10 | Apache-2.0 | Zed 官方发布 | 🐴 dark horse |
| rusqlite | 0.40.2 | 2026-08 | MIT | 极活跃 | 存储层确定 |
| ammonia | 4.2.4 | 2026-10 | MIT/Apache | 活跃 | HTML 消毒确定 |
| sqlite-fts5 | — | — | — | **不存在（404）** | 原方案作废 → 第二轮重设计 |

## 1. melib：license 是唯一的硬决策点

- `EUPL-1.2 OR GPL-3.0-or-later` 双许可，**两个都是 copyleft**（EUPL 甚至把网络使用算作分发）。静态链接进 s-mail ⇒ s-mail 必须开源且用兼容许可（GPL-3.0 即可）。
- 个人使用 / 本来就打算开源 ⇒ **零成本**，melib 依然是性价比最高的路线（IMAP/SMTP/JMAP/maildir/MIME 全有，0.8.13 持续维护）。
- 想保留闭源可能性 ⇒ 只能走自建路线。
- 其他事实：TLS 用 native-tls（非 rustls）；features 里确有 `jmap`；文档字段为 null（API 文档缺乏，学习靠源码和 meli 源码）。

## 2. 自建路线的底牌换人：async-imap

- 0.12.0（2026-09-30），维护者 link2xt（Delta Chat 团队），近 90 天下载 ~96 万，MIT/Apache。
- 只有 ~5k 行，纯 IMAP 客户端，不含同步逻辑 —— 等于只给你协议，UIDVALIDITY/QRESYNC/状态机全自己写。
- 坑：**default runtime 是 async-std**，必须 `default-features = false, features = ["runtime-tokio"]`。
- 搭配：`lettre`（SMTP）+ `mail-parser`/`mail-builder` + `rustls`。

## 3. iced vs egui vs gpui（以事实为准）

| 维度 | iced 0.14 | egui 0.37 | gpui 0.2.2 |
|---|---|---|---|
| License | MIT | MIT/Apache | Apache-2.0 |
| MSRV | 1.88 | **1.95**（追新工具链） | 未声明（edition 2024） |
| 渲染 | wgpu + tiny-skia | epaint/GPU | Metal(mac)/blade |
| 大文本列表 | 无内建虚拟化，需手动窗口化 | `ScrollArea::show_rows` 内建 | UniformList 内建（Zed 验证过百万行） |
| CJK 输入法 | ⚠️ 历史弱项，需 spike | ⚠️ 需 spike | ✅ Zed 中文输入实际可用 |
| markdown | ✅ 官方 feature | 无 | 无（自己排） |
| 文档/生态 | 书 + 例子多 | 最好 | 几乎没有文档 |
| 平台 | 全平台 | 全平台 | mac 成熟/Linux 可用/Windows 进行中 |

- 结论：**iced 0.14 仍是主线假设，但把 IME 列为一票否决项**（见第二轮 §4）。
- gpui 从"不存在"升级为"存在但险"：依赖拉 Zed 全家桶、API 无稳定性承诺、文档缺失；但 IME 和大列表是它唯一两项"开箱即胜"的能力。

## 4. rusqlite / FTS5 事实

- 0.40.2 活跃；`bundled` feature 会启用 `modern_sqlite`（bundled_bindings）。
- **features 列表里没有任何 fts5 开关** —— bundled 构建是否编译进 FTS5 取决于 libsqlite3-sys 构建脚本，文档未承诺。
- 计划中的 `sqlite-fts5` crate（注册自定义 FTS5 tokenizer）**不存在**（API 404）。
- ⇒ 搜索方案改为"应用层 bigram 预切分 + 标准 unicode61"，完全不依赖自定义 tokenizer（第二轮 §3），FTS5 可用性本身放进 M0 集成测试验证。

## 5. ammonia

4.2.4（2026-10-17 刚更新），html5ever 系白名单消毒器，MIT/Apache。HTML 邮件降级管线的第一棒确定。
