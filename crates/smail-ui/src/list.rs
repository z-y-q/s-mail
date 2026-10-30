//! 邮件列表视图
//!
//! ## 职责
//!
//! - **顶栏**：搜索框、语言切换、同步状态
//! - **侧栏**：账号/文件夹树、特殊视图（草稿/发件箱/统一收件箱）
//! - **邮件列表**：虚拟化滚动、批量操作、过滤排序
//! - **显示模型构建**：会话聚合/平铺切换、过滤（全部/未读/标记）
//!
//! ## 虚拟化滚动
//!
//! - **行高**：固定 52px（[`ROW_H`]）
//! - **缓冲区**：可视区上下各 12 行（[`BUFFER`]）
//! - **性能**：千封邮件流畅滚动
//!
//! ## 会话聚合（M5）
//!
//! - **文件夹视图**：可开启会话聚合（In-Reply-To 链）
//! - **搜索/统一收件箱**：恒定平铺模式
//! - **过滤逻辑**：会话级谓词（任一成员命中显示整会话）
//! - **排序**：会话间按最新活动降序，会话内按时间升序
//!
//! ## 批量操作
//!
//! - 复选框多选 → 批量标记/移动/删除/归档/垃圾
//! - 操作条悬浮显示，跨文件夹支持

use crate::{DispKind, ListFilter, MailDisp, State};

pub(crate) const ROW_H: f32 = 64.0;
/// 可视高度初始估计值（仅首个滚动事件前使用，之后以 viewport 实测为准）
pub(crate) const DEFAULT_VIEW_H: f32 = 590.0;
/// 列表 scrollable 的部件 id（切文件夹/进搜索时 scroll_to 回顶用）
pub(crate) const MAIL_LIST_ID: &str = "mail-list";

/// 行级过滤谓词（会话视图在会话级聚合后再判）
fn row_matches(f: ListFilter, seen: bool, flagged: bool) -> bool {
    match f {
        ListFilter::All => true,
        ListFilter::Unread => !seen,
        ListFilter::Flagged => flagged,
    }
}

/// M5：会话/平铺展示条目（文件夹视图 + 开关时聚合；搜索/统一收件箱恒平铺）。
/// view 每帧读 State.mail_display 缓存，不在此重建。
/// 过滤/排序在此应用（数据层不动）：会话级谓词=任一成员命中显示整会话；
/// 排序仅改变会话间/行间顺序（会话内保持时间升序）
pub(crate) fn build_mail_display(state: &State) -> Vec<MailDisp> {
    let cur_acct = state.nav.cur.as_ref().map(|(a, _)| *a);
    let is_drafts = !state.nav.in_search
        && state.nav.cur.as_ref().map(|(_, c)| c == "__drafts__") == Some(true);
    if is_drafts {
        vec![]
    } else if state.nav.threaded && !state.nav.in_search && !state.nav.viewing_unified {
        let acct = cur_acct.unwrap_or(0);
        let flat: Vec<smail_store::MessageRow> =
            state.rows.iter().map(|(_, r)| r.clone()).collect();
        let mut v = Vec::new();
        // 组结构（根 + 展开的子行）——升序时按组反转，
        // 直接 reverse 整个列表会把子行翻到根前面（显示错乱）
        let mut groups: Vec<Vec<MailDisp>> = Vec::new();
        for t in smail_core::threads::group_threads(&flat) {
            // 会话级过滤谓词：任一成员命中即显示整个会话
            let thread_hit = match state.nav.list_filter {
                ListFilter::All => true,
                ListFilter::Unread => t.has_unseen(),
                ListFilter::Flagged => t.members.iter().any(|m| m.flagged),
            };
            if !thread_hit {
                continue;
            }
            let root_hash = t.root().envelope_hash;
            let mut group = vec![MailDisp {
                acct,
                row: t.root().clone(),
                kind: DispKind::ThreadRoot {
                    latest: t.latest().envelope_hash,
                    count: t.members.len(),
                    unseen: t.has_unseen(),
                    last_from: t.latest().from_addr.clone(),
                    last_date: t.latest().date_unix,
                },
            }];
            if state.expanded.get(&root_hash).copied().unwrap_or(false) {
                for m in t.members.iter().skip(1) {
                    group.push(MailDisp {
                        acct,
                        row: m.clone(),
                        kind: DispKind::ThreadChild,
                    });
                }
            }
            groups.push(group);
        }
        if state.nav.list_asc {
            groups.reverse();
        }
        v.extend(groups.into_iter().flatten());
        v
    } else {
        // 搜索是独立视图（无过滤/排序控件）——不继承文件夹视图的过滤与排序，
        // 否则隐形状态泄漏：搜到的结果被无声删减/重排
        let apply_filter = !state.nav.in_search;
        let mut v: Vec<MailDisp> = state
            .rows
            .iter()
            .filter(|(_, r)| !apply_filter || row_matches(state.nav.list_filter, r.seen, r.flagged))
            .map(|(a, r)| MailDisp {
                acct: *a,
                row: r.clone(),
                kind: DispKind::Flat,
            })
            .collect();
        if apply_filter && state.nav.list_asc {
            v.sort_by_key(|r| r.row.date_unix);
        }
        v
    }
}

/// 剥掉 Re:/回复: 前缀做会话主题
pub(crate) fn clean_subject(s: &str) -> String {
    let mut t = s.trim();
    loop {
        let lower = t.to_ascii_lowercase();
        if let Some(r) = lower.strip_prefix("re:") {
            t = t[r.len()..].trim_start();
        } else if t.starts_with("回复：") || t.starts_with("回复:") {
            t = t["回复：".len().min(t.len())..].trim_start();
        } else {
            return t.to_string();
        }
    }
}
