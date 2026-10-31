//! 发件箱管理（重试队列）
//!
//! ## 职责
//!
//! - **入队**：enqueue_outbox（发送失败时入队）
//! - **重试队列**：pending_outbox（取 pending 且未超重试上限的邮件）
//! - **状态管理**：outbox_mark（更新状态与重试计数）、outbox_reset（手动重发）
//! - **清理**：outbox_clean_sent（保留最近 100 条 sent）、outbox_remove（删除）
//! - **计数**：outbox_counts（pending/failed 计数，侧栏显示用）
//! - **穷尽处理**：fail_exhausted_outbox（重试 20 次后标记 failed）
//!
//! ## 重试策略
//!
//! - 每 6s 一轮检查，每次调用 outbox_mark 自增 attempts
//! - attempts ≥ 20 次后调用 fail_exhausted_outbox 标记为 failed
//! - 约 2 分钟后不再重试，防止无限打服务器

use super::{OutboxRow, Store};

/// 发件重试上限（每 6s 一轮 → 约 2 分钟后奔 failed，不无限打服务器）
const OUTBOX_MAX_ATTEMPTS: i64 = 20;

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_outbox(
        &self,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
        attachments: &[String],
        priority: bool,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO outbox(to_addr, cc, bcc, subject, body, attachments, priority)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            (
                to,
                cc,
                bcc,
                subject,
                body,
                attachments.join("\u{1}"),
                priority as i64,
            ),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 延后投递（撤销发送/定时发送共用）：status='hold'，到 send_at 才真投。
    /// id 由调用方生成（unix 纳秒，撤销按钮直接持此 id 删行）
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_hold_outbox(
        &self,
        id: i64,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
        attachments: &[String],
        priority: bool,
        send_at: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO outbox(id, to_addr, cc, bcc, subject, body, attachments, priority, status, send_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'hold', ?9)",
            (
                id,
                to,
                cc,
                bcc,
                subject,
                body,
                attachments.join("\u{1}"),
                priority as i64,
                send_at,
            ),
        )?;
        Ok(())
    }

    /// 到期的延后投递转 pending（flush 每轮开头调用；下一轮取走真投）
    pub fn release_due_holds(&self, now: i64) -> usize {
        self.conn
            .execute(
                "UPDATE outbox SET status='pending' WHERE status='hold' AND send_at <= ?1",
                [now],
            )
            .unwrap_or(0)
    }

    pub fn pending_outbox(&self) -> rusqlite::Result<Vec<OutboxRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, to_addr, cc, bcc, subject, body, attachments, priority, send_at, status, attempts, error \
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
                priority: r.get::<_, i64>(7)? != 0,
                send_at: r.get(8)?,
                status: r.get(9)?,
                attempts: r.get(10)?,
                error: r.get(11)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// 发件箱全量（含 failed——UI 发件箱视图用）
    pub fn list_outbox_all(&self) -> rusqlite::Result<Vec<OutboxRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, to_addr, cc, bcc, subject, body, attachments, priority, send_at, status, attempts, error \
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
                    priority: r.get::<_, i64>(7)? != 0,
                    send_at: r.get(8)?,
                    status: r.get(9)?,
                    attempts: r.get(10)?,
                    error: r.get(11)?,
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
