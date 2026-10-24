//! 邮件会话聚合（线程化）
//!
//! ## 职责
//!
//! - **会话聚合**：按 In-Reply-To 关系将邮件组织为线程
//! - **轻量实现**：JWZ 算法的简化版（边界见 DESIGN.md M5）
//! - **排序优化**：会话内按时间升序，会话间按最新活动降序
//!
//! ## 算法策略
//!
//! ```text
//! 邮件列表
//!   ↓
//! 归一化 Message-ID / In-Reply-To（trim + 去 `<>`）
//!   ↓
//! 精确匹配直系父邮件
//!   ↓
//! 父件不在当前集合 → 自成根
//!   ↓
//! 环保护（深度 > 集合大小即停）
//! ```
//!
//! ## 设计取舍（JWZ 简化版）
//!
//! - **只认 In-Reply-To**：指向直系父件，不做 References 链回退
//! - **无 subject 兜底**：父件被删/在别文件夹时不合并（避免误判）
//! - **环保护**：沿父链上行超过集合大小即停（病态数据不挂死，可能拆成两线程）
//! - **时间排序**：
//!   - 会话内：按时间升序（最早邮件为根）
//!   - 会话间：按最新活动时间降序（对齐邮件列表习惯）

use std::collections::HashMap;

use smail_store::MessageRow;

/// 一个会话。`members` 按时间升序，`root()` 为最早一封。
#[derive(Debug, Clone)]
pub struct Thread {
    pub members: Vec<MessageRow>,
}

impl Thread {
    pub fn root(&self) -> &MessageRow {
        self.members.first().expect("空会话")
    }

    pub fn latest(&self) -> &MessageRow {
        self.members.last().expect("空会话")
    }

    /// 任一成员未读则会话未读
    pub fn has_unseen(&self) -> bool {
        self.members.iter().any(|m| !m.seen)
    }
}

fn norm_id(s: &str) -> String {
    s.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// 把同一文件夹（或搜索结果）内的行聚成会话。
pub fn group_threads(rows: &[MessageRow]) -> Vec<Thread> {
    let by_id: HashMap<String, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (norm_id(&r.message_id), i))
        .collect();

    let mut parent: Vec<Option<usize>> = vec![None; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        let irt = norm_id(&r.in_reply_to);
        if irt.is_empty() {
            continue;
        }
        if let Some(&p) = by_id.get(&irt) {
            if p != i {
                parent[i] = Some(p);
            }
        }
    }

    // 沿父链找根；hops 上限 = 集合大小（环保护）
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..rows.len() {
        let mut cur = i;
        let mut hops = 0;
        while let Some(p) = parent[cur] {
            if hops > rows.len() {
                break;
            }
            cur = p;
            hops += 1;
        }
        groups.entry(cur).or_default().push(i);
    }

    let mut threads: Vec<Thread> = groups
        .into_values()
        .map(|mut idxs| {
            idxs.sort_by_key(|&i| rows[i].date_unix);
            Thread {
                members: idxs.into_iter().map(|i| rows[i].clone()).collect(),
            }
        })
        .collect();
    threads.sort_by_key(|a| std::cmp::Reverse(a.latest().date_unix));
    threads
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(hash: i64, mid: &str, irt: &str, date: i64, subject: &str) -> MessageRow {
        MessageRow {
            envelope_hash: hash,
            folder: "INBOX".into(),
            message_id: mid.into(),
            in_reply_to: irt.into(),
            subject: subject.into(),
            from_addr: "a@b".into(),
            date_unix: date,
            seen: false,
            flagged: false,
            has_body: false,
        }
    }

    #[test]
    fn chain_three_levels() {
        let rows = vec![
            row(3, "<c@x>", "<b@x>", 300, "Re: 主题"),
            row(1, "<a@x>", "", 100, "主题"),
            row(2, "<b@x>", "<a@x>", 200, "Re: 主题"),
            row(9, "<solo@x>", "", 250, "无关"),
        ];
        let ts = group_threads(&rows);
        assert_eq!(ts.len(), 2);
        let chain = ts.iter().find(|t| t.members.len() == 3).unwrap();
        assert_eq!(
            chain
                .members
                .iter()
                .map(|m| m.envelope_hash)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "会话内应按时间升序"
        );
        // 会话间按最新活动降序：solo(250) < chain(300)
        assert_eq!(ts[0].members.len(), 3);
    }

    #[test]
    fn orphan_reply_is_own_root() {
        let rows = vec![
            row(1, "<a@x>", "", 100, "主题"),
            row(2, "<orphan@x>", "<gone@x>", 200, "Re: 主题"),
        ];
        let ts = group_threads(&rows);
        assert_eq!(ts.len(), 2, "父件缺失的回复自成根");
    }

    #[test]
    fn id_normalization_matches() {
        let rows = vec![
            row(1, "<a@x>", "", 100, "主题"),    // 标准 <> 形式
            row(2, "<c@x>", " a@x ", 200, "Re"), // irt 无尖括号+空白，归一化后应挂上
        ];
        let ts = group_threads(&rows);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].members.len(), 2);
    }

    #[test]
    fn cycle_does_not_hang() {
        let rows = vec![
            row(1, "<a@x>", "<b@x>", 100, "A"),
            row(2, "<b@x>", "<a@x>", 200, "B"),
        ];
        let ts = group_threads(&rows); // 必须返回
        assert!(ts.len() <= 2, "环允许拆分但不许挂死");
    }

    #[test]
    fn self_reference_ignored() {
        let rows = vec![row(1, "<a@x>", "<a@x>", 100, "自指")];
        let ts = group_threads(&rows);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].members.len(), 1);
    }
}
