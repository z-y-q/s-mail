//! 草稿管理（本地 + 云同步）
//!
//! ## 职责
//!
//! - **草稿 CRUD**：save_draft/list_drafts/delete_draft
//! - **云同步关联**：draft_message_id/set_draft_message_id（B2 云同步用）
//! - **双向同步对账**：all_draft_mids（已上传草稿的服务器 Message-ID）
//!
//! ## 云同步策略（B2）
//!
//! - 本地草稿 id=0 时为新建，id>0 为更新
//! - message_id 为空表示未上传/仅本地，非空表示已关联服务器副本
//! - all_draft_mids 返回已上传草稿，用于双向同步对账

use super::{DraftRow, Store};

impl Store {
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
        rows.collect::<rusqlite::Result<Vec<_>>>()
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
}
