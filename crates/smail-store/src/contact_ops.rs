//! 通讯录管理（v7）
//!
//! ## 职责
//!
//! - **自动收集**：收信时从发件人提取并去重
//! - **补全数据源**：list_contacts_display 返回 "名字 <email>" 格式
//! - **全量管理**：list_contacts_full（管理界面用）
//! - **删除联系人**：delete_contact

use super::{ContactRow, Store};

impl Store {
    /// 最近联系人（M7 自动补全）：发件地址去重，按最近收信时间排序。
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
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// 收信自动收集：批量 upsert（单事务；名字非空才覆盖旧名，时间总更新）。
    /// 邮箱先归一化（去空白 + 小写）：同一地址大小写/空白变体收敛到一行，
    /// 否则 UNIQUE(email) 之外会积累出显示完全相同的重复行
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
                let email = email.trim().to_lowercase();
                if email.is_empty() {
                    continue;
                }
                stmt.execute((email, name.trim(), now))?;
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
}
