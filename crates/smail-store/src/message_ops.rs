//! 邮件消息 CRUD 操作
//!
//! ## 职责
//!
//! - **消息增删改**：upsert、delete、批量操作
//! - **标记管理**：seen/flagged 状态更新
//! - **离线队列**：断网时标记回放（pending_flags 表）
//! - **正文状态**：has_body 标记（正文已拉取）
//!
//! ## 事务策略
//!
//! - 批量操作单事务提交（upsert_new_batch/delete_messages_batch/apply_flags_batch）
//! - 删除消息必须同时清理 FTS 索引，防止孤行

use super::{tokenize, MessageRow, PendingFlag, Store};
use std::collections::HashMap;

impl Store {
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

    /// 批量写入新邮件并建信封级 FTS（分批事务；同步路径专用）。
    /// 旧路径逐条 upsert+index = 每封两次隐式事务提交，万封首次同步显著放大 I/O。
    /// 新路径分批提交：10000 封若一次性提交可能锁库 5 秒，分批后每批 <0.5s。
    pub fn upsert_new_batch(&self, rows: &[MessageRow]) -> rusqlite::Result<()> {
        const CHUNK_SIZE: usize = 500;
        for chunk in rows.chunks(CHUNK_SIZE) {
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
                let mut fts_ins = tx.prepare(
                    "INSERT INTO messages_fts(rowid, subject, body) VALUES (?1, ?2, '')",
                )?;
                for row in chunk {
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
            tx.commit()?;
        }
        Ok(())
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
}
