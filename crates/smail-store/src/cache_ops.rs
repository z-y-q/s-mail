//! AI 摘要缓存
//!
//! ## 职责
//!
//! - **独立存储**：单独 SQLite 文件（与主索引分离）
//! - **摘要读写**：get_summary/put_summary（按账号+哈希键索引）
//! - **空间控制**：prune(keep) 只保留最近 N 条（摘要可再生，淘汰无损）

use rusqlite::Connection;
use std::path::Path;

pub struct AiCache {
    conn: Connection,
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
