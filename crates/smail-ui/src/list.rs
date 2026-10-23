//! 主视图：顶栏 / 侧栏 / 邮件列表（含虚拟化窗口与批量操作条）+ 显示模型构建。

use iced::widget::{button, column, container, row, scrollable, text, Space};
use iced::{Element, Length};

use crate::{theme, unix_to_local, DispKind, ListFilter, MailDisp, Message, State};

pub(crate) const ROW_H: f32 = 52.0;
/// 可视高度初始估计值（仅首个滚动事件前使用，之后以 viewport 实测为准）
pub(crate) const DEFAULT_VIEW_H: f32 = 590.0;
const BUFFER: usize = 12;
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
    let cur_acct = state.cur.as_ref().map(|(a, _)| *a);
    let is_drafts =
        !state.in_search && state.cur.as_ref().map(|(_, c)| c == "__drafts__") == Some(true);
    if is_drafts {
        vec![]
    } else if state.threaded && !state.in_search && !state.viewing_unified {
        let acct = cur_acct.unwrap_or(0);
        let flat: Vec<smail_store::MessageRow> =
            state.rows.iter().map(|(_, r)| r.clone()).collect();
        let mut v = Vec::new();
        // 组结构（根 + 展开的子行）——升序时按组反转，
        // 直接 reverse 整个列表会把子行翻到根前面（显示错乱）
        let mut groups: Vec<Vec<MailDisp>> = Vec::new();
        for t in smail_core::threads::group_threads(&flat) {
            // 会话级过滤谓词：任一成员命中即显示整个会话
            let thread_hit = match state.list_filter {
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
        if state.list_asc {
            groups.reverse();
        }
        v.extend(groups.into_iter().flatten());
        v
    } else {
        // 搜索是独立视图（无过滤/排序控件）——不继承文件夹视图的过滤与排序，
        // 否则隐形状态泄漏：搜到的结果被无声删减/重排
        let apply_filter = !state.in_search;
        let mut v: Vec<MailDisp> = state
            .rows
            .iter()
            .filter(|(_, r)| !apply_filter || row_matches(state.list_filter, r.seen, r.flagged))
            .map(|(a, r)| MailDisp {
                acct: *a,
                row: r.clone(),
                kind: DispKind::Flat,
            })
            .collect();
        if apply_filter && state.list_asc {
            v.sort_by(|a, b| a.row.date_unix.cmp(&b.row.date_unix));
        }
        v
    }
}

/// 剥掉 Re:/回复: 前缀做会话主题
fn clean_subject(s: &str) -> String {
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

pub(crate) fn main_view(state: &State) -> Element<'_, Message> {
    // 状态行过长会挤掉右侧按钮：逐段截断（字符级，不切中文字节）
    let clip = |s: &str| -> String {
        if s.chars().count() > 28 {
            let t: String = s.chars().take(28).collect();
            format!("{t}…")
        } else {
            s.to_string()
        }
    };
    let status_line: String = if state.account_names.len() > 1 {
        (0..state.account_names.len())
            .filter_map(|i| {
                state
                    .statuses
                    .get(&i)
                    .map(|s| format!("{}｜{}", state.account_names[i], clip(s)))
            })
            .collect::<Vec<_>>()
            .join("  ")
    } else {
        clip(&state.statuses.get(&0).cloned().unwrap_or_default())
    };

    let topbar = container(
        row![
            button(text(state.l().compose_new).size(14))
                .on_press_maybe(if state.account_names.is_empty() {
                    None
                } else {
                    Some(Message::ComposeNew)
                })
                .padding([5, 12])
                .style(theme::accent_button),
            text("s-mail").size(16),
            button(
                text(if state.threaded {
                    state.l().view_flat
                } else {
                    state.l().view_threads
                })
                .size(12)
            )
            .on_press(Message::ToggleThreading)
            .padding([4, 8])
            .style(theme::ghost_button),
            text_input_search(state),
            button(text("↻").size(14))
                .on_press(Message::ManualRefresh)
                .padding([4, 8])
                .style(theme::ghost_button),
            text(status_line.clone())
                .size(11)
                .style(theme::secondary_text),
            Space::new().width(Length::Fill),
            button(
                text(if matches!(state.prefs.ui_theme, iced::Theme::Dark) {
                    "☀️"
                } else {
                    "🌙"
                })
                .size(13),
            )
            .on_press(Message::ToggleUiTheme)
            .padding([4, 8])
            .style(theme::ghost_button),
            button(text(state.prefs.lang.toggle_label()).size(12))
                .on_press(Message::ToggleLang)
                .padding([4, 8])
                .style(theme::ghost_button),
            button(text(state.l().add_account).size(12))
                .on_press(Message::SetupOpen)
                .padding([4, 8])
                .style(theme::ghost_button),
        ]
        .spacing(10)
        .padding([8, 14]),
    )
    .style(theme::toolbar_bg);

    // 侧栏：按账号分组（macOS Mail 式：重底色 + 圆角选中）
    let mut sidebar = column![].spacing(1).padding(10);
    // 统一收件箱入口（多账号才有意义）
    if state.account_names.len() > 1 {
        let unified_cur = state.viewing_unified;
        let total_unread: u32 = state
            .folders
            .iter()
            .filter(|(_, f)| f.path.eq_ignore_ascii_case("INBOX"))
            .map(|(_, f)| f.unseen)
            .sum();
        sidebar = sidebar.push(
            button(
                row![text(if total_unread > 0 {
                    crate::lang::f(state.l().unified_unread, &[&total_unread])
                } else {
                    state.l().unified.to_string()
                })
                .size(13),]
                .align_y(iced::alignment::Vertical::Center),
            )
            .on_press(Message::UnifiedInboxSelected)
            .padding([3, 6])
            .width(Length::Fill)
            .style(move |t, st| theme::sidebar_item(t, st, unified_cur)),
        );
        sidebar = sidebar.push(Space::new().height(6));
    }
    if state.account_names.is_empty() {
        sidebar = sidebar
            .push(
                text(state.l().no_account)
                    .size(12)
                    .style(theme::secondary_text),
            )
            .push(Space::new().height(4))
            .push(
                button(text(state.l().add_account_big).size(13))
                    .on_press(Message::SetupOpen)
                    .padding([4, 8])
                    .style(theme::accent_button),
            );
    }
    for (i, name) in state.account_names.iter().enumerate() {
        sidebar = sidebar.push(text(name.clone()).size(11).style(theme::secondary_text));
        sidebar = sidebar.push(Space::new().height(2));
        let drafts_cur = state
            .cur
            .as_ref()
            .map(|(a, c)| *a == i && c == "__drafts__")
            == Some(true);
        let draft_count = state.drafts.iter().filter(|(a, _)| *a == i).count();
        sidebar = sidebar.push(
            button(
                text(format!(
                    "  {} {}",
                    if drafts_cur { "▸" } else { " " },
                    if draft_count > 0 {
                        crate::lang::f(state.l().drafts_n, &[&draft_count])
                    } else {
                        state.l().drafts.to_string()
                    }
                ))
                .size(13),
            )
            .on_press(Message::FolderSelected(i, "__drafts__".into()))
            .padding([3, 6])
            .style(move |t, st| theme::sidebar_item(t, st, drafts_cur)),
        );
        let (ob_p, ob_f) = state.outbox_counts.get(&i).copied().unwrap_or((0, 0));
        if ob_p + ob_f > 0 {
            let ob_cur = state
                .cur
                .as_ref()
                .map(|(a, c)| *a == i && c == "__outbox__")
                == Some(true);
            let ob_label = format!(
                "  {} {}",
                if ob_cur { "▸" } else { " " },
                if ob_f > 0 {
                    format!("{}（⏳{ob_p} ✗{ob_f}）", state.l().outbox_label)
                } else {
                    format!("{}（⏳{ob_p}）", state.l().outbox_label)
                }
            );
            sidebar = sidebar.push(
                button(text(ob_label).size(13))
                    .on_press(Message::FolderSelected(i, "__outbox__".into()))
                    .padding([3, 6])
                    .width(Length::Fill)
                    .style(move |t, st| theme::sidebar_item(t, st, ob_cur)),
            );
        }
        for (_a, f) in state.folders.iter().filter(|(a, _)| *a == i) {
            // 服务器 Drafts 由本地草稿行代表（避免双入口双数据源）
            if f.kind == smail_core::FolderKind::Drafts {
                continue;
            }
            // 层级树：按 '/' 深度缩进；特殊文件夹显示本地化名（选中/操作仍用全路径）
            let depth = f.path.matches('/').count();
            // 改名输入行取代原行（同 new_folder 的两态模式）
            if let Some((ra, rpath, rname)) = &state.rename_folder {
                if *ra == i && rpath == &f.path {
                    sidebar = sidebar.push(
                        row![
                            iced::widget::text_input(state.l().new_folder_ph, rname)
                                .on_input(Message::RenameFolderInput)
                                .on_submit(Message::RenameFolderConfirm)
                                .size(12)
                                .width(Length::Fill)
                                .padding(3),
                            button(text(state.l().do_rename).size(12))
                                .on_press(Message::RenameFolderConfirm)
                                .padding([3, 8])
                                .style(theme::accent_button),
                            button(text(state.l().cancel).size(12))
                                .on_press(Message::RenameFolderCancel)
                                .padding([3, 6]),
                        ]
                        .spacing(4),
                    );
                    continue;
                }
            }
            let is_cur = state.cur.as_ref().map(|(ca, c)| *ca == i && c == &f.path) == Some(true);
            let unread_n = f.unseen;
            let unread_pill: Option<Element<Message>> = if unread_n > 0 {
                Some(
                    container(text(unread_n.to_string()).size(11))
                        .padding([0, 7])
                        .style(theme::unread_pill)
                        .into(),
                )
            } else {
                None
            };
            let folder_btn = button(
                row![
                    text(format!(
                        "{}{}{} {}",
                        "  ".repeat(depth),
                        match f.kind {
                            smail_core::FolderKind::Inbox => "📥",
                            smail_core::FolderKind::Sent => "📤",
                            smail_core::FolderKind::Junk => "🚫",
                            smail_core::FolderKind::Archive => "🗄",
                            smail_core::FolderKind::Trash => "🗑",
                            _ => " ",
                        },
                        if is_cur { "▸" } else { " " },
                        crate::lang::folder_display(state.l(), f.kind, &f.path)
                    ))
                    .size(13),
                    Space::new().width(Length::Fill),
                    unread_pill,
                ]
                .align_y(iced::alignment::Vertical::Center),
            )
            .on_press(Message::FolderSelected(i, f.path.clone()))
            .padding([3, 6])
            .width(Length::Fill)
            .style(move |t, st| theme::sidebar_item(t, st, is_cur));
            // M7：非 INBOX 行尾加改名/删除小按钮（与主按钮并列，避免嵌套按钮吞事件）
            let ren: Option<Element<Message>> = if f.path.eq_ignore_ascii_case("INBOX") {
                None
            } else {
                Some(
                    button(text("✎").size(10))
                        .on_press(Message::RenameFolderOpen(i, f.path.clone()))
                        .padding([2, 4])
                        .style(theme::ghost_button)
                        .into(),
                )
            };
            let del: Option<Element<Message>> = if f.path.eq_ignore_ascii_case("INBOX") {
                None
            } else {
                Some(
                    button(text("✕").size(10))
                        .on_press(Message::AskFolderDelete(i, f.path.clone()))
                        .padding([2, 4])
                        .style(button::danger)
                        .into(),
                )
            };
            let mut folder_row = row![].spacing(2).width(Length::Fill);
            folder_row = folder_row.push(folder_btn);
            if let Some(r) = ren {
                folder_row = folder_row.push(r);
            }
            if let Some(d) = del {
                folder_row = folder_row.push(d);
            }
            sidebar = sidebar.push(folder_row);
        }
        // M7：账号组尾——新建文件夹（按钮/输入行两态）
        if let Some((nf_acct, nf_name)) = &state.new_folder {
            if *nf_acct == i {
                sidebar = sidebar.push(
                    row![
                        new_folder_input(state, nf_name),
                        button(text(state.l().do_create).size(12))
                            .on_press(Message::NewFolderConfirm)
                            .padding([3, 8])
                            .style(theme::accent_button),
                        button(text(state.l().cancel).size(12))
                            .on_press(Message::NewFolderCancel)
                            .padding([3, 6]),
                    ]
                    .spacing(4),
                );
            }
        } else {
            sidebar = sidebar.push(
                button(text(state.l().new_folder).size(11))
                    .on_press(Message::NewFolderOpen(i))
                    .padding([2, 6])
                    .style(theme::ghost_button),
            );
        }
        sidebar = sidebar.push(Space::new().height(8));
    }
    let sidebar = container(scrollable(sidebar))
        .width(200)
        .height(Length::Fill)
        .style(theme::sidebar_bg);

    // 列表
    let cur_name = if state.viewing_unified {
        crate::lang::f(state.l().unified_hdr, &[&state.rows.len()])
    } else if state.in_search {
        crate::lang::f(
            state.l().search_hdr,
            &[&state.search_query, &state.rows.len()],
        )
    } else {
        match &state.cur {
            Some((a, f)) if f == "__drafts__" => crate::lang::f(
                state.l().drafts_hdr,
                &[
                    &state
                        .account_names
                        .get(*a)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "?".into()),
                    &state.drafts.iter().filter(|(x, _)| x == a).count(),
                ],
            ),
            Some((a, f)) => {
                // 列表头也显示本地化文件夹名（与侧栏一致）
                let fname = state
                    .folders
                    .iter()
                    .find(|(fa, fi)| *fa == *a && &fi.path == f)
                    .map(|(_, fi)| crate::lang::folder_display(state.l(), fi.kind, &fi.path))
                    .unwrap_or_else(|| f.clone());
                format!(
                    "{} / {fname}",
                    state
                        .account_names
                        .get(*a)
                        .map(|s| s.as_str())
                        .unwrap_or("?")
                )
            }
            None => String::new(),
        }
    };

    let is_drafts =
        !state.in_search && state.cur.as_ref().map(|(_, c)| c == "__drafts__") == Some(true);
    let is_outbox =
        !state.in_search && state.cur.as_ref().map(|(_, c)| c == "__outbox__") == Some(true);
    let cur_acct = state.cur.as_ref().map(|(a, _)| *a);

    // 过滤/排序条（文件夹与统一收件箱视图；草稿/搜索不加）。
    // 判据用底层数据 rows 而非显示结果——过滤后空结果时控件必须仍在，
    // 否则用户被困在空过滤里（曾发生：过滤★无结果→条消失→无法切回全部）
    let filter_bar: Option<Element<Message>> =
        if !is_drafts && !state.in_search && !state.rows.is_empty() {
            let mk = |label: String, f: ListFilter, active: bool| {
                button(text(label).size(11))
                    .on_press(Message::SetListFilter(f))
                    .padding([2, 8])
                    .style(move |t, st| {
                        if active {
                            theme::accent_button(t, st)
                        } else {
                            theme::ghost_button(t, st)
                        }
                    })
            };
            let sort_label = if state.list_asc {
                state.l().sort_asc
            } else {
                state.l().sort_desc
            };
            Some(
                row![
                    mk(
                        state.l().filter_all.to_string(),
                        ListFilter::All,
                        state.list_filter == ListFilter::All
                    ),
                    mk(
                        state.l().filter_unread.to_string(),
                        ListFilter::Unread,
                        state.list_filter == ListFilter::Unread
                    ),
                    mk(
                        state.l().filter_starred.to_string(),
                        ListFilter::Flagged,
                        state.list_filter == ListFilter::Flagged
                    ),
                    Space::new().width(Length::Fill),
                    button(text(sort_label).size(11))
                        .on_press(Message::ToggleListSort)
                        .padding([2, 8])
                        .style(theme::ghost_button),
                ]
                .spacing(4)
                .into(),
            )
        } else {
            None
        };

    let mail_display = &state.mail_display;
    let n_entries = if is_drafts {
        state
            .drafts
            .iter()
            .filter(|(a, _)| Some(*a) == cur_acct)
            .count()
    } else if is_outbox {
        state
            .outbox_rows
            .iter()
            .filter(|(a, _)| Some(*a) == cur_acct)
            .count()
    } else {
        mail_display.len()
    };
    // 虚拟化窗口钳制：过滤/同步删行/删草稿后 list_offset 可能超出新列表
    // （空白屏；drafts 分支 first>last 还会 usize 下溢 panic）
    let max_off = (n_entries.saturating_sub(1) as f32 * ROW_H).max(0.0);
    let first = (((state.list_offset.min(max_off)) / ROW_H) as usize).saturating_sub(BUFFER);
    let visible = (state.list_view_h / ROW_H).ceil() as usize + BUFFER * 2;
    let last = (first + visible).min(n_entries);
    let top_pad = first as f32 * ROW_H;
    let bottom_pad = (n_entries - last) as f32 * ROW_H;

    let mut list = column![
        text(cur_name).size(12),
        Space::new()
            .width(Length::Fill)
            .height(Length::Fixed(top_pad)),
    ];
    if let Some(fb) = filter_bar {
        list = list.push(fb);
    }
    // Trash 视图：清空废纸篓（含确认条——不可恢复操作）
    if !is_drafts && !is_outbox && state.cur_is_trash_folder() && !state.rows.is_empty() {
        if state.pending_empty_trash {
            let n = state.rows.len();
            list = list.push(
                container(
                    row![
                        text(crate::lang::f(state.l().empty_trash_q, &[&n])).size(12),
                        button(text(state.l().perm_delete).size(12))
                            .on_press(Message::EmptyTrashConfirm)
                            .style(button::danger)
                            .padding([2, 8]),
                        button(text(state.l().cancel).size(12))
                            .on_press(Message::EmptyTrashCancel)
                            .padding([2, 8]),
                        Space::new().width(Length::Fill),
                    ]
                    .spacing(8),
                )
                .padding([6, 8])
                .width(Length::Fill)
                .style(|t| theme::status_badage(t, theme::BadgeKind::Warn)),
            );
        } else {
            list = list.push(
                button(text(state.l().empty_trash).size(11))
                    .on_press(Message::EmptyTrashRequest)
                    .padding([2, 8])
                    .style(button::danger),
            );
        }
    }

    // M7：文件夹删除确认条（破坏性操作，列表顶部醒目确认）
    if let Some((_fa, fpath)) = &state.pending_folder_delete {
        let fpath = fpath.clone();
        list = list.push(
            container(
                row![
                    text(crate::lang::f(state.l().folder_del_q, &[&fpath])).size(12),
                    button(text(state.l().perm_delete).size(12))
                        .on_press(Message::FolderDeleteConfirm)
                        .style(button::danger)
                        .padding([2, 8]),
                    button(text("取消").size(12))
                        .on_press(Message::FolderDeleteCancel)
                        .padding([2, 8]),
                    Space::new().width(Length::Fill),
                ]
                .spacing(8),
            )
            .padding([6, 8])
            .width(Length::Fill)
            .style(|t| theme::status_badage(t, theme::BadgeKind::Warn)),
        );
    }

    // M7：批量操作条（有选择时显示；搜索结果跨账号也可批量）
    if !is_drafts && !state.selection.is_empty() {
        list = list.push(batch_bar(state));
    }
    // AI 整理计划预览（AI 只出建议；「应用」是人工点击，走与手工批量
    // 完全相同的命令路径——含乐观移除与后台搬运）
    if state.ai.plan_loading {
        list = list.push(
            container(text(state.l().ai_plan_loading).size(12))
                .padding([4, 6])
                .style(theme::reader_meta),
        );
    } else if let Some(groups) = &state.ai.plan {
        let n_checked = groups.iter().filter(|g| g.checked).count();
        let mut panel = column![].spacing(2);
        for (i, gui) in groups.iter().enumerate() {
            let g = &gui.group;
            let op_desc = match &g.op {
                smail_ai::PlanOp::MoveTo(f) => crate::lang::f(state.l().ai_plan_move_to, &[f]),
                smail_ai::PlanOp::MarkSeen => state.l().mark_read.to_string(),
                smail_ai::PlanOp::MarkStar => state.l().star.to_string(),
            };
            let on = gui.checked;
            panel = panel.push(
                button(
                    row![
                        text(if on { "☑" } else { "☐" }).size(13),
                        text(format!(
                            "{}{} {}",
                            g.title,
                            crate::lang::f(state.l().ai_plan_group_n, &[&g.ids.len()]),
                            op_desc
                        ))
                        .size(12),
                        Space::new().width(Length::Fill),
                        button(text(if gui.expanded { "▾" } else { "▸" }).size(11))
                            .on_press(Message::AiPlanExpand(i))
                            .padding([2, 6])
                            .style(theme::ghost_button),
                    ]
                    .spacing(6)
                    .align_y(iced::alignment::Vertical::Center),
                )
                .on_press(Message::AiPlanToggle(i))
                .padding([2, 6])
                .width(Length::Fill)
                .style(move |t, st| {
                    if on {
                        theme::sidebar_item(t, st, true)
                    } else {
                        theme::ghost_button(t, st)
                    }
                }),
            );
            // 展开态：成员清单（主题行；从当前 rows 反查，应用前都在）
            if gui.expanded {
                let mut members = column![].spacing(1).padding([0, 26]);
                for (ma, mh) in &g.ids {
                    let subj = state
                        .rows
                        .iter()
                        .find(|(ra, r)| *ra == *ma && r.envelope_hash == *mh)
                        .map(|(_, r)| {
                            if r.subject.is_empty() {
                                state.l().no_subject.to_string()
                            } else {
                                r.subject.clone()
                            }
                        })
                        .unwrap_or_else(|| "-".into());
                    members = members.push(
                        text(format!("· {subj}"))
                            .size(11)
                            .style(theme::secondary_text),
                    );
                }
                panel = panel.push(members);
            }
        }
        panel = panel.push(
            row![
                button(text(crate::lang::f(state.l().ai_plan_apply, &[&n_checked])).size(12))
                    .on_press(Message::AiPlanApply)
                    .padding([3, 10])
                    .style(theme::accent_button),
                button(text(state.l().cancel).size(12))
                    .on_press(Message::AiPlanCancel)
                    .padding([3, 10])
                    .style(theme::ghost_button),
                Space::new().width(Length::Fill),
            ]
            .spacing(6),
        );
        list = list.push(container(panel).padding(4).style(theme::reader_meta));
    }

    if is_drafts {
        let items: Vec<_> = state
            .drafts
            .iter()
            .filter(|(a, _)| Some(*a) == cur_acct)
            .skip(first)
            .take(last.saturating_sub(first))
            .collect();
        for (a, d) in items {
            list = list.push(
                button(
                    column![
                        text(format!(
                            "✎ {}",
                            if d.subject.is_empty() {
                                state.l().no_subject
                            } else {
                                &d.subject
                            }
                        ))
                        .size(13),
                        text(format!("→ {}", d.to_addr)).size(11),
                    ]
                    .spacing(2),
                )
                .on_press(Message::DraftRowClicked(*a, d.id))
                .padding([6, 8]),
            );
        }
    } else if is_outbox {
        let items: Vec<_> = state
            .outbox_rows
            .iter()
            .filter(|(a, _)| Some(*a) == cur_acct)
            .skip(first)
            .take(last.saturating_sub(first))
            .collect();
        for (_a, o) in items {
            let (icon, err) = if o.status == "failed" {
                ("✗", o.error.clone())
            } else {
                ("⏳", String::new())
            };
            list = list.push(
                column![
                    row![
                        text(format!(
                            "{icon} {}",
                            if o.subject.is_empty() {
                                state.l().no_subject
                            } else {
                                &o.subject
                            }
                        ))
                        .size(13),
                        Space::new().width(Length::Fill),
                        if o.status == "failed" {
                            button(text(state.l().outbox_retry).size(11))
                                .on_press(Message::OutboxRetry(o.id))
                                .padding([2, 6])
                                .style(theme::accent_button)
                        } else {
                            button(text(state.l().outbox_retry).size(11))
                                .padding([2, 6])
                                .style(theme::ghost_button)
                        },
                        button(text("✕").size(11))
                            .on_press(Message::OutboxRemove(o.id))
                            .padding([2, 6])
                            .style(button::danger),
                    ]
                    .spacing(6)
                    .align_y(iced::alignment::Vertical::Center),
                    text(format!("→ {}", o.to_addr))
                        .size(11)
                        .style(theme::secondary_text),
                    text(format!("{err}（{}/{}）", o.attempts, 5))
                        .size(10)
                        .style(theme::secondary_text),
                ]
                .spacing(2),
            );
        }
    } else {
        for d in &mail_display[first.min(mail_display.len())..last.min(mail_display.len())] {
            let (acct, r) = (d.acct, &d.row);
            let is_sel = state.selected == Some((acct, r.envelope_hash));
            let tag = if state.viewing_unified {
                let acct_name = state
                    .account_names
                    .get(acct)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("  [{}]", acct_name)
            } else if state.in_search {
                let acct_name = state
                    .account_names
                    .get(acct)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                // 文件夹名本地化（与侧栏一致，不再显示 INBOX 等服务器原名）
                let fname = state
                    .folders
                    .iter()
                    .find(|(fa, fi)| *fa == acct && fi.path.eq_ignore_ascii_case(&r.folder))
                    .map(|(_, fi)| crate::lang::folder_display(state.l(), fi.kind, &fi.path))
                    .unwrap_or_else(|| r.folder.clone());
                format!("  [{acct_name} / {fname}]")
            } else {
                String::new()
            };
            match &d.kind {
                DispKind::ThreadRoot {
                    latest,
                    count,
                    unseen,
                    last_from,
                    last_date,
                } => {
                    let badge = if *count > 1 {
                        format!("（{}）", count)
                    } else {
                        String::new()
                    };
                    let expanded_now = state
                        .expanded
                        .get(&r.envelope_hash)
                        .copied()
                        .unwrap_or(false);
                    let star_txt: Element<Message> = if r.flagged {
                        text("★")
                            .size(12)
                            .style(|_t| text::Style {
                                color: Some(theme::star_color()),
                            })
                            .into()
                    } else {
                        text("").size(12).into()
                    };
                    // 会话根的选择框挂在最新一封（批量操作对象直觉是所见最新信）
                    let cb = select_checkbox(state, acct, *latest);
                    list = list.push(
                        row![
                            cb,
                            button(
                                column![
                                    row![
                                        star_txt,
                                        text(format!(
                                            "{} {}{badge}",
                                            if expanded_now { "▾" } else { "▸" },
                                            if *unseen { "●" } else { "" },
                                        ))
                                        .size(13),
                                        text(clean_subject(&r.subject)).size(13).font(
                                            theme::ui_font(if *unseen {
                                                iced::font::Weight::Semibold
                                            } else {
                                                iced::font::Weight::Normal
                                            })
                                        ),
                                        Space::new().width(Length::Fill),
                                        text(unix_to_local(*last_date))
                                            .size(11)
                                            .style(theme::secondary_text),
                                    ],
                                    text(format!("{last_from}{tag}"))
                                        .size(11)
                                        .style(theme::secondary_text),
                                ]
                                .spacing(2),
                            )
                            .on_press(Message::ThreadRootClicked(acct, r.envelope_hash, *latest))
                            .padding([6, 8])
                            .width(Length::Fill)
                            .style(move |t, st| theme::list_row(t, st, is_sel)),
                        ]
                        .spacing(2),
                    );
                }
                DispKind::ThreadChild => {
                    let star_txt: Element<Message> = if r.flagged {
                        text("★")
                            .size(11)
                            .style(|_t| text::Style {
                                color: Some(theme::star_color()),
                            })
                            .into()
                    } else {
                        text("").size(11).into()
                    };
                    let cb = select_checkbox(state, acct, r.envelope_hash);
                    list = list.push(
                        row![
                            cb,
                            button(
                                column![
                                    row![
                                        star_txt,
                                        text(format!("↳ {}", r.subject)).size(12),
                                        Space::new().width(Length::Fill),
                                        text(unix_to_local(r.date_unix))
                                            .size(10)
                                            .style(theme::secondary_text),
                                    ],
                                    text(format!("   {}", r.from_addr))
                                        .size(10)
                                        .style(theme::secondary_text),
                                ]
                                .spacing(1),
                            )
                            .on_press(Message::RowSelected(acct, r.envelope_hash))
                            .padding([4, 10])
                            .width(Length::Fill)
                            .style(move |t, st| theme::list_row(t, st, is_sel)),
                        ]
                        .spacing(2),
                    );
                }
                DispKind::Flat => {
                    let dot = if r.seen { "" } else { "●" };
                    let star_txt: Element<Message> = if r.flagged {
                        text("★")
                            .size(12)
                            .style(|_t| text::Style {
                                color: Some(theme::star_color()),
                            })
                            .into()
                    } else {
                        text("").size(12).into()
                    };
                    let subj_weight = if r.seen {
                        iced::font::Weight::Normal
                    } else {
                        iced::font::Weight::Semibold
                    };
                    let cb = select_checkbox(state, acct, r.envelope_hash);
                    list = list.push(
                        row![
                            cb,
                            button(
                                column![
                                    row![
                                        star_txt,
                                        text(format!("{dot} {}", r.subject))
                                            .size(13)
                                            .font(theme::ui_font(subj_weight)),
                                        Space::new().width(Length::Fill),
                                        text(unix_to_local(r.date_unix))
                                            .size(11)
                                            .style(theme::secondary_text),
                                    ],
                                    if state.in_search {
                                        if let Some(snip) =
                                            state.search_snippets.get(&r.envelope_hash)
                                        {
                                            text(format!("  「{snip}」"))
                                                .size(11)
                                                .style(theme::secondary_text)
                                        } else {
                                            text(format!("{}{tag}", r.from_addr))
                                                .size(11)
                                                .style(theme::secondary_text)
                                        }
                                    } else {
                                        text(format!("{}{tag}", r.from_addr))
                                            .size(11)
                                            .style(theme::secondary_text)
                                    }
                                ]
                                .spacing(2),
                            )
                            .on_press(Message::RowSelected(acct, r.envelope_hash))
                            .padding([6, 8])
                            .width(Length::Fill)
                            .style(move |t, st| theme::list_row(t, st, is_sel)),
                        ]
                        .spacing(2),
                    );
                }
            }
        }
    }
    if !is_drafts && mail_display.is_empty() && !state.in_search {
        let (icon, msg): (&str, String) = if state.account_names.is_empty() {
            ("📬", state.l().empty_add_account.into())
        } else if !state.rows.is_empty() {
            // 底层有数据但显示为空 = 被过滤掉了，提示切回而非误导“没有邮件”
            let f = match state.list_filter {
                ListFilter::All => "",
                ListFilter::Unread => state.l().filter_unread,
                ListFilter::Flagged => state.l().filter_starred,
            };
            ("🔍", crate::lang::f(state.l().empty_no_match, &[&f]))
        } else {
            ("📭", state.l().empty_no_mail.into())
        };
        list = list.push(
            container(
                column![
                    text(icon).size(28),
                    text(msg).size(13).style(theme::secondary_text),
                ]
                .spacing(6)
                .align_x(iced::alignment::Alignment::Center),
            )
            .width(Length::Fill)
            .padding([24, 0])
            .center(Length::Fill),
        );
    }
    // 满页提示：未到底提示可继续滚动加载；到底后给出总数确认
    if !is_drafts && state.rows.len() >= smail_core::UI_LIST_LIMIT as usize {
        let msg = if state.viewing_unified {
            crate::lang::f(state.l().unified_cap, &[&state.rows.len()])
        } else if state.load_more_done {
            crate::lang::f(state.l().at_bottom, &[&state.rows.len()])
        } else {
            crate::lang::f(state.l().loaded_pages, &[&state.rows.len()])
        };
        list = list.push(
            container(text(msg).size(11).style(theme::secondary_text))
                .width(Length::Fill)
                .center_x(Length::Fill)
                .padding([6, 0]),
        );
    }
    list = list.push(
        Space::new()
            .width(Length::Fill)
            .height(Length::Fixed(bottom_pad)),
    );

    let mail_list = container(
        scrollable(list)
            .id(iced::widget::Id::new(MAIL_LIST_ID))
            .height(Length::Fill)
            .on_scroll(|vp: scrollable::Viewport| {
                Message::ListScrolled(vp.absolute_offset().y, vp.bounds().height)
            }),
    )
    .width(380);

    let reader: Element<Message> = crate::reader::reader_view(state);

    // M6：OAuth 设备流横幅（授权 URL + 验证码，下一条状态事件自动消失）
    let mut root = column![topbar];
    if let Some((url, code)) = &state.oauth_prompt {
        root = root.push(
            container(
                row![
                    text(state.l().oauth_title).size(13),
                    text(crate::lang::f(state.l().oauth_body, &[&url])).size(13),
                    text(code.clone())
                        .size(13)
                        .font(theme::ui_font(iced::font::Weight::Bold)),
                    button(text(state.l().oauth_open).size(12))
                        .on_press(Message::OAuthOpenUrl)
                        .padding([3, 10])
                        .style(theme::accent_button),
                    Space::new().width(Length::Fill),
                ]
                .spacing(6),
            )
            .padding([8, 16])
            .style(|t| theme::status_badage(t, theme::BadgeKind::Warn))
            .width(Length::Fill),
        );
    }
    // 断连黄条（持续提示直到重连成功——密码错/断网不再只是顶栏一闪而过）
    let disconnected: Vec<String> = (0..state.account_names.len())
        .filter(|i| state.conn_ok.get(i) == Some(&false))
        .map(|i| state.account_names[i].clone())
        .collect();
    if !disconnected.is_empty() {
        root = root.push(
            container(
                row![
                    text(state.l().conn_lost).size(12),
                    text(disconnected.join("、")).size(12),
                    Space::new().width(Length::Fill),
                    text(state.l().conn_retrying)
                        .size(11)
                        .style(theme::secondary_text),
                ]
                .spacing(8),
            )
            .padding([6, 16])
            .width(Length::Fill)
            .style(|t| theme::status_badage(t, theme::BadgeKind::Warn)),
        );
    }
    // AI 隐私门（首次使用前确认一次；ai_ack 记入 ui.json）
    if state.ai.privacy_pending.is_some() {
        root = root.push(
            container(
                row![
                    text(state.l().ai_privacy_title).size(13),
                    text(crate::lang::f(
                        state.l().ai_privacy_text,
                        &[&state.ai.cfg.base_url],
                    ))
                    .size(12)
                    .style(theme::secondary_text),
                    Space::new().width(Length::Fill),
                    button(text(state.l().ai_privacy_go).size(12))
                        .on_press(Message::AiPrivacyAck)
                        .padding([3, 10])
                        .style(theme::accent_button),
                    button(text(state.l().cancel).size(12))
                        .on_press(Message::AiPrivacyCancel)
                        .padding([3, 10])
                        .style(theme::ghost_button),
                ]
                .spacing(8),
            )
            .padding([8, 16])
            .width(Length::Fill)
            .style(|t| theme::status_badage(t, theme::BadgeKind::Warn)),
        );
    }
    column![
        root,
        row![sidebar, mail_list, reader]
            .spacing(1)
            .height(Length::Fill),
    ]
    .into()
}

// ---------------------------------------------------------------------------
// M7 辅助：选择框 / 批量操作条
// ---------------------------------------------------------------------------

fn select_checkbox(state: &State, acct: usize, hash: i64) -> Element<'_, Message> {
    let on = state.selection.contains(&(acct, hash));
    button(text(if on { "☑" } else { "☐" }).size(13))
        .on_press(Message::ToggleSelect(acct, hash))
        .padding([6, 2])
        .style(move |t, st| {
            if on {
                theme::accent_button(t, st)
            } else {
                theme::ghost_button(t, st)
            }
        })
        .into()
}

fn batch_bar(state: &State) -> Element<'_, Message> {
    let n = state.selection.len();
    let bar = row![
        text(crate::lang::f(state.l().selected_n, &[&n])).size(12),
        button(text(state.l().select_all).size(12))
            .on_press(Message::SelectAll)
            .padding([2, 8])
            .style(theme::ghost_button),
        button(text(state.l().mark_read).size(12))
            .on_press(Message::BatchSeen(true))
            .padding([2, 8])
            .style(theme::ghost_button),
        button(text(state.l().mark_unread).size(12))
            .on_press(Message::BatchSeen(false))
            .padding([2, 8])
            .style(theme::ghost_button),
        button(text(state.l().star).size(12))
            .on_press(Message::BatchStar)
            .padding([2, 8])
            .style(theme::ghost_button),
        button(text(state.l().batch_move).size(12))
            .on_press(Message::BatchMoveOpen)
            .padding([2, 8])
            .style(theme::ghost_button),
        button(text(state.l().batch_archive).size(12))
            .on_press(Message::BatchArchive)
            .padding([2, 8])
            .style(theme::ghost_button),
        button(text(state.l().batch_spam).size(12))
            .on_press(Message::BatchSpam)
            .padding([2, 8])
            .style(theme::ghost_button),
        button(text(state.l().batch_delete).size(12))
            .on_press(Message::AskBatchDelete)
            .padding([2, 8])
            .style(theme::ghost_button),
        Space::new().width(Length::Fill),
        button(
            text(if state.ai.busy == Some(crate::ai::BusyKind::Organize) {
                state.l().ai_working
            } else {
                state.l().ai_organize_btn
            })
            .size(12),
        )
        .on_press_maybe(if state.ai.busy.is_some() {
            None
        } else {
            Some(Message::AiOrganizeGo)
        })
        .padding([2, 8])
        .style(theme::ghost_button),
        button(text(state.l().clear).size(12))
            .on_press(Message::ClearSelection)
            .padding([2, 8])
            .style(theme::ghost_button),
    ]
    .spacing(4);
    let mut col = column![container(bar).padding([4, 6]).style(theme::reader_meta)].spacing(2);

    // 移动目标列表：选择涉及的各账号，分组列出（排除各自当前文件夹）
    if state.batch_move_open {
        let mut accts: Vec<usize> = state.selection.iter().map(|(a, _)| *a).collect();
        accts.sort_unstable();
        accts.dedup();
        let multi = accts.len() > 1;
        let mut picks = column![].spacing(2);
        let mut n = 0usize;
        for acct in accts {
            let cur = state
                .cur
                .as_ref()
                .filter(|(a, _)| *a == acct)
                .map(|(_, c)| c.clone());
            for (_fa, f) in state.folders.iter().filter(|(a, _)| *a == acct) {
                if cur.as_deref() == Some(f.path.as_str()) {
                    continue;
                }
                n += 1;
                let label = if multi {
                    format!(
                        "→ {} / {}",
                        state
                            .account_names
                            .get(acct)
                            .map(|s| s.as_str())
                            .unwrap_or("?"),
                        crate::lang::folder_display(state.l(), f.kind, &f.path)
                    )
                } else {
                    format!(
                        "→ {}",
                        crate::lang::folder_display(state.l(), f.kind, &f.path)
                    )
                };
                picks = picks.push(
                    button(text(label).size(12))
                        .on_press(Message::BatchMoveDest(acct, f.path.clone()))
                        .padding([2, 8])
                        .style(theme::ghost_button),
                );
            }
        }
        if n == 0 {
            picks = picks.push(
                text(state.l().no_dest)
                    .size(12)
                    .style(theme::secondary_text),
            );
        }
        col = col.push(
            container(scrollable(picks).height(110))
                .padding(4)
                .style(theme::reader_meta),
        );
    }

    // 批量删除确认（服务器永久删除，不可撤销）
    if state.pending_batch_delete {
        col = col.push(
            container(
                row![
                    text(crate::lang::f(
                        if state.cur_is_trash_folder() {
                            state.l().batch_del_q_perm
                        } else {
                            state.l().batch_del_q_move
                        },
                        &[&n],
                    ))
                    .size(12),
                    button(text(state.l().perm_delete).size(12))
                        .on_press(Message::BatchDeleteConfirm)
                        .style(button::danger)
                        .padding([2, 8]),
                    button(text(state.l().cancel).size(12))
                        .on_press(Message::BatchDeleteCancel)
                        .padding([2, 8]),
                    Space::new().width(Length::Fill),
                ]
                .spacing(8),
            )
            .padding([6, 8])
            .width(Length::Fill)
            .style(|t| theme::status_badage(t, theme::BadgeKind::Warn)),
        );
    }
    col.into()
}

// -- 侧栏小组件（从 main_view 抽出，只在此处用） --

fn text_input_search(state: &State) -> Element<'_, Message> {
    iced::widget::text_input(state.l().search_ph, &state.search_text)
        .on_input(Message::SearchInput)
        .on_submit(Message::SearchSubmit)
        .size(13)
        .width(220)
        .padding(5)
        .into()
}

fn new_folder_input<'a>(state: &'a State, name: &'a str) -> Element<'a, Message> {
    iced::widget::text_input(state.l().new_folder_ph, name)
        .on_input(Message::NewFolderInput)
        .on_submit(Message::NewFolderConfirm)
        .size(12)
        .width(Length::Fill)
        .padding(3)
        .into()
}
