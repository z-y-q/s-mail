//! 全文搜索操作（FTS5 + bigram 切分）
//!
//! ## 职责
//!
//! - **信封索引**：index_envelope（同步时建主题+发件人索引）
//! - **正文索引**：index_body（后台索引器或打开邮件时调用）
//! - **索引状态**：unindexed/mark_indexed（工单队列）
//! - **搜索查询**：search/search_with_snippets（snippet 返回匹配摘录）
//!
//! ## 切分策略
//!
//! - **CJK**：bigram 切分（"搜索" → "搜 索"）+ 停用词过滤（减少索引体积 ~15%）
//! - **ASCII**：保留原词（"hello" → "hello"）
//! - 索引与查询两侧均用 tokenize() 预处理
//!
//! ## Snippet 高亮
//!
//! `search_with_snippets()` 使用 FTS5 snippet() 返回匹配词附近 30 字符的摘录：
//! - 匹配词用 「」 包裹高亮
//! - 省略部分用 … 标记
//! - UI 可显示："…账单「报销」流程…"

use super::{row_from, tokenize, MessageRow, Store};

/// 列表行预览提取：正文 → 纯文本单行（换行折叠、markdown 符号剥除）前 ~160 字符。
/// markdown 与纯文本正文都走此清洗（HTML 邮件在转换成 markdown 后进来）；
/// 链接语法 [文本](URL) 只保留文本——营销邮件正文常是整段超长链接，
/// 不剥除的话预览会被 URL 淹没
pub fn preview_from_body(body: &str) -> String {
    // 先剥链接：[文本](URL) → 文本（字节索引扫描，'[' 无配对 ']' 或 ']' 后
    // 非 '](' 时按普通字符通过）
    let bytes = body.as_bytes();
    let mut stripped = String::with_capacity(body.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(close_rel) = body[i..].find(']') {
                let close = i + close_rel;
                if body[close..].starts_with("](") {
                    if let Some(paren_rel) = body[close..].find(')') {
                        let paren = close + paren_rel;
                        let label = body[i + 1..close].trim();
                        if !label.is_empty() {
                            if !stripped.is_empty() {
                                stripped.push(' ');
                            }
                            stripped.push_str(label);
                        }
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        let ch = body[i..].chars().next().unwrap_or('\u{fffd}');
        stripped.push(ch);
        i += ch.len_utf8();
    }
    let mut out = String::new();
    'outer: for line in stripped.lines() {
        let t = line
            .trim()
            .trim_start_matches(['#', '>', '`'])
            .trim()
            .trim_end_matches(['`', '|'])
            .trim();
        if t.is_empty() || t == "---" {
            continue;
        }
        for word in t.split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
            if out.chars().count() >= 160 {
                break 'outer;
            }
        }
    }
    if out.chars().count() > 160 {
        out.chars().take(158).collect::<String>() + "…"
    } else {
        out
    }
}

impl Store {
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

    /// 正文索引（打开邮件或后台索引器调用）。
    /// 顺带生成列表行预览：纯文本前 ~160 字符（换行折叠为空格）
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
            "UPDATE messages SET indexed = 1, preview = ?2 WHERE envelope_hash = ?1",
            rusqlite::params![hash, preview_from_body(body)],
        )?;
        tx.commit()
    }

    /// 标记已索引但不写入正文（M6 排查：加密邮件——避免密文污染 FTS 且反复拉取）
    pub fn mark_indexed(&self, hash: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET indexed = 1 WHERE envelope_hash = ?1",
            (hash,),
        )?;
        Ok(())
    }

    /// 未建正文索引的邮件（后台索引器的工单）
    pub fn unindexed(&self, limit: u32) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT envelope_hash FROM messages WHERE indexed = 0
             ORDER BY date_unix DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

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
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// 全文搜索（bigram 切分后的 MATCH，JOIN 取原始行）
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
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }
}

#[cfg(test)]
mod preview_tests {
    use super::preview_from_body;

    #[test]
    fn strips_link_syntax_keeps_label() {
        let md = "[限时抢购](https://links.example.com/u/click?_t=abcdef&m=9324) 立即升级 [官网](https://x.example.com)";
        let p = preview_from_body(md);
        assert!(p.contains("限时抢购"), "{p}");
        assert!(p.contains("立即升级"), "{p}");
        assert!(!p.contains("https://"), "URL 必须剥离：{p}");
    }

    #[test]
    fn html_markdown_body_yields_clean_single_line() {
        let md = "## Pro 全新功能\n\n探索 MCP 和高级虚拟机工具，升级即省 68%。\n\n---\n\n[在线查看](https://x)";
        let p = preview_from_body(md);
        assert!(p.starts_with("Pro 全新功能"), "{p}");
        assert!(p.contains("升级即省 68%"), "{p}");
        assert!(!p.contains('\n'), "预览必须单行：{p}");
        assert!(!p.contains("https://"), "{p}");
    }

    #[test]
    fn plain_text_passthrough_truncated() {
        let long = "字".repeat(400);
        let p = preview_from_body(&long);
        // 超长折叠为 158 字符 + 省略号
        assert_eq!(p.chars().count(), 159);
        assert!(p.ends_with('…'));
    }
}
