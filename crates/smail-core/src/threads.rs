//! 会话聚合（M5）：In-Reply-To 直系挂接的轻量线程化。
//!
//! 设计取舍（JWZ 简化版，边界见 DESIGN.md M5）：
//! - message_id / in_reply_to 归一化（trim + 去 `<>`）后精确匹配
//! - 只认 In-Reply-To 指向的直系父件；父件不在当前集合（被删/在别的文件夹）
//!   则该邮件自成根 —— 不做 References 链回退、不做 subject 兜底合并
//! - 环保护：沿父链上行超过集合大小即停（病态数据不挂死，可能拆成两thread）
//! - 会话内按时间升序；会话间按最新活动时间降序（对齐邮件列表习惯）

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
    threads.sort_by(|a, b| b.latest().date_unix.cmp(&a.latest().date_unix));
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
