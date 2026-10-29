//! 邮件查询操作
//!
//! ## 职责
//!
//! - **单邮件查询**：get(hash)
//! - **文件夹列表**：list_folder + folder_stats
//! - **统计查询**：count_folder/count_unseen
//! - **同步探测记录**：folder_probe 的读写（SELECT EXISTS/UIDNEXT 短路）
//! - **空间回收**：maybe_vacuum（大量删信后碎片整理）、vacuum_stats（UI 显示）

use super::{row_from, MessageRow, Store};

/// VACUUM 空间统计（UI 显示：索引占用 45MB，可回收 12MB）
#[derive(Debug, Clone)]
pub struct VacuumStats {
    /// 数据库文件总大小（字节）
    pub total_bytes: u64,
    /// 实际使用空间（字节）
    pub used_bytes: u64,
    /// 可回收空闲空间（字节）
    pub free_bytes: u64,
    /// 是否建议执行 VACUUM（空闲 >10MB）
    pub should_vacuum: bool,
}

/// 文件夹同步探测记录：上次全量同步后的 SELECT EXISTS/UIDNEXT/UIDVALIDITY 快照。
/// 三值与服务器当前值一致 ⇒ 集合未变，可短路重拉（UIDVALIDITY 防文件夹
/// 重建后数值巧合）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FolderProbe {
    pub uidnext: i64,
    pub exists_n: i64,
    pub uidvalidity: i64,
    /// 上次全量同步完成的 unix 秒（低频校准：超过阈值强制全量重拉一次，
    /// 兜住 EXISTS/UIDNEXT 都不变但 FLAGS 被远端改掉的场景）
    pub synced_at: i64,
}

impl Store {
    pub fn get_folder_probe(&self, folder: &str) -> rusqlite::Result<Option<FolderProbe>> {
        let mut stmt = self.conn.prepare(
            "SELECT uidnext, exists_n, uidvalidity, synced_at FROM folder_probe WHERE folder = ?1",
        )?;
        let mut rows = stmt.query_map([folder], |r| {
            Ok(FolderProbe {
                uidnext: r.get(0)?,
                exists_n: r.get(1)?,
                uidvalidity: r.get(2)?,
                synced_at: r.get(3)?,
            })
        })?;
        rows.next().transpose()
    }

    pub fn set_folder_probe(&self, folder: &str, probe: &FolderProbe) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO folder_probe(folder, uidnext, exists_n, uidvalidity, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(folder) DO UPDATE SET
                uidnext = excluded.uidnext,
                exists_n = excluded.exists_n,
                uidvalidity = excluded.uidvalidity,
                synced_at = excluded.synced_at",
            rusqlite::params![
                folder,
                probe.uidnext,
                probe.exists_n,
                probe.uidvalidity,
                probe.synced_at
            ],
        )?;
        Ok(())
    }

    pub fn delete_folder_probe(&self, folder: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM folder_probe WHERE folder = ?1", [folder])?;
        Ok(())
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
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// 本地索引里的全部文件夹与计数（离线优先：启动即广播侧栏）
    pub fn folder_stats(&self) -> rusqlite::Result<Vec<(String, u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT folder, COUNT(*), COALESCE(SUM(seen = 0), 0)
             FROM messages GROUP BY folder",
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

    /// 空间统计（UI 显示用：索引占用多少、可回收多少）
    pub fn vacuum_stats(&self) -> rusqlite::Result<VacuumStats> {
        let freelist: i64 = self
            .conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;

        let total_bytes = page_count as u64 * page_size as u64;
        let free_bytes = freelist as u64 * page_size as u64;
        let used_bytes = total_bytes.saturating_sub(free_bytes);

        Ok(VacuumStats {
            total_bytes,
            used_bytes,
            free_bytes,
            should_vacuum: free_bytes > 10 * 1024 * 1024, // 建议阈值：10MB
        })
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

    /// 文件夹内全部哈希（删文件夹时清理本地缓存用）
    pub fn hashes_in_folder(&self, folder: &str) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT envelope_hash FROM messages WHERE folder = ?1")?;
        let rows = stmt.query_map([folder], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }
}
