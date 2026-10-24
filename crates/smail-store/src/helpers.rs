//! 辅助函数
//!
//! ## 职责
//!
//! - **row_from**：rusqlite Row → MessageRow 转换器
//! - **is_cjk**：判断字符是否为中日韩统一表意文字
//! - **tokenize**：bigram 切分器（CJK 按 bigram，ASCII 保留原词）
//! - **停用词过滤**：剔除高频无意义词（的、了、是、在等）减少索引体积

use super::MessageRow;

pub(crate) fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
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

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// 中文停用词列表（高频虚词、助词、连词）
const STOPWORDS: &[&str] = &[
    "的", "了", "是", "在", "我", "有", "和", "就", "不", "人", "都", "一", "一个", "上", "也",
    "很", "到", "说", "要", "去", "你", "会", "着", "没有", "看", "好", "自己", "这", "那", "里",
    "就是", "还", "为", "能", "与", "及", "等", "但",
];

fn is_stopword_char(c: char) -> bool {
    STOPWORDS.contains(&c.to_string().as_str())
}

/// bigram 分词 + 停用词过滤
/// - CJK 字符：按 bigram 切分（"全文搜索" → "全文 文搜 搜索"）
/// - ASCII：保留完整单词
/// - 停用词：单字及其构成的 bigram 全部跳过（"我的账单" → "账单"）
pub fn tokenize(input: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut cjk = String::new();

    fn push_cjk(cjk: &mut String, tokens: &mut Vec<String>) {
        let chars: Vec<char> = cjk.chars().collect();
        // 先过滤停用词字符
        let filtered: Vec<char> = chars
            .into_iter()
            .filter(|c| !is_stopword_char(*c))
            .collect();
        match filtered.len() {
            0 => {}
            1 => tokens.push(filtered[0].to_string()),
            _ => {
                for w in filtered.windows(2) {
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
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_cjk() {
        assert_eq!(tokenize("搜索"), "搜索");
        assert_eq!(tokenize("全文搜索"), "全文 文搜 搜索");
    }

    #[test]
    fn test_tokenize_ascii() {
        assert_eq!(tokenize("hello world"), "hello world");
    }

    #[test]
    fn test_tokenize_mixed() {
        assert_eq!(tokenize("搜索 hello 世界"), "搜索 hello 世界");
    }

    #[test]
    fn test_stopwords_filtering() {
        // 停用词应被过滤
        assert_eq!(tokenize("我的账单"), "账单", "单字停用词'我的'被过滤");
        assert_eq!(tokenize("这是报销"), "报销", "'这是'被过滤");
        // 有意义的词保留
        assert_eq!(tokenize("发票报销"), "发票 票报 报销");
    }
}
