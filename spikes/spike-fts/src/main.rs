//! S4 · FTS5 + CJK bigram 检索 POC（docs/analysis/round-2 §2.1）
//!
//! 验证两件事：
//!   A. rusqlite `bundled` 构建包含 FTS5（CREATE VIRTUAL TABLE 成功）
//!   B. 应用层 bigram 预切分让 unicode61 能查中文双字词（"发票"）
//!
//! 自动验收：cargo test -p spike-fts
//! 人读演示：cargo run -p spike-fts
//!
//! 任一测试失败 → 决策点触发（round-3 §2 S4 的备选路线）。

use rusqlite::Connection;

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// 索引与查询两侧共用的切分器：ASCII 词原样、CJK 连续段滑窗切二元、空格连接。
/// unicode61 本身会做大小写折叠与标点切分，这里只负责 CJK。
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

fn setup() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch("CREATE VIRTUAL TABLE messages_fts USING fts5(subject, body);")
        .expect("FTS5 不可用：bundled 构建未启用 FTS5 —— 触发决策点（round-3 §2 S4）");
    conn
}

fn insert(conn: &Connection, subject: &str, body: &str) {
    conn.execute(
        "INSERT INTO messages_fts (subject, body) VALUES (?1, ?2)",
        [tokenize(subject), tokenize(body)],
    )
    .unwrap();
}

fn search(conn: &Connection, query: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT subject FROM messages_fts WHERE messages_fts MATCH ?1 ORDER BY rank")
        .unwrap();
    let rows = stmt
        .query_map([tokenize(query)], |row| row.get::<_, String>(0))
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn main() {
    let conn = setup();
    insert(
        &conn,
        "五月账单",
        "请查收本月的发票与报销单，发票号 INV-12345。",
    );
    insert(
        &conn,
        "面试通知",
        "明天下午三点进行第二轮面试，请准时参加。",
    );
    insert(
        &conn,
        "Re: 项目周报",
        "本周完成了 indexer 与 outbox 的联调。",
    );

    for q in ["发票", "面试", "报销", "indexer", "发票 报销"] {
        let hits = search(&conn, q);
        println!("查询 {q:?} → 命中 {}: {:?}", hits.len(), hits);
    }
    println!("\n切分示例：\"发票报销\" → {:?}", tokenize("发票报销"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts5_is_available_in_bundled_build() {
        let _ = setup();
    }

    #[test]
    fn tokenization_basics() {
        assert_eq!(tokenize("发票报销"), "发票 票报 报销");
        assert_eq!(tokenize("面试"), "面试");
        assert_eq!(tokenize("Invoice 发票"), "Invoice 发票");
        assert_eq!(tokenize("第一代计划"), "第一 一代 代计 计划"); // 跨词边界 bigram：召回优先
    }

    #[test]
    fn chinese_two_char_query_hits() {
        let conn = setup();
        insert(
            &conn,
            "五月账单",
            "请查收本月的发票与报销单，发票号 INV-12345。",
        );
        insert(
            &conn,
            "面试通知",
            "明天下午三点进行第二轮面试，请准时参加。",
        );

        assert_eq!(search(&conn, "发票").len(), 1, "双字词必须能命中");
        assert_eq!(search(&conn, "面试").len(), 1);
        assert_eq!(search(&conn, "报销").len(), 1);
        assert_eq!(search(&conn, "发票 报销").len(), 1, "多词隐式 AND");
        assert_eq!(search(&conn, "INV").len(), 1, "英文词不受影响");
        assert_eq!(search(&conn, "不存在的词组").len(), 0);
    }
}
