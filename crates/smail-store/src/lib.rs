//! 邮件本地存储层
//!
//! ## 职责
//!
//! - SQLite 索引：信封元数据（主题、发件人、时间、标记状态）
//! - FTS5 全文搜索：bigram 分词 + 中英文混合查询
//! - 草稿管理：临时保存 + 自动清理
//! - 发件箱队列：离线排队 + 重试机制
//! - 通讯录：发件人自动提取 + 频率统计
//!
//! ## 架构原则
//!
//! **纪律**：原始 EML 文件在磁盘（`bodies/{hash}.eml`），索引可重建
//!
//! ```text
//! 磁盘
//!   └─ data_dir/
//!       ├─ index.db (SQLite)
//!       │   ├─ messages (信封索引)
//!       │   ├─ messages_fts (FTS5 全文)
//!       │   ├─ drafts (草稿)
//!       │   ├─ outbox (发件队列)
//!       │   └─ contacts (通讯录)
//!       └─ bodies/
//!           └─ {hash}.eml (原始邮件)
//! ```
//!
//! ## FTS5 设计
//!
//! - **独立表**（standalone）：不使用 external-content 模式，避免同步复杂性
//! - **bigram 预切分**：存储时已分词，查询时直接匹配
//! - **空间权衡**：文本膨胀约 2x，换取更新简单可靠（DELETE + INSERT）
//! - **snippet 摘录**：`search_with_snippets()` 用 FTS5 snippet() 返回
//!   匹配词附近摘录（「」高亮），搜索结果行直接展示命中上下文
//!
//! ## 并发模型
//!
//! - **WAL 模式**：支持多线程并发读写（Reader 线程 + 服务主循环）
//! - **busy_timeout**：遇锁自动重试 5 秒，避免 SQLITE_BUSY 错误
//!
//! ## Schema 迁移
//!
//! 使用 `user_version` pragma 管理版本：
//! - 当前版本检测 → 应用缺失的迁移 → 更新 user_version

mod cache_ops;
mod contact_ops;
mod draft_ops;
mod error;
mod folder_ops;
mod helpers;
mod message_ops;
mod outbox_ops;
mod query_ops;
mod search_ops;

#[cfg(test)]
mod migration_tests;

pub use cache_ops::AiCache;
pub use error::{Result, StoreError};
pub use query_ops::{FolderProbe, VacuumStats};
pub use search_ops::preview_from_body;

use helpers::{row_from, tokenize};
use rusqlite::Connection;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub envelope_hash: i64,
    pub folder: String,
    pub message_id: String,
    pub subject: String,
    pub from_addr: String,
    pub date_unix: i64,
    pub seen: bool,
    pub flagged: bool,
    pub has_body: bool,
    /// M5：会话聚合（"<id>" 形式，空=无/根）
    pub in_reply_to: String,
    /// 列表行正文预览（纯文本前 ~160 字符；正文索引时生成，新邮件为空）
    pub preview: String,
}

#[derive(Debug, Clone)]
pub struct DraftRow {
    pub id: i64,
    pub to_addr: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub id: i64,
    pub to_addr: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<String>,
    pub status: String,
    pub attempts: i32,
    pub error: String,
}

/// 待回放的标记意图：hash +（seen 意图, flagged 意图），None=该字段不动
pub type PendingFlag = (i64, Option<bool>, Option<bool>);

/// 通讯录条目（收信自动收集 + 手动管理）
#[derive(Debug, Clone)]
pub struct ContactRow {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub updated_at: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // busy_timeout 必须最先设置：journal_mode 变更需写锁，
        // 并发首开（服务主循环 + Reader 线程）时无超时会直接 database is locked
        conn.pragma_update(None, "busy_timeout", 5000)?;
        // 并发首开兑底：journal_mode=WAL 变更撞上另一连接的建表事务时，
        // SQLite 可能不经过 busy handler 直接返回 BUSY（文档行为）。整个
        // 初始化序列幂等（IF NOT EXISTS / ALTER 已存在则忽略），撞 BUSY 整段重试
        let mut attempt = 0u32;
        loop {
            match Self::init(&conn) {
                Ok(()) => return Ok(Self { conn }),
                Err(e) if attempt < 20 && is_busy(&e) => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 只读连接（UI 切文件夹时直读本地索引，不等服务线程从长同步里回头）。
    /// 不建表不迁移：库不存在即报错——调用方据此判定「无缓存」。
    /// busy_timeout 照设：服务线程写事务期间 UI 读等锁而不是报错；
    /// 写方法在只读连接上直接 SQLITE_READONLY，快速失败不静默
    pub fn open_readonly(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(Self { conn })
    }

    /// 初始化序列（WAL + 建表 + v1~v7 迁移）：全部幂等，可重入
    fn init(conn: &Connection) -> rusqlite::Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // WAL 配套：NORMAL 足够（索引可重建，掉电最多丢最后事务不失一致性）
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages(
                envelope_hash INTEGER PRIMARY KEY,
                folder        TEXT NOT NULL,
                message_id    TEXT,
                subject       TEXT,
                from_addr     TEXT,
                date_unix     INTEGER,
                seen          INTEGER NOT NULL DEFAULT 0,
                has_body      INTEGER NOT NULL DEFAULT 0,
                in_reply_to   TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_messages_folder_date
                ON messages(folder, date_unix DESC);
            CREATE TABLE IF NOT EXISTS drafts(
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                to_addr    TEXT NOT NULL DEFAULT '',
                cc         TEXT NOT NULL DEFAULT '',
                subject    TEXT NOT NULL DEFAULT '',
                body       TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS outbox(
                id      INTEGER PRIMARY KEY AUTOINCREMENT,
                to_addr TEXT NOT NULL,
                cc      TEXT NOT NULL DEFAULT '',
                subject TEXT NOT NULL DEFAULT '',
                body    TEXT NOT NULL,
                status  TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0,
                error   TEXT NOT NULL DEFAULT ''
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                subject, body, tokenize='unicode61'
            );",
        )?;
        // v1：M1 库补 flagged；v2：补 indexed + outbox.attachments；v3：会话聚合 In-Reply-To
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 3 {
            if v < 1 {
                let _ = conn.execute(
                    "ALTER TABLE messages ADD COLUMN flagged INTEGER NOT NULL DEFAULT 0",
                    [],
                );
            }
            if v < 2 {
                let _ = conn.execute(
                    "ALTER TABLE messages ADD COLUMN indexed INTEGER NOT NULL DEFAULT 0",
                    [],
                );
                let _ = conn.execute(
                    "ALTER TABLE outbox ADD COLUMN attachments TEXT NOT NULL DEFAULT ''",
                    [],
                );
            }
            if v < 3 {
                let _ = conn.execute(
                    "ALTER TABLE messages ADD COLUMN in_reply_to TEXT NOT NULL DEFAULT ''",
                    [],
                );
            }
            conn.pragma_update(None, "user_version", 3)?;
        }
        // v4：离线标记回放队列（断网时本地改的已读/星标，重连后推回服务器）
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 4 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_flags(
                    envelope_hash INTEGER PRIMARY KEY,
                    seen         INTEGER,   -- NULL=不动
                    flagged      INTEGER    -- NULL=不动
                );",
            )?;
            conn.pragma_update(None, "user_version", 4)?;
        }
        // v5：草稿云同步——服务器副本的 Message-ID（空串=未上传/仅本地）
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 5 {
            let _ = conn.execute(
                "ALTER TABLE drafts ADD COLUMN message_id TEXT NOT NULL DEFAULT ''",
                [],
            );
            conn.pragma_update(None, "user_version", 5)?;
        }
        // v6：密送（outbox 重试与草稿都要携带）
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 6 {
            let _ = conn.execute(
                "ALTER TABLE outbox ADD COLUMN bcc TEXT NOT NULL DEFAULT ''",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE drafts ADD COLUMN bcc TEXT NOT NULL DEFAULT ''",
                [],
            );
            conn.pragma_update(None, "user_version", 6)?;
        }
        // v7：通讯录（收信自动收集 + 手动管理）
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 7 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS contacts(
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    email      TEXT NOT NULL UNIQUE,
                    name       TEXT NOT NULL DEFAULT '',
                    updated_at INTEGER NOT NULL DEFAULT 0
                );",
            )?;
            // 首次建表：从现有发件人回填（recent_senders 同源）
            conn.execute_batch(
                "INSERT OR IGNORE INTO contacts(email, name, updated_at)
                 SELECT from_addr, '', MAX(date_unix) FROM messages
                 WHERE from_addr != '' AND from_addr LIKE '%<%'
                 GROUP BY from_addr;",
            )?;
            conn.pragma_update(None, "user_version", 7)?;
        }
        // v8：文件夹同步探测记录（SELECT 的 EXISTS/UIDNEXT/UIDVALIDITY
        // 上次全量值）。三值与服务器当前一致 ⇒ 集合未变（UIDVALIDITY 防
        // 文件夹重建后数值巧合），重拉可短路（省整包 FETCH）
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 8 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS folder_probe(
                    folder      TEXT PRIMARY KEY,
                    uidnext     INTEGER NOT NULL,
                    exists_n    INTEGER NOT NULL,
                    uidvalidity INTEGER NOT NULL,
                    synced_at   INTEGER NOT NULL
                );",
            )?;
            conn.pragma_update(None, "user_version", 8)?;
        }
        // v9：列表行正文预览（正文索引时生成；新邮件默认空，索引后回填）
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 9 {
            let _ = conn.execute(
                "ALTER TABLE messages ADD COLUMN preview TEXT NOT NULL DEFAULT ''",
                [],
            );
            conn.pragma_update(None, "user_version", 9)?;
        }
        Ok(())
    }
}

/// SQLITE_BUSY 判定（journal_mode 变更撞锁时不走 busy handler 的立即返回）
fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                ..
            },
            _,
        )
    )
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.conn.pragma_update(None, "optimize", 0);
    }
}

#[cfg(test)]
mod concurrent_open_tests {
    use super::*;

    /// 回归：服务主循环 + Reader 线程并发首开同一新库。journal_mode=WAL
    /// 变更撞上另一连接的建表事务时 SQLite 可立即返回 BUSY（不走 busy
    /// handler），曾导致服务线程启动即死（e2e 偶发「打开索引失败」）。
    /// 修复：Store::open 内整段重试。多线程齐发首开全部必须成功。
    #[test]
    fn concurrent_first_open_all_succeed() {
        let dir = std::env::temp_dir().join(format!("smail-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v.db");
        let n = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let db = db.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&db).map(|_| ())
                })
            })
            .collect();
        let fails: Vec<String> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap().err())
            .map(|e| e.to_string())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(fails.is_empty(), "并发首开有失败：{fails:?}");
    }
}

#[cfg(test)]
mod vacuum_tests {
    use super::*;

    #[test]
    fn maybe_vacuum_threshold_respected() {
        let dir = std::env::temp_dir().join(format!("smail-vac-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("v.db")).unwrap();
        // 空库无碎片：高阈值不触发
        assert!(!s.maybe_vacuum(1 << 30));
        // 低阈值必触发（页大小 > 0 即空闲可能为 0——freelist 0*page < 1 也可能不触发，
        // 换 0 阈值强制：0 空闲 < 0 false → 不触发；用插入删除制造碎片后触发）
        let mut row = MessageRow {
            envelope_hash: 1,
            folder: "INBOX".into(),
            message_id: "a".into(),
            subject: "s".into(),
            from_addr: "f".into(),
            date_unix: 1,
            seen: false,
            flagged: false,
            has_body: false,
            in_reply_to: String::new(),
            preview: String::new(),
        };
        for i in 0..500 {
            row.envelope_hash = i;
            s.upsert(&row).unwrap();
        }
        let hashes: Vec<i64> = (0..500).collect();
        s.delete_messages_batch(&hashes).unwrap();
        assert!(s.maybe_vacuum(1), "删除 500 行后应触发 VACUUM");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(tag: &str) -> Store {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "smail-store-{}-{tag}-{}.db",
            std::process::id(),
            unique()
        ));
        let _ = std::fs::remove_file(&dir);
        Store::open(&dir).unwrap()
    }

    fn unique() -> u32 {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    #[test]
    fn outbox_retry_limit() {
        let s = mem("outbox-limit");
        let oid = s.enqueue_outbox("x@y.z", "", "", "t", "b", &[]).unwrap();
        // 穷尽 20 次重试后：不再进入 pending 集，状态转 failed
        for _ in 0..20 {
            s.outbox_mark(oid, "pending", "temporary").unwrap();
        }
        assert!(s.pending_outbox().unwrap().is_empty(), "达到上限后不再重试");
        assert_eq!(s.fail_exhausted_outbox().unwrap(), 1);
        let status: String = s
            .conn
            .query_row("SELECT status FROM outbox WHERE id=?1", [oid], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "failed");
        // 未超限的 pending 不受影响
        let oid2 = s.enqueue_outbox("y@y.z", "", "", "t2", "b", &[]).unwrap();
        s.outbox_mark(oid2, "pending", "e").unwrap();
        assert_eq!(s.fail_exhausted_outbox().unwrap(), 0);
        assert_eq!(s.pending_outbox().unwrap().len(), 1);
    }

    fn row(hash: i64, folder: &str, subject: &str) -> MessageRow {
        MessageRow {
            envelope_hash: hash,
            folder: folder.into(),
            message_id: format!("<{hash}@x>"),
            subject: subject.into(),
            from_addr: "张三 <zhangsan@example.com>".into(),
            date_unix: 1_700_000_000 + hash,
            seen: false,
            flagged: false,
            has_body: false,
            in_reply_to: String::new(),
            preview: String::new(),
        }
    }

    #[test]
    fn tokenization_basics() {
        assert_eq!(tokenize("发票报销"), "发票 票报 报销");
        assert_eq!(tokenize("Invoice 发票"), "Invoice 发票");
    }

    #[test]
    fn fts_search_chinese() {
        let s = mem("fts");
        s.upsert(&row(1, "INBOX", "五月账单")).unwrap();
        s.index_envelope(1, "五月账单", "张三").unwrap();
        s.upsert(&row(2, "INBOX", "面试通知")).unwrap();
        s.index_envelope(2, "面试通知", "李四").unwrap();

        // 信封级：主题可搜
        assert_eq!(s.search("账单", 10).unwrap().len(), 1);
        assert_eq!(s.search("面试", 10).unwrap().len(), 1);
        assert_eq!(s.search("不存在", 10).unwrap().len(), 0);

        // 正文索引后：正文词可搜（含负数 rowid）
        let hash: i64 = -42;
        s.upsert(&MessageRow {
            envelope_hash: hash,
            ..row(2, "INBOX", "旧信")
        })
        .unwrap();
        s.index_envelope(hash, "旧信", "王五").unwrap();
        assert_eq!(s.search("报销", 10).unwrap().len(), 0, "正文未索引前搜不到");
        s.index_body(hash, "请报销差旅费用").unwrap();
        assert_eq!(s.search("报销", 10).unwrap().len(), 1, "正文索引后可搜");

        // unindexed 工单
        let un = s.unindexed(10).unwrap();
        assert!(un.contains(&1) && !un.contains(&hash));
    }

    #[test]
    fn batch_upsert_and_delete() {
        let s = mem("batch");
        let rows: Vec<MessageRow> = (1..=50)
            .map(|i| row(i, "INBOX", &format!("批量邮件 {i}")))
            .collect();
        s.upsert_new_batch(&rows).unwrap();
        assert_eq!(s.count_folder("INBOX").unwrap(), 50);
        // 信封级 FTS 随事务批量写入：主题即搜
        assert_eq!(s.search("批量邮件", 100).unwrap().len(), 50);

        // 服务端删除同步路径：批量清理 messages + FTS
        let vanish: Vec<i64> = (11..=50).collect();
        s.delete_messages_batch(&vanish).unwrap();
        assert_eq!(s.count_folder("INBOX").unwrap(), 10);
        assert_eq!(s.search("批量邮件", 100).unwrap().len(), 10);
    }

    #[test]
    fn flags_roundtrip_and_pending_replay() {
        let s = mem("flags");
        let rows: Vec<MessageRow> = (1..=3).map(|i| row(i, "INBOX", &format!("f{i}"))).collect();
        s.upsert_new_batch(&rows).unwrap();

        // known_flags：本地 flags 快照
        let known = s.known_flags("INBOX").unwrap();
        assert_eq!(known.len(), 3);
        assert_eq!(known.get(&1), Some(&(false, false)));

        // 服务器 flags 回传：差异批量应用
        s.apply_flags_batch(&[(1, true, false), (2, false, true)])
            .unwrap();
        let known = s.known_flags("INBOX").unwrap();
        assert_eq!(known.get(&1), Some(&(true, false)));
        assert_eq!(known.get(&2), Some(&(false, true)));
        assert_eq!(known.get(&3), Some(&(false, false)), "未提项不动");

        // 离线队列：后到覆盖同字段、不同字段合并；回放后清理
        s.queue_flag(1, Some(true), None).unwrap(); // seen
        s.queue_flag(1, None, Some(true)).unwrap(); // 合并 flagged
        s.queue_flag(2, Some(false), None).unwrap(); // 覆盖 seen
        s.queue_flag(99, Some(true), None).unwrap(); // 无对应行（作废项）
        let pending = s.pending_flags().unwrap();
        assert_eq!(pending.len(), 3);
        assert!(pending.contains(&(1, Some(true), Some(true))));
        assert!(pending.contains(&(2, Some(false), None)));
        s.remove_pending_flags(&[1, 2, 99]).unwrap();
        assert!(s.pending_flags().unwrap().is_empty());
    }

    #[test]
    fn drafts_and_outbox_with_attachments() {
        let s = mem("outbox");
        let oid = s
            .enqueue_outbox(
                "x@y.z",
                "",
                "",
                "带附件",
                "正文",
                &["/tmp/a.pdf".into(), "/tmp/b.zip".into()],
            )
            .unwrap();
        let rows = s.pending_outbox().unwrap();
        assert_eq!(rows[0].attachments.len(), 2);
        s.outbox_mark(oid, "sent", "").unwrap();
        assert!(s.pending_outbox().unwrap().is_empty());
    }

    #[test]
    fn folder_rows_delete_and_rename() {
        let s = mem("folder-ops");
        s.upsert(&row(1, "Archive", "旧信一")).unwrap();
        s.upsert(&row(2, "Archive", "旧信二")).unwrap();
        s.upsert(&row(3, "INBOX", "新信")).unwrap();
        s.index_envelope(1, "旧信一", "张三").unwrap();
        s.index_envelope(2, "旧信二", "张三").unwrap();

        // 改名：记录迁移、FTS 保留可搜
        assert_eq!(s.rename_folder_rows("Archive", "存档").unwrap(), 2);
        assert_eq!(s.count_folder("Archive").unwrap(), 0);
        assert_eq!(s.count_folder("存档").unwrap(), 2);
        assert_eq!(s.search("旧信", 10).unwrap().len(), 2, "改名后 FTS 仍可搜");

        // 删除：messages 与 FTS 一并清理
        assert_eq!(s.hashes_in_folder("存档").unwrap().len(), 2);
        s.delete_folder_rows("存档").unwrap();
        assert_eq!(s.count_folder("存档").unwrap(), 0);
        assert_eq!(
            s.search("旧信", 10).unwrap().len(),
            0,
            "删文件夹后 FTS 同步清理"
        );
        assert_eq!(s.count_folder("INBOX").unwrap(), 1, "其他文件夹不受影响");
    }

    #[test]
    fn recent_senders_dedup_and_order() {
        let s = mem("contacts");
        let mut r1 = row(1, "INBOX", "信一");
        r1.from_addr = "张三 <zhang@x.com>".into();
        r1.date_unix = 100;
        let mut r2 = row(2, "INBOX", "信二");
        r2.from_addr = "李四 <li@x.com>".into();
        r2.date_unix = 300;
        let mut r3 = row(3, "INBOX", "信三");
        r3.from_addr = "张三 <zhang@x.com>".into(); // 同发件人更早的信
        r3.date_unix = 50;
        let mut r4 = row(4, "INBOX", "空发件人");
        r4.from_addr = String::new();
        for r in [&r1, &r2, &r3, &r4] {
            s.upsert(r).unwrap();
        }

        let got = s.recent_senders(10).unwrap();
        assert_eq!(got.len(), 2, "去重且空发件人排除：{got:?}");
        assert_eq!(got[0].0, "李四 <li@x.com>", "按最近收信时间降序");
        assert_eq!(got[0].1, 300);
        assert_eq!(got[1].0, "张三 <zhang@x.com>");
        assert_eq!(got[1].1, 100, "同发件人取最近一封的时间");
        assert_eq!(s.recent_senders(1).unwrap().len(), 1, "limit 生效");
    }

    /// 通讯录写入归一化：大小写/空白变体的同一地址只留一行（UNIQUE(email) 收敛）
    #[test]
    fn contacts_upsert_normalizes_email_case() {
        let s = mem("contacts-norm");
        s.upsert_contacts_batch(&[
            ("announce@m.parallels.com".into(), "Parallels".into()),
            ("  ANNOUNCE@m.parallels.com ".into(), "Parallels".into()),
            ("1299675389@qq.com".into(), "我".into()),
            ("1299675389@QQ.com".into(), "我".into()),
        ])
        .unwrap();
        let full = s.list_contacts_full().unwrap();
        assert_eq!(full.len(), 2, "变体合并为一行：{full:?}");
        let emails: Vec<&str> = full.iter().map(|c| c.email.as_str()).collect();
        assert!(emails.contains(&"announce@m.parallels.com"), "{emails:?}");
        assert!(emails.contains(&"1299675389@qq.com"), "{emails:?}");
        // 名字非空才覆盖：后到的同名变体不把已有名字洗掉
        assert!(full.iter().all(|c| !c.name.is_empty()), "{full:?}");
    }

    /// 同步探测记录：写入/读取/覆盖往返一致（同步短路的依据数据）
    #[test]
    fn folder_probe_roundtrip() {
        let s = mem("probe-rt");
        assert!(s.get_folder_probe("INBOX").unwrap().is_none());
        s.set_folder_probe(
            "INBOX",
            &crate::FolderProbe {
                uidnext: 42,
                exists_n: 7,
                uidvalidity: 12345,
                synced_at: 1700000000,
            },
        )
        .unwrap();
        let p = s.get_folder_probe("INBOX").unwrap().unwrap();
        assert_eq!(
            (p.uidnext, p.exists_n, p.uidvalidity, p.synced_at),
            (42, 7, 12345, 1700000000)
        );
        // 覆盖写：同文件夹更新为新快照
        s.set_folder_probe(
            "INBOX",
            &crate::FolderProbe {
                uidnext: 43,
                exists_n: 8,
                uidvalidity: 12345,
                synced_at: 1700000100,
            },
        )
        .unwrap();
        let p = s.get_folder_probe("INBOX").unwrap().unwrap();
        assert_eq!((p.uidnext, p.exists_n), (43, 8));
        assert_eq!(s.get_folder_probe("Archive").unwrap(), None, "互不串扰");
        s.delete_folder_probe("INBOX").unwrap();
        assert!(s.get_folder_probe("INBOX").unwrap().is_none());
    }
}
