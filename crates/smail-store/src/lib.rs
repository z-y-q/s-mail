//! smail-store：SQLite 索引 + FTS5 全文搜索 + 草稿 + 发件箱。
//!
//! 纪律：原始 EML 在磁盘；索引可重建。schema 用 user_version 迁移。
//! M3 新增：FTS5 表（bigram 预切分，S4 spike 验证过的方案）、indexed 标记、
//! 后台索引器配合（core 负责批量补正文索引）。
//!
//! FTS 说明：standalone 表（非 external-content，无 content 同步舞步），
//! 存的是切分后的文本（bigram ~2x 膨胀，DESIGN 接受）；不提供 snippet（结果页
//! 只显示主题），换取更新简单可靠（DELETE+INSERT）。

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;

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
        Ok(Self { conn })
    }

    // ---------------- messages ----------------

    pub fn upsert(&self, row: &MessageRow) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO messages(envelope_hash, folder, message_id, subject, from_addr,
                                  date_unix, seen, flagged, has_body, in_reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(envelope_hash) DO UPDATE SET
                seen = excluded.seen,
                flagged = excluded.flagged",
            (
                row.envelope_hash,
                &row.folder,
                &row.message_id,
                &row.subject,
                &row.from_addr,
                row.date_unix,
                row.seen as i64,
                row.flagged as i64,
                row.has_body as i64,
                &row.in_reply_to,
            ),
        )?;
        Ok(())
    }

    /// 批量写入新邮件并建信封级 FTS（单事务提交；同步路径专用）。
    /// 旧路径逐条 upsert+index = 每封两次隐式事务提交，万封首次同步显著放大 I/O。
    pub fn upsert_new_batch(&self, rows: &[MessageRow]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO messages(envelope_hash, folder, message_id, subject, from_addr,
                                      date_unix, seen, flagged, has_body, in_reply_to)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(envelope_hash) DO UPDATE SET
                    seen = excluded.seen,
                    flagged = excluded.flagged",
            )?;
            let mut fts_del = tx.prepare("DELETE FROM messages_fts WHERE rowid = ?1")?;
            let mut fts_ins =
                tx.prepare("INSERT INTO messages_fts(rowid, subject, body) VALUES (?1, ?2, '')")?;
            for row in rows {
                upsert.execute((
                    row.envelope_hash,
                    &row.folder,
                    &row.message_id,
                    &row.subject,
                    &row.from_addr,
                    row.date_unix,
                    row.seen as i64,
                    row.flagged as i64,
                    row.has_body as i64,
                    &row.in_reply_to,
                ))?;
                fts_del.execute((row.envelope_hash,))?;
                let headers = tokenize(&format!("{} {}", row.subject, row.from_addr));
                fts_ins.execute((row.envelope_hash, headers))?;
            }
        }
        tx.commit()
    }

    /// 批量删除 messages + FTS（单事务；服务端删除同步 / UIDVALIDITY 全量收敛用）
    pub fn delete_messages_batch(&self, hashes: &[i64]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut del_msg = tx.prepare("DELETE FROM messages WHERE envelope_hash = ?1")?;
            let mut del_fts = tx.prepare("DELETE FROM messages_fts WHERE rowid = ?1")?;
            for h in hashes {
                del_msg.execute((h,))?;
                del_fts.execute((h,))?;
            }
        }
        tx.commit()
    }

    pub fn set_seen(&self, hash: i64, seen: bool) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET seen = ?2 WHERE envelope_hash = ?1",
            (hash, seen as i64),
        )?;
        Ok(())
    }

    pub fn set_flagged(&self, hash: i64, flagged: bool) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET flagged = ?2 WHERE envelope_hash = ?1",
            (hash, flagged as i64),
        )?;
        Ok(())
    }

    pub fn delete_message(&self, hash: i64) -> rusqlite::Result<()> {
        // 单事务：两条语句部分成功会留下 FTS 孤行/索引残留
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM messages WHERE envelope_hash = ?1", (hash,))?;
        tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", (hash,))?;
        tx.commit()
    }

    pub fn mark_has_body(&self, hash: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET has_body = 1 WHERE envelope_hash = ?1",
            (hash,),
        )?;
        Ok(())
    }

    /// 已知邮件的本地 flags（同步 diff 用：返回 (hash → (seen, flagged))）。
    /// 替代旧 known_hashes：服务器 flags 回传需要与本地比对，顺手也省了二次查询
    pub fn known_flags(&self, folder: &str) -> rusqlite::Result<HashMap<i64, (bool, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT envelope_hash, seen, flagged FROM messages WHERE folder = ?1")?;
        let rows = stmt.query_map([folder], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get::<_, i64>(1)? != 0, r.get::<_, i64>(2)? != 0),
            ))
        })?;
        let mut m = HashMap::with_capacity(rows.size_hint().0);
        for row in rows {
            let (h, flags) = row?;
            m.insert(h, flags);
        }
        Ok(m)
    }

    /// 批量应用 flags（单事务；服务器 flags 回传路径）
    pub fn apply_flags_batch(&self, updates: &[(i64, bool, bool)]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt =
                tx.prepare("UPDATE messages SET seen = ?2, flagged = ?3 WHERE envelope_hash = ?1")?;
            for (hash, seen, flagged) in updates {
                stmt.execute((hash, *seen as i64, *flagged as i64))?;
            }
        }
        tx.commit()
    }

    // ---------------- 离线标记回放（v4） ----------------

    /// 记录待推送服务器的标记意图（同字段后到覆盖先到；重连后回放）
    pub fn queue_flag(
        &self,
        hash: i64,
        seen: Option<bool>,
        flagged: Option<bool>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO pending_flags(envelope_hash, seen, flagged) VALUES (?1, ?2, ?3)
             ON CONFLICT(envelope_hash) DO UPDATE SET
                seen   = COALESCE(excluded.seen, seen),
                flagged = COALESCE(excluded.flagged, flagged)",
            (hash, seen.map(|b| b as i64), flagged.map(|b| b as i64)),
        )?;
        Ok(())
    }

    /// 待回放队列（不删除；成功推送后即 remove）
    pub fn pending_flags(&self) -> rusqlite::Result<Vec<PendingFlag>> {
        let mut stmt = self
            .conn
            .prepare("SELECT envelope_hash, seen, flagged FROM pending_flags")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?.map(|v| v != 0),
                r.get::<_, Option<i64>>(2)?.map(|v| v != 0),
            ))
        })?;
        rows.collect()
    }

    /// 移除已成功推送的队列项（单事务）
    pub fn remove_pending_flags(&self, hashes: &[i64]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM pending_flags WHERE envelope_hash = ?1")?;
            for h in hashes {
                stmt.execute((h,))?;
            }
        }
        tx.commit()
    }

    pub fn get(&self, hash: i64) -> rusqlite::Result<Option<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT envelope_hash, folder, message_id, subject, from_addr,
                    date_unix, seen, has_body, flagged, in_reply_to
             FROM messages WHERE envelope_hash = ?1",
        )?;
        let mut rows = stmt.query_map([hash], row_from)?;
        rows.next().transpose()
    }

    pub fn list_folder(&self, folder: &str, limit: u32) -> rusqlite::Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT envelope_hash, folder, message_id, subject, from_addr,
                    date_unix, seen, has_body, flagged, in_reply_to
             FROM messages WHERE folder = ?1
             ORDER BY date_unix DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![folder, limit], row_from)?;
        rows.collect::<Result<_, _>>()
    }

    /// 本地索引里的全部文件夹与计数（离线优先：启动即广播侧栏）
    pub fn folder_stats(&self) -> rusqlite::Result<Vec<(String, u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT folder, COUNT(*), COALESCE(SUM(seen = 0), 0)              FROM messages GROUP BY folder",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 空间回收：空闲页超过阈值才 VACUUM（大量删信后文件不缩的问题）。
    /// 返回是否执行了 VACUUM。启动早期调用，只在碎片严重时付出一次代价。
    pub fn maybe_vacuum(&self, threshold_bytes: u64) -> bool {
        let ok = (|| -> rusqlite::Result<bool> {
            let freelist: i64 = self
                .conn
                .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
            let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let free_bytes = freelist as u64 * page_size as u64;
            if free_bytes < threshold_bytes {
                return Ok(false);
            }
            self.conn.execute("VACUUM", [])?;
            Ok(true)
        })();
        ok.unwrap_or(false)
    }

    pub fn count_folder(&self, folder: &str) -> rusqlite::Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE folder = ?1",
            [folder],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    pub fn count_unseen(&self, folder: &str) -> rusqlite::Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE folder = ?1 AND seen = 0",
            [folder],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    // ---------------- M7：文件夹级操作 + 联系人 ----------------

    /// 文件夹内全部哈希（删文件夹时清理本地缓存用）
    pub fn hashes_in_folder(&self, folder: &str) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT envelope_hash FROM messages WHERE folder = ?1")?;
        let rows = stmt.query_map([folder], |r| r.get::<_, i64>(0))?;
        rows.collect::<Result<_, _>>()
    }

    /// 删除文件夹的全部本地记录（messages + FTS；服务器删除成功后调用）
    pub fn delete_folder_rows(&self, folder: &str) -> rusqlite::Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM messages_fts WHERE rowid IN \
             (SELECT envelope_hash FROM messages WHERE folder = ?1)",
            [folder],
        )?;
        self.conn
            .execute("DELETE FROM messages WHERE folder = ?1", [folder])?;
        Ok(n)
    }

    /// 文件夹改名：本地记录随服务器新路径更新（FTS 无 folder 概念，不动）
    pub fn rename_folder_rows(&self, old: &str, new: &str) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE messages SET folder = ?2 WHERE folder = ?1",
            (old, new),
        )
    }

    /// 最近联系人（ M7 自动补全）：发件地址去重，按最近收信时间排序。
    /// 地址为 melib Display 格式（"名字 <a@b>"），UI 直接展示/回填。
    pub fn recent_senders(&self, limit: usize) -> rusqlite::Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_addr, MAX(date_unix) AS last FROM messages \
             WHERE from_addr != '' GROUP BY from_addr \
             ORDER BY last DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<_, _>>()
    }

    // ---------------- 通讯录（v7） ----------------

    /// 收信自动收集：批量 upsert（单事务；名字非空才覆盖旧名，时间总更新）
    pub fn upsert_contacts_batch(&self, items: &[(String, String)]) -> rusqlite::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|x| x.as_secs() as i64)
            .unwrap_or(0);
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO contacts(email, name, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(email) DO UPDATE SET
                    name = CASE WHEN excluded.name != '' THEN excluded.name ELSE contacts.name END,
                    updated_at = excluded.updated_at",
            )?;
            for (email, name) in items {
                stmt.execute((email, name, now))?;
            }
        }
        tx.commit()
    }

    /// 补全数据源：display 格式（"名字 <a@b>" 或裸地址），按最近联系排序
    pub fn list_contacts_display(&self, limit: usize) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT email, name FROM contacts ORDER BY updated_at DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| {
            let email: String = r.get(0)?;
            let name: String = r.get(1)?;
            Ok(if name.is_empty() {
                email
            } else {
                format!("{name} <{email}>")
            })
        })?;
        rows.collect()
    }

    /// 全量列表（管理界面用）
    pub fn list_contacts_full(&self) -> rusqlite::Result<Vec<ContactRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, email, name, updated_at FROM contacts ORDER BY updated_at DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(ContactRow {
                id: r.get(0)?,
                email: r.get(1)?,
                name: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn delete_contact(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM contacts WHERE id = ?1", (id,))?;
        Ok(())
    }

    // ---------------- FTS（M3） ----------------

    /// 信封级索引（同步时调用）：主题+发件人即可搜，正文待后台索引
    pub fn index_envelope(&self, hash: i64, subject: &str, from: &str) -> rusqlite::Result<()> {
        let headers = tokenize(&format!("{subject} {from}"));
        self.conn
            .execute("DELETE FROM messages_fts WHERE rowid = ?1", (hash,))?;
        self.conn.execute(
            "INSERT INTO messages_fts(rowid, subject, body) VALUES (?1, ?2, '')",
            (hash, headers),
        )?;
        Ok(())
    }

    /// 正文索引（打开邮件或后台索引器调用）
    pub fn index_body(&self, hash: i64, body: &str) -> rusqlite::Result<()> {
        let row = self
            .get(hash)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let headers = tokenize(&format!("{} {}", row.subject, row.from_addr));
        // 单事务：DELETE+INSERT+UPDATE 三步原子化（中断不留半成品索引态）
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", (hash,))?;
        tx.execute(
            "INSERT INTO messages_fts(rowid, subject, body) VALUES (?1, ?2, ?3)",
            (hash, headers, tokenize(body)),
        )?;
        tx.execute(
            "UPDATE messages SET indexed = 1 WHERE envelope_hash = ?1",
            (hash,),
        )?;
        tx.commit()
    }

    /// 未建正文索引的邮件（后台索引器的工单）
    /// 标记已索引但不写入正文（M6 排查：加密邮件——避免密文污染 FTS 且反复拉取）
    pub fn mark_indexed(&self, hash: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET indexed = 1 WHERE envelope_hash = ?1",
            (hash,),
        )?;
        Ok(())
    }

    pub fn unindexed(&self, limit: u32) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT envelope_hash FROM messages WHERE indexed = 0
             ORDER BY date_unix DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| r.get::<_, i64>(0))?;
        rows.collect::<Result<_, _>>()
    }

    /// 全文搜索（bigram 切分后的 MATCH，JOIN 取原始行）
    /// 搜索（带正文片段）：FTS5 snippet() 返回匹配词附近的摘录
    pub fn search_with_snippets(
        &self,
        query: &str,
        limit: u32,
    ) -> rusqlite::Result<Vec<(MessageRow, String)>> {
        let tokenized = tokenize(query);
        if tokenized.trim().is_empty() {
            return Ok(vec![]);
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.envelope_hash, m.folder, m.message_id, m.subject, m.from_addr,
                    m.date_unix, m.seen, m.has_body, m.flagged, m.in_reply_to,
                    snippet(messages_fts, 1, '「', '」', '…', 30)
             FROM messages_fts f JOIN messages m ON m.envelope_hash = f.rowid
             WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![tokenized, limit], |r| {
            let row = row_from(r)?;
            let snip: String = r.get(10)?;
            Ok((row, snip))
        })?;
        rows.collect::<Result<_, _>>()
    }

    pub fn search(&self, query: &str, limit: u32) -> rusqlite::Result<Vec<MessageRow>> {
        let tokenized = tokenize(query);
        if tokenized.trim().is_empty() {
            return Ok(vec![]);
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.envelope_hash, m.folder, m.message_id, m.subject, m.from_addr,
                    m.date_unix, m.seen, m.has_body, m.flagged, m.in_reply_to
             FROM messages_fts f JOIN messages m ON m.envelope_hash = f.rowid
             WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![tokenized, limit], row_from)?;
        rows.collect::<Result<_, _>>()
    }

    // ---------------- drafts ----------------

    pub fn save_draft(&self, d: &DraftRow) -> rusqlite::Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|x| x.as_secs() as i64)
            .unwrap_or(0);
        if d.id > 0 {
            self.conn.execute(
                "UPDATE drafts SET to_addr=?2, cc=?3, bcc=?4, subject=?5, body=?6, updated_at=?7
                 WHERE id=?1",
                (&d.id, &d.to_addr, &d.cc, &d.bcc, &d.subject, &d.body, now),
            )?;
            return Ok(d.id);
        }
        self.conn.execute(
            "INSERT INTO drafts(to_addr, cc, bcc, subject, body, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6)",
            (&d.to_addr, &d.cc, &d.bcc, &d.subject, &d.body, now),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_drafts(&self) -> rusqlite::Result<Vec<DraftRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, to_addr, cc, bcc, subject, body, updated_at
             FROM drafts ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DraftRow {
                id: r.get(0)?,
                to_addr: r.get(1)?,
                cc: r.get(2)?,
                bcc: r.get(3)?,
                subject: r.get(4)?,
                body: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?;
        rows.collect::<Result<_, _>>()
    }

    pub fn delete_draft(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM drafts WHERE id = ?1", (id,))?;
        Ok(())
    }

    /// B2 草稿云同步：服务器副本的 Message-ID（None=不存在或未上传）
    pub fn draft_message_id(&self, id: i64) -> rusqlite::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT message_id FROM drafts WHERE id = ?1")?;
        let mut rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(r) => {
                let mid = r?;
                Ok(if mid.is_empty() { None } else { Some(mid) })
            }
            None => Ok(None),
        }
    }

    /// B2：记录草稿上传后的服务器 Message-ID
    pub fn set_draft_message_id(&self, id: i64, mid: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("UPDATE drafts SET message_id = ?2 WHERE id = ?1", (id, mid))?;
        Ok(())
    }

    /// 已关联服务器 Message-ID 的全部草稿（双向同步对账用；未上传的不参与删传播）
    pub fn all_draft_mids(&self) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, message_id FROM drafts WHERE message_id != ''")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect()
    }

    // ---------------- outbox ----------------

    pub fn enqueue_outbox(
        &self,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
        attachments: &[String],
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO outbox(to_addr, cc, bcc, subject, body, attachments)
             VALUES (?1,?2,?3,?4,?5,?6)",
            (to, cc, bcc, subject, body, attachments.join("\u{1}")),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn pending_outbox(&self) -> rusqlite::Result<Vec<OutboxRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, to_addr, cc, bcc, subject, body, attachments, status, attempts, error \
             FROM outbox WHERE status = 'pending' AND attempts < ?1",
        )?;
        let rows = stmt.query_map([OUTBOX_MAX_ATTEMPTS], |r| {
            let att: String = r.get(6)?;
            Ok(OutboxRow {
                id: r.get(0)?,
                to_addr: r.get(1)?,
                cc: r.get(2)?,
                bcc: r.get(3)?,
                subject: r.get(4)?,
                body: r.get(5)?,
                attachments: att
                    .split('\u{1}')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
                status: r.get(7)?,
                attempts: r.get(8)?,
                error: r.get(9)?,
            })
        })?;
        rows.collect::<Result<_, _>>()
    }

    /// 发件箱全量（含 failed——UI 发件箱视图用）
    pub fn list_outbox_all(&self) -> rusqlite::Result<Vec<OutboxRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, to_addr, cc, bcc, subject, body, attachments, status, attempts, error \
             FROM outbox ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let att: String = r.get(6)?;
                Ok(OutboxRow {
                    id: r.get(0)?,
                    to_addr: r.get(1)?,
                    cc: r.get(2)?,
                    bcc: r.get(3)?,
                    subject: r.get(4)?,
                    body: r.get(5)?,
                    attachments: att.split('\u{1}').map(|s| s.to_string()).collect(),
                    status: r.get(7)?,
                    attempts: r.get(8)?,
                    error: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 重发：状态回 pending、清计数与错误（下轮 flush 或重连后发送）
    pub fn outbox_reset(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE outbox SET status = 'pending', attempts = 0, error = '' WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// 清理 sent 状态的旧 outbox 行（保留最近 100 条；防磁盘慢泄漏）
    pub fn outbox_clean_sent(&self) {
        let _ = self.conn.execute(
            "DELETE FROM outbox WHERE status = 'sent' AND id NOT IN (
             SELECT id FROM outbox WHERE status = 'sent' ORDER BY id DESC LIMIT 100
             )",
            [],
        );
    }

    pub fn outbox_remove(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        Ok(())
    }

    /// (pending, failed) 计数（侧栏入口显隐）
    pub fn outbox_counts(&self) -> rusqlite::Result<(usize, usize)> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM outbox WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let f: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM outbox WHERE status = 'failed'",
            [],
            |r| r.get(0),
        )?;
        Ok((n as usize, f as usize))
    }

    pub fn outbox_mark(&self, id: i64, status: &str, error: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE outbox SET status=?2, error=?3, attempts = attempts + 1 WHERE id=?1",
            (id, status, error),
        )?;
        Ok(())
    }

    /// 重试穷尽：pending 且 attempts ≥ 上限 → failed（防永久失败无限 6s 重试）
    pub fn fail_exhausted_outbox(&self) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE outbox SET status='failed', error='重试次数用尽（20 次）' \
             WHERE status='pending' AND attempts >= ?1",
            [OUTBOX_MAX_ATTEMPTS],
        )
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // 收尾：optimize 更新查询统计，TRUNCATE checkpoint 回收 -wal 空间
        let _ = self
            .conn
            .execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);");
    }
}

/// 发件重试上限（每 6s 一轮 → 约 2 分钟后奔 failed，不无限打服务器）
const OUTBOX_MAX_ATTEMPTS: i64 = 20;

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        envelope_hash: r.get(0)?,
        folder: r.get(1)?,
        message_id: r.get(2)?,
        subject: r.get(3)?,
        from_addr: r.get(4)?,
        date_unix: r.get(5)?,
        seen: r.get::<_, i64>(6)? != 0,
        has_body: r.get::<_, i64>(7)? != 0,
        flagged: r.get::<_, i64>(8)? != 0,
        in_reply_to: r.get::<_, String>(9)?,
    })
}

// ---------------------------------------------------------------------------
// bigram 切分器（S4 spike 验证；索引与查询两侧共用）
// ---------------------------------------------------------------------------

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

pub fn tokenize(input: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut cjk = String::new();

    fn push_cjk(cjk: &mut String, tokens: &mut Vec<String>) {
        let chars: Vec<char> = cjk.chars().collect();
        match chars.len() {
            0 => {}
            1 => tokens.push(chars[0].to_string()),
            _ => {
                for w in chars.windows(2) {
                    tokens.push(w.iter().collect());
                }
            }
        }
        cjk.clear();
    }

    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            if !cjk.is_empty() {
                push_cjk(&mut cjk, &mut tokens);
            }
            word.push(c);
        } else if is_cjk(c) {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            cjk.push(c);
        } else {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            push_cjk(&mut cjk, &mut tokens);
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    push_cjk(&mut cjk, &mut tokens);
    tokens.join(" ")
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
mod ai_cache_tests {
    use super::*;

    #[test]
    fn summary_round_trip_and_prune() {
        let dir = std::env::temp_dir().join(format!("smail-aicache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = AiCache::open(&dir.join("ai.db")).unwrap();
        assert_eq!(c.get_summary(0, 42), None);
        c.put_summary(0, 42, "摘要A");
        c.put_summary(0, 42, "摘要B"); // 覆盖
        assert_eq!(c.get_summary(0, 42).as_deref(), Some("摘要B"));
        c.put_summary(0, 43, "x");
        c.put_summary(0, 44, "y");
        c.prune(2);
        let n: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM summaries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
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
}

// ---------------------------------------------------------------------------
// AI 摘要缓存（独立 ai.db；与账号库分离——UI 层持有，服务线程不碰）
// ---------------------------------------------------------------------------

pub struct AiCache {
    conn: rusqlite::Connection,
}

impl AiCache {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS summaries(
                acct       INTEGER NOT NULL,
                hash       INTEGER NOT NULL,
                text       TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (acct, hash)
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn get_summary(&self, acct: usize, hash: i64) -> Option<String> {
        self.conn
            .query_row(
                "SELECT text FROM summaries WHERE acct = ?1 AND hash = ?2",
                [acct as i64, hash],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    pub fn put_summary(&self, acct: usize, hash: i64, text: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let _ = self.conn.execute(
            "INSERT INTO summaries(acct, hash, text, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(acct, hash) DO UPDATE SET text = ?3, updated_at = ?4",
            rusqlite::params![acct as i64, hash, text, now],
        );
    }

    /// 只保留最近 keep 条（防无限膨胀；摘要可再生，淘汰无损）
    pub fn prune(&self, keep: usize) {
        let _ = self.conn.execute(
            "DELETE FROM summaries WHERE (acct, hash, updated_at) NOT IN (
                SELECT acct, hash, updated_at FROM summaries
                ORDER BY updated_at DESC LIMIT ?1
            )",
            [keep as i64],
        );
    }
}
