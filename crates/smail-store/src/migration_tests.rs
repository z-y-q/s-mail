//! Schema 迁移回归测试
//!
//! 确保 v0→v7 各个版本升级路径正确：
//! - v0：初始 schema（messages/drafts/outbox/messages_fts 基础表）
//! - v1：messages.flagged
//! - v2：messages.indexed + outbox.attachments
//! - v3：messages.in_reply_to
//! - v4：pending_flags 表
//! - v5：drafts.message_id
//! - v6：outbox.bcc + drafts.bcc
//! - v7：contacts 表 + 回填发件人

use rusqlite::Connection;
use std::path::PathBuf;

fn temp_db(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "smail-migration-{}-{}.db",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// 创建 v0 基础 schema（最早期版本：无 flagged/indexed/in_reply_to）
fn create_v0_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE messages(
            envelope_hash INTEGER PRIMARY KEY,
            folder        TEXT NOT NULL,
            message_id    TEXT,
            subject       TEXT,
            from_addr     TEXT,
            date_unix     INTEGER,
            seen          INTEGER NOT NULL DEFAULT 0,
            has_body      INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX idx_messages_folder_date ON messages(folder, date_unix DESC);
        CREATE TABLE drafts(
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            to_addr    TEXT NOT NULL DEFAULT '',
            cc         TEXT NOT NULL DEFAULT '',
            subject    TEXT NOT NULL DEFAULT '',
            body       TEXT NOT NULL DEFAULT '',
            updated_at INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE outbox(
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            to_addr TEXT NOT NULL,
            cc      TEXT NOT NULL DEFAULT '',
            subject TEXT NOT NULL DEFAULT '',
            body    TEXT NOT NULL,
            status  TEXT NOT NULL DEFAULT 'pending',
            attempts INTEGER NOT NULL DEFAULT 0,
            error   TEXT NOT NULL DEFAULT ''
        );
        CREATE VIRTUAL TABLE messages_fts USING fts5(subject, body, tokenize='unicode61');",
    )?;
    conn.pragma_update(None, "user_version", 0)?;
    Ok(())
}

#[test]
fn migration_v0_to_v7_complete() {
    let path = temp_db("v0-to-v7");
    {
        let conn = Connection::open(&path).unwrap();
        create_v0_schema(&conn).unwrap();

        // v0 数据：插入一封邮件
        conn.execute(
            "INSERT INTO messages(envelope_hash, folder, message_id, subject, from_addr, date_unix, seen, has_body)
             VALUES (1, 'INBOX', '<1@x>', '测试邮件', '张三 <test@x.com>', 1700000000, 0, 0)",
            [],
        )
        .unwrap();

        // v0 草稿：无 message_id/bcc 字段
        conn.execute(
            "INSERT INTO drafts(to_addr, cc, subject, body, updated_at)
             VALUES ('a@b.c', '', 'draft1', 'body1', 100)",
            [],
        )
        .unwrap();

        // v0 outbox：无 attachments/bcc 字段
        conn.execute(
            "INSERT INTO outbox(to_addr, cc, subject, body, status, attempts, error)
             VALUES ('x@y.z', '', 'out1', 'b1', 'pending', 0, '')",
            [],
        )
        .unwrap();

        // 确认 v0 状态
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0);
    }

    // 用 Store::open 触发完整迁移（v0→v7）
    let store = super::Store::open(&path).unwrap();

    // 验证最终版本
    let v: i64 = store
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 9, "应升级到 v9");

    // v1：flagged 字段存在且默认值正确
    let flagged: i64 = store
        .conn
        .query_row(
            "SELECT flagged FROM messages WHERE envelope_hash = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(flagged, 0);

    // v2：indexed 字段存在
    let indexed: i64 = store
        .conn
        .query_row(
            "SELECT indexed FROM messages WHERE envelope_hash = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed, 0);

    // v2：outbox.attachments 字段存在
    let att: String = store
        .conn
        .query_row("SELECT attachments FROM outbox WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(att, "");

    // v3：in_reply_to 字段存在
    let irt: String = store
        .conn
        .query_row(
            "SELECT in_reply_to FROM messages WHERE envelope_hash = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(irt, "");

    // v4：pending_flags 表存在
    store
        .conn
        .execute(
            "INSERT INTO pending_flags(envelope_hash, seen, flagged) VALUES (99, 1, NULL)",
            [],
        )
        .unwrap();

    // v5：drafts.message_id 字段存在
    let mid: String = store
        .conn
        .query_row("SELECT message_id FROM drafts WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(mid, "");

    // v6：drafts.bcc 和 outbox.bcc 字段存在
    let bcc_draft: String = store
        .conn
        .query_row("SELECT bcc FROM drafts WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bcc_draft, "");
    let bcc_outbox: String = store
        .conn
        .query_row("SELECT bcc FROM outbox WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bcc_outbox, "");

    // v7：contacts 表存在 + 回填发件人（1 条记录）
    let count: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM contacts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "应回填 1 个联系人");
    let email: String = store
        .conn
        .query_row("SELECT email FROM contacts WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(email, "张三 <test@x.com>");

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn migration_v3_to_v7_partial() {
    let path = temp_db("v3-to-v7");
    {
        let conn = Connection::open(&path).unwrap();
        create_v0_schema(&conn).unwrap();

        // 手动模拟 v1/v2/v3 迁移（跳过 v0→v3 中间步骤）
        conn.execute(
            "ALTER TABLE messages ADD COLUMN flagged INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .unwrap();
        conn.execute(
            "ALTER TABLE messages ADD COLUMN indexed INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .unwrap();
        conn.execute(
            "ALTER TABLE outbox ADD COLUMN attachments TEXT NOT NULL DEFAULT ''",
            [],
        )
        .unwrap();
        conn.execute(
            "ALTER TABLE messages ADD COLUMN in_reply_to TEXT NOT NULL DEFAULT ''",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();

        conn.execute(
            "INSERT INTO messages(envelope_hash, folder, message_id, subject, from_addr, date_unix, seen, has_body, flagged, indexed, in_reply_to)
             VALUES (2, 'INBOX', '<2@x>', 'v3邮件', '李四 <li@x.com>', 1700000100, 0, 0, 1, 0, '')",
            [],
        )
        .unwrap();
    }

    let store = super::Store::open(&path).unwrap();

    let v: i64 = store
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 9);

    // v4：pending_flags 表应创建
    store
        .conn
        .execute(
            "INSERT INTO pending_flags(envelope_hash, seen, flagged) VALUES (88, NULL, 1)",
            [],
        )
        .unwrap();

    // v7：contacts 回填
    let count: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM contacts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn migration_idempotence() {
    let path = temp_db("idempotence");
    {
        let conn = Connection::open(&path).unwrap();
        create_v0_schema(&conn).unwrap();
    }

    // 第一次迁移
    let _ = super::Store::open(&path).unwrap();
    let v1: i64 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v1, 9);

    // 第二次打开：应幂等（不重复执行迁移）
    let _ = super::Store::open(&path).unwrap();
    let v2: i64 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v2, 9);

    // contacts 表不应重复回填（幂等性）
    let count: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM contacts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "空库迁移不应产生联系人");

    std::fs::remove_file(&path).unwrap();
}
