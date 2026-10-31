//! 邮件列表主视图（从 list.rs 拆分）
//!
//! ## 职责
//!
//! - **顶栏组件**：撰写按钮、搜索框、刷新按钮、主题切换
//! - **侧栏组件**：账号/文件夹树、特殊视图入口
//! - **邮件列表渲染**：虚拟化滚动 + 行级 UI
//! - **批量操作栏**：选择复选框 + 操作按钮
//! - **小组件**：搜索输入框、新建文件夹输入框

use iced::widget::{
    button, canvas, column, container, mouse_area, row, scrollable, stack, text, Space,
};
use iced::{Element, Length};

use crate::{list, relative_date, theme, DispKind, ListFilter, Message, State};

const ROW_H: f32 = list::ROW_H;
const BUFFER: usize = 12;
const MAIL_LIST_ID: &str = list::MAIL_LIST_ID;

use list::clean_subject;
pub(crate) fn main_view(state: &State) -> Element<'_, Message> {
    // 投放目标命中表清空：本帧绘制期由 hit_target 包装组件重新登记
    //（拖放命中测试永远看当前帧几何，无跨帧残留）
    state.drag_targets.borrow_mut().clear();
    // 状态行过长会挤掉右侧按钮：逐段截断（字符级，不切中文字节）
    let clip = |s: &str| -> String {
        if s.chars().count() > 48 {
            let t: String = s.chars().take(48).collect();
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

    // 会话视图下才有意义：全部展开/折叠当前邮箱的会话。
    // 单个状态感知按钮——有已展开的会话显示「折叠全部」，否则「展开全部」；
    // 平铺/搜索/统一收件箱/草稿/发件箱视图下是死按钮，直接隐藏；
    // 且当前列表里没有任何多封会话时也隐藏（全是单封时展开/折叠无视觉效果，
    // 按钮出现却无效正是「没用」反馈的根因）
    let cur_view = state.nav.cur.as_ref().map(|(_, c)| c.as_str());
    let thread_capable = state.nav.threaded
        && !state.nav.in_search
        && !state.nav.viewing_unified
        && !matches!(
            cur_view,
            Some("__drafts__") | Some("__outbox__") | Some("__all_inbox__")
        );
    let has_multi_thread = state
        .mail_display
        .iter()
        .any(|d| matches!(&d.kind, crate::DispKind::ThreadRoot { count, .. } if *count > 1));
    let expand_collapse: Element<Message> = if thread_capable && has_multi_thread {
        let any_expanded = state.expanded.values().any(|v| *v);
        let (label, msg) = if any_expanded {
            (state.l().collapse_all, Message::CollapseAllThreads)
        } else {
            (state.l().expand_all, Message::ExpandAllThreads)
        };
        button(text(label).size(13))
            .on_press(msg)
            .padding([4, 8])
            .style(theme::ghost_button)
            .into()
    } else {
        Space::new().width(0).into()
    };
    let topbar = container(
        row![
            button(text(state.l().compose_new).size(13))
                .on_press_maybe(if state.account_names.is_empty() {
                    None
                } else {
                    Some(Message::ComposeNew)
                })
                .padding([5, 12])
                .style(theme::accent_button),
            // 应用名不进顶栏：窗口标题/程序坞已有身份，能不显示的就不显示
            button(
                // 显示当前模式（点击切换）：「会话视图 ⇄ 平铺视图」一眼自明
                text(if state.nav.threaded {
                    state.l().view_threads
                } else {
                    state.l().view_flat
                })
                .size(13)
            )
            .on_press(Message::ToggleThreading)
            .padding([4, 8])
            .style(theme::ghost_button),
            // 会话视图下才有意义：全部展开/折叠当前邮箱的会话
            expand_collapse,
            text_input_search(state),
            button(text("↻").size(13))
                .on_press(Message::ManualRefresh)
                .padding([4, 8])
                .style(theme::ghost_button),
            text(status_line.clone())
                .size(11)
                .style(theme::secondary_text),
            Space::new().width(Length::Fill),
            button(
                text(if matches!(state.prefs.ui_theme, iced::Theme::Dark) {
                    "☀"
                } else {
                    "☾"
                })
                .size(13),
            )
            .on_press(Message::ToggleUiTheme)
            .padding([4, 8])
            .style(theme::ghost_button),
            button(text(state.prefs.lang.toggle_label()).size(13))
                .on_press(Message::ToggleLang)
                .padding([4, 8])
                .style(theme::ghost_button),
            button(text(state.l().add_account).size(13))
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
        let unified_cur = state.nav.viewing_unified;
        let total_unread: u32 = state
            .nav
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
    // 智能邮箱分组：跨账号聚合的虚拟视图（本地索引直读，离线可用）
    if !state.account_names.is_empty() {
        sidebar = sidebar.push(
            text(state.l().smart_hdr)
                .size(11)
                .style(theme::secondary_text),
        );
        sidebar = sidebar.push(Space::new().height(2));
        for (key, label) in [
            ("__smart_unread__", state.l().smart_unread),
            ("__smart_flagged__", state.l().smart_flagged),
        ] {
            let smart_cur = state.nav.cur.as_ref().map(|(_, c)| c == key) == Some(true);
            sidebar = sidebar.push(
                button(text(format!("  {} {}", if smart_cur { "▸" } else { " " }, label)).size(13))
                    .on_press(Message::FolderSelected(0, key.into()))
                    .padding([3, 6])
                    .width(Length::Fill)
                    .style(move |t, st| theme::sidebar_item(t, st, smart_cur)),
            );
        }
        sidebar = sidebar.push(Space::new().height(6));
    }
    if state.account_names.is_empty() {
        sidebar = sidebar
            .push(
                text(state.l().no_account)
                    .size(13)
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
        // 文件夹按固定优先级排序（收件箱→草稿→已发送→垃圾→归档→废纸篓→其他）：
        // 服务器 LIST 的字典序无意义；本地草稿行插在收件箱之后
        let mut acct_folders: Vec<smail_core::FolderInfo> = state
            .nav
            .folders
            .iter()
            .filter(|(a, _)| *a == i)
            .map(|(_, f)| f.clone())
            .collect();
        // 标准文件夹恒定显示：清空废纸篓后本地索引无行、文件夹从列表消失，
        // 下次广播又出现——侧栏闪烁。缺失时合成规范名行（unseen=0），
        // 点击后服务端 ensure 命中已有文件夹或创建
        let defaults: [(smail_core::FolderKind, &str); 5] = [
            (smail_core::FolderKind::Inbox, "INBOX"),
            (smail_core::FolderKind::Sent, "Sent Messages"),
            (smail_core::FolderKind::Junk, "Junk"),
            (smail_core::FolderKind::Archive, "Archive"),
            (smail_core::FolderKind::Trash, "Trash"),
        ];
        for (kind, path) in defaults {
            if !acct_folders.iter().any(|f| f.kind == kind) {
                acct_folders.push(smail_core::FolderInfo {
                    path: path.to_string(),
                    kind,
                    unseen: 0,
                    total: 0,
                });
            }
        }
        acct_folders.sort_by_key(|f| {
            let kind_w = match f.kind {
                smail_core::FolderKind::Inbox => 0,
                smail_core::FolderKind::Drafts => 1,
                smail_core::FolderKind::Sent => 2,
                smail_core::FolderKind::Junk => 3,
                smail_core::FolderKind::Archive => 4,
                smail_core::FolderKind::Trash => 5,
                smail_core::FolderKind::Plain => 6,
            };
            (kind_w, f.path.clone())
        });
        // 本地草稿行插在收件箱之后（排序位次 1）；始终显示入口
        let mut drafts_pushed = false;
        let (ob_p, ob_f) = state.outbox_counts.get(&i).copied().unwrap_or((0, 0));
        if ob_p + ob_f > 0 {
            let ob_cur = state
                .nav
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
        for f in &acct_folders {
            // 服务器 Drafts 由本地草稿行代表（避免双入口双数据源）
            if f.kind == smail_core::FolderKind::Drafts {
                continue;
            }
            // 本地草稿行插在收件箱之后（固定排序位次 1）
            if f.kind == smail_core::FolderKind::Inbox && !drafts_pushed {
                drafts_pushed = true;
                let drafts_cur = state
                    .nav
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
            }
            // 层级树：按 '/' 深度缩进；特殊文件夹显示本地化名（选中/操作仍用全路径）
            let depth = f.path.matches('/').count();
            // 改名输入行取代原行（同 new_folder 的两态模式）
            if let Some((ra, rpath, rname)) = &state.folder_edit.rename_folder {
                if *ra == i && rpath == &f.path {
                    sidebar = sidebar.push(
                        row![
                            iced::widget::text_input(state.l().new_folder_ph, rname)
                                .on_input(Message::RenameFolderInput)
                                .on_submit(Message::RenameFolderConfirm)
                                .size(13)
                                .width(Length::Fill)
                                .padding(3),
                            button(text(state.l().do_rename).size(13))
                                .on_press(Message::RenameFolderConfirm)
                                .padding([3, 8])
                                .style(theme::accent_button),
                            button(text(state.l().cancel).size(13))
                                .on_press(Message::RenameFolderCancel)
                                .padding([3, 6]),
                        ]
                        .spacing(4),
                    );
                    continue;
                }
            }
            let is_cur = state
                .nav
                .cur
                .as_ref()
                .map(|(ca, c)| *ca == i && c == &f.path)
                == Some(true);
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
            // 拖拽悬停高亮：拖动邮件经过时按选中态配色（与分割拖拽互不干扰）
            let drop_hover = state
                .drag
                .as_ref()
                .filter(|d| d.active)
                .and_then(|d| d.hover.as_ref())
                .map(|t| t.acct == i && t.path == f.path)
                .unwrap_or(false);
            let folder_btn = button(
                row![
                    text(format!(
                        // 纯文字排版（Apple Mail 式）：⇩⊘▤⌫ 这类稀有字形在
                        // SF Pro 里缺字形回退渲染参差，是侧栏「丑」的主因；
                        // 层级靠缩进表达，选中态靠底色，未读靠徽标
                        "{}{}{}",
                        "  ".repeat(depth),
                        if is_cur { "▸ " } else { "" },
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
            .style(move |t, st| theme::sidebar_item(t, st, is_cur || drop_hover));
            // 拖拽投放目标登记：文件夹按钮整行都是落点
            let folder_btn = crate::drag::hit_target(
                folder_btn,
                state.drag_targets.clone(),
                crate::drag::DropTarget {
                    acct: i,
                    path: f.path.clone(),
                    label: crate::lang::folder_display(state.l(), f.kind, &f.path),
                },
            );
            // 改名/删除只给自建文件夹（Plain）。标准文件夹（收发/垃圾/废纸篓/
            // 归档）是服务器结构，行尾常驻红 ✕ 既危险又不可用——一律不显示
            let (ren, del): (Option<Element<Message>>, Option<Element<Message>>) =
                if f.kind == smail_core::FolderKind::Plain {
                    (
                        Some(
                            button(text("✎").size(11))
                                .on_press(Message::RenameFolderOpen(i, f.path.clone()))
                                .padding([2, 4])
                                .style(theme::ghost_button)
                                .into(),
                        ),
                        Some(
                            button(text("✕").size(11))
                                .on_press(Message::AskFolderDelete(i, f.path.clone()))
                                .padding([2, 4])
                                .style(theme::ghost_button)
                                .into(),
                        ),
                    )
                } else {
                    (None, None)
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
        if let Some((nf_acct, nf_name)) = &state.folder_edit.new_folder {
            if *nf_acct == i {
                sidebar = sidebar.push(
                    row![
                        new_folder_input(state, nf_name),
                        button(text(state.l().do_create).size(13))
                            .on_press(Message::NewFolderConfirm)
                            .padding([3, 8])
                            .style(theme::accent_button),
                        button(text(state.l().cancel).size(13))
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
    let sidebar = container(scrollable(sidebar).direction(theme::scroller()))
        .width(state.sidebar_w)
        .height(Length::Fill)
        .style(theme::sidebar_bg);
    // 拖拽分割条：按住左右拖调列宽（全局鼠标流跟踪，松开持久化）
    let split_sidebar = split_handle(crate::SplitKind::Sidebar);
    let split_list = split_handle(crate::SplitKind::List);

    // 列表
    let cur_name = if state.nav.viewing_unified {
        crate::lang::f(state.l().unified_hdr, &[&state.rows.len()])
    } else if state.nav.in_search {
        crate::lang::f(
            state.l().search_hdr,
            &[&state.nav.search_query, &state.rows.len()],
        )
    } else {
        match &state.nav.cur {
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
            Some((_, f)) if f == "__smart_unread__" => {
                format!("{} / {}", state.l().smart_hdr, state.l().smart_unread)
            }
            Some((_, f)) if f == "__smart_flagged__" => {
                format!("{} / {}", state.l().smart_hdr, state.l().smart_flagged)
            }
            Some((a, f)) => {
                // 列表头也显示本地化文件夹名（与侧栏一致）
                let fname = state
                    .nav
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

    let is_drafts = !state.nav.in_search
        && state.nav.cur.as_ref().map(|(_, c)| c == "__drafts__") == Some(true);
    let is_outbox = !state.nav.in_search
        && state.nav.cur.as_ref().map(|(_, c)| c == "__outbox__") == Some(true);
    let cur_acct = state.nav.cur.as_ref().map(|(a, _)| *a);

    // 过滤/排序条（文件夹与统一收件箱视图；草稿/搜索不加）。
    // 判据用底层数据 rows 而非显示结果——过滤后空结果时控件必须仍在，
    // 否则用户被困在空过滤里（曾发生：过滤★无结果→条消失→无法切回全部）
    let filter_bar: Option<Element<Message>> =
        if !is_drafts && !state.nav.in_search && !state.rows.is_empty() {
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
            let sort_label = if state.nav.list_asc {
                state.l().sort_asc
            } else {
                state.l().sort_desc
            };
            // 维度标签：日期/发件人/主题（点击循环切换维度）
            let key_label = state
                .nav
                .sort_key
                .label(state.prefs.lang == crate::lang::Lang::Zh);
            Some(
                row![
                    mk(
                        state.l().filter_all.to_string(),
                        ListFilter::All,
                        state.nav.list_filter == ListFilter::All
                    ),
                    mk(
                        state.l().filter_unread.to_string(),
                        ListFilter::Unread,
                        state.nav.list_filter == ListFilter::Unread
                    ),
                    mk(
                        state.l().filter_starred.to_string(),
                        ListFilter::Flagged,
                        state.nav.list_filter == ListFilter::Flagged
                    ),
                    Space::new().width(Length::Fill),
                    button(text(state.l().mark_all_read).size(11))
                        .on_press(Message::MarkAllRead)
                        .padding([2, 8])
                        .style(theme::ghost_button),
                    // 手动批量应用规则（真实文件夹视图；存量回放入口）。
                    // 统一收件箱/智能视图是聚合伪视图，无文件夹语义
                    (!state.nav.viewing_unified && !state.cur_is_smart()).then(|| {
                        button(text(state.l().apply_rules).size(11))
                            .on_press_maybe(cur_acct.map(Message::ApplyRulesFolder))
                            .padding([2, 8])
                            .style(theme::ghost_button)
                    }),
                    button(text(format!("{key_label} {sort_label}")).size(11))
                        .on_press(Message::CycleSortKey)
                        .padding([2, 8])
                        .style(theme::ghost_button),
                    button(text(if state.nav.list_asc { "↓" } else { "↑" }).size(11))
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
    let first = (((state.nav.list_offset.min(max_off)) / ROW_H) as usize).saturating_sub(BUFFER);
    let visible = (state.nav.list_view_h / ROW_H).ceil() as usize + BUFFER * 2;
    let last = (first + visible).min(n_entries);
    let top_pad = first as f32 * ROW_H;
    let bottom_pad = (n_entries - last) as f32 * ROW_H;

    let mut list = column![
        text(cur_name).size(13),
        Space::new()
            .width(Length::Fill)
            .height(Length::Fixed(top_pad)),
    ];
    if let Some(fb) = filter_bar {
        list = list.push(fb);
    }
    // 废纸篓/垃圾视图：整箱清空入口（含确认条——不可恢复操作）
    let cur_junk = !is_drafts && !is_outbox && state.cur_is_junk_folder();
    if !is_drafts
        && !is_outbox
        && (state.cur_is_trash_folder() || cur_junk)
        && !state.rows.is_empty()
    {
        let (empty_label, empty_q) = if cur_junk {
            (state.l().empty_junk, state.l().empty_junk_q)
        } else {
            (state.l().empty_trash, state.l().empty_trash_q)
        };
        if state.pending_empty_trash {
            let n = state.rows.len();
            list = list.push(
                container(
                    row![
                        text(crate::lang::f(empty_q, &[&n])).size(13),
                        button(text(state.l().perm_delete).size(13))
                            .on_press(Message::EmptyTrashConfirm)
                            .style(button::danger)
                            .padding([2, 8]),
                        button(text(state.l().cancel).size(13))
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
                button(text(empty_label).size(11))
                    .on_press(Message::EmptyTrashRequest)
                    .padding([2, 8])
                    .style(button::danger),
            );
        }
    }

    // M7：文件夹删除确认条（破坏性操作，列表顶部醒目确认）
    if let Some((_fa, fpath)) = &state.folder_edit.pending_delete {
        let fpath = fpath.clone();
        list = list.push(
            container(
                row![
                    text(crate::lang::f(state.l().folder_del_q, &[&fpath])).size(13),
                    button(text(state.l().perm_delete).size(13))
                        .on_press(Message::FolderDeleteConfirm)
                        .style(button::danger)
                        .padding([2, 8]),
                    button(text("取消").size(13))
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
    if !is_drafts && !state.batch.selection.is_empty() {
        list = list.push(batch_bar(state));
    }
    // AI 整理计划预览（AI 只出建议；「应用」是人工点击，走与手工批量
    // 完全相同的命令路径——含乐观移除与后台搬运）
    if state.ai.plan_loading {
        list = list.push(
            container(text(state.l().ai_plan_loading).size(13))
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
                        .size(13),
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
                button(text(crate::lang::f(state.l().ai_plan_apply, &[&n_checked])).size(13))
                    .on_press(Message::AiPlanApply)
                    .padding([3, 10])
                    .style(theme::accent_button),
                button(text(state.l().cancel).size(13))
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
                        .size(11)
                        .style(theme::secondary_text),
                ]
                .spacing(2),
            );
        }
    } else {
        for d in &mail_display[first.min(mail_display.len())..last.min(mail_display.len())] {
            let (acct, r) = (d.acct, &d.row);
            let is_sel = state.selected == Some((acct, r.envelope_hash));
            let tag = if state.nav.viewing_unified {
                let acct_name = state
                    .account_names
                    .get(acct)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("  [{}]", acct_name)
            } else if state.nav.in_search {
                let acct_name = state
                    .account_names
                    .get(acct)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                // 文件夹名本地化（与侧栏一致，不再显示 INBOX 等服务器原名）
                let fname = state
                    .nav
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
                            .size(13)
                            .style(|_t| text::Style {
                                color: Some(theme::star_color()),
                            })
                            .into()
                    } else {
                        text("").size(13).into()
                    };
                    // 会话根的选择框挂在最新一封（批量操作对象直觉是所见最新信）
                    let cb = select_checkbox(state, acct, *latest);
                    // 会话根第三行：最新正文预览（与平铺行一致，Mail.app 式）
                    let thread_preview: String = {
                        let mut p: String = r.preview.chars().take(60).collect();
                        if r.preview.chars().count() > 60 {
                            p.push('…');
                        }
                        p
                    };
                    let row_el: Element<Message> = row![
                        cb,
                        button(
                            column![
                                row![
                                    star_txt,
                                    // 展开箭头只给多封会话（单封画箭头纯属噪音）
                                    text(format!(
                                        "{} {}{badge}",
                                        if *count > 1 {
                                            if expanded_now {
                                                "▾"
                                            } else {
                                                "▸"
                                            }
                                        } else {
                                            " "
                                        },
                                        if *unseen { "●" } else { "" },
                                    ))
                                    .size(13),
                                    text(clean_subject(&r.subject))
                                        .size(13)
                                        .font(theme::ui_font(if *unseen {
                                            iced::font::Weight::Semibold
                                        } else {
                                            iced::font::Weight::Normal
                                        })),
                                    Space::new().width(Length::Fill),
                                    text(relative_date(*last_date))
                                        .size(11)
                                        .style(theme::secondary_text),
                                ],
                                text(format!("{last_from}{tag}"))
                                    .size(11)
                                    .style(theme::secondary_text),
                                (!thread_preview.is_empty()).then(|| {
                                    text(thread_preview).size(11).style(theme::secondary_text)
                                }),
                            ]
                            .spacing(2),
                        )
                        .padding([6, 8])
                        .width(Length::Fill)
                        .style(move |t, st| theme::list_row(t, st, is_sel)),
                    ]
                    .spacing(2)
                    .into();
                    // 点击语义走 wrap_row_menu 的按下登记（松开还原），button 不再挂点击
                    list = list.push(wrap_row_menu(
                        row_el,
                        acct,
                        r.envelope_hash,
                        Message::ThreadRootClicked(acct, r.envelope_hash, *latest),
                    ));
                    if state.context_menu == Some((acct, r.envelope_hash)) {
                        list = list.push(context_menu_row(&crate::list_view::MenuCtx {
                            state,
                            acct,
                            hash: r.envelope_hash,
                            flagged: r.flagged,
                            from_addr: &r.from_addr,
                            subject: &r.subject,
                            folder_kind: smail_core::folder_kind(&r.folder),
                            folder_path: &r.folder,
                        }));
                    }
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
                    // 未读子行：圆点 + 粗体（展开的会话里一眼找到未读那封）
                    let child_unseen = !r.seen;
                    let subject_line =
                        format!("{}↳ {}", if child_unseen { "●" } else { "" }, r.subject);
                    let cb = select_checkbox(state, acct, r.envelope_hash);
                    let row_el: Element<Message> = row![
                        cb,
                        button(
                            column![
                                row![
                                    star_txt,
                                    text(subject_line).size(13).font(theme::ui_font(
                                        if child_unseen {
                                            iced::font::Weight::Semibold
                                        } else {
                                            iced::font::Weight::Normal
                                        }
                                    )),
                                    Space::new().width(Length::Fill),
                                    text(relative_date(r.date_unix))
                                        .size(11)
                                        .style(theme::secondary_text),
                                ],
                                text(format!("   {}", r.from_addr))
                                    .size(11)
                                    .style(theme::secondary_text),
                            ]
                            .spacing(1),
                        )
                        .padding([4, 10])
                        .width(Length::Fill)
                        .style(move |t, st| theme::list_row(t, st, is_sel)),
                    ]
                    .spacing(2)
                    .into();
                    list = list.push(wrap_row_menu(
                        row_el,
                        acct,
                        r.envelope_hash,
                        Message::RowSelected(acct, r.envelope_hash),
                    ));
                    if state.context_menu == Some((acct, r.envelope_hash)) {
                        list = list.push(context_menu_row(&crate::list_view::MenuCtx {
                            state,
                            acct,
                            hash: r.envelope_hash,
                            flagged: r.flagged,
                            from_addr: &r.from_addr,
                            subject: &r.subject,
                            folder_kind: smail_core::folder_kind(&r.folder),
                            folder_path: &r.folder,
                        }));
                    }
                }
                DispKind::Flat => {
                    let dot = if r.seen { "" } else { "●" };
                    let star_txt: Element<Message> = if r.flagged {
                        text("★")
                            .size(13)
                            .style(|_t| text::Style {
                                color: Some(theme::star_color()),
                            })
                            .into()
                    } else {
                        text("").size(13).into()
                    };
                    let subj_weight = if r.seen {
                        iced::font::Weight::Normal
                    } else {
                        iced::font::Weight::Semibold
                    };
                    let cb = select_checkbox(state, acct, r.envelope_hash);
                    // Mail.app 式三行布局：发件人(粗)+日期 / 主题 / 正文预览。
                    // 预览取正文前 ~48 字（正文索引后回填，未索引的行留空）
                    let from_name = crate::from_display_name(&r.from_addr);
                    let preview: String = {
                        let mut p: String = r.preview.chars().take(60).collect();
                        if r.preview.chars().count() > 60 {
                            p.push('…');
                        }
                        p
                    };
                    let search_line: Option<Element<Message>> = if state.nav.in_search {
                        state.search_snippets.get(&r.envelope_hash).map(|snip| {
                            Element::from(
                                text(format!("  「{snip}」"))
                                    .size(11)
                                    .style(theme::secondary_text),
                            )
                        })
                    } else {
                        None
                    };
                    let mut lines = column![
                        row![
                            star_txt,
                            text(from_name.clone())
                                .size(13)
                                .font(theme::ui_font(iced::font::Weight::Semibold)),
                            Space::new().width(Length::Fill),
                            text(relative_date(r.date_unix))
                                .size(11)
                                .style(theme::secondary_text),
                        ],
                        row![text(format!("{dot}{}", r.subject))
                            .size(13)
                            .font(theme::ui_font(subj_weight)),],
                    ]
                    .spacing(2);
                    // 第三行优先级：搜索片段 > 正文预览 > 收件标签
                    if let Some(snip) = search_line {
                        lines = lines.push(snip);
                    } else if !preview.is_empty() {
                        lines = lines.push(text(preview).size(11).style(theme::secondary_text));
                    } else if !tag.is_empty() {
                        lines = lines.push(
                            text(tag.trim().to_string())
                                .size(11)
                                .style(theme::secondary_text),
                        );
                    }
                    let row_el: Element<Message> = row![
                        cb,
                        button(container(lines).width(Length::Fill).padding([4, 0]),)
                            .padding([6, 8])
                            .width(Length::Fill)
                            .style(move |t, st| theme::list_row(t, st, is_sel)),
                    ]
                    .spacing(2)
                    .into();
                    list = list.push(wrap_row_menu(
                        row_el,
                        acct,
                        r.envelope_hash,
                        Message::RowSelected(acct, r.envelope_hash),
                    ));
                    if state.context_menu == Some((acct, r.envelope_hash)) {
                        list = list.push(context_menu_row(&crate::list_view::MenuCtx {
                            state,
                            acct,
                            hash: r.envelope_hash,
                            flagged: r.flagged,
                            from_addr: &r.from_addr,
                            subject: &r.subject,
                            folder_kind: smail_core::folder_kind(&r.folder),
                            folder_path: &r.folder,
                        }));
                    }
                }
            }
        }
    }
    if !is_drafts && mail_display.is_empty() && !state.nav.in_search {
        // 空态三分：账号缺失 / 同步在途 / 数据被过滤；
        // 列表真空了但服务器报着未读 → 给「重新同步」出口（否则徽标和
        // 空列表互相打脸，用户不知道信在哪）
        let cur_unseen: u32 = match &state.nav.cur {
            Some((a, f)) => state
                .nav
                .folders
                .iter()
                .filter(|(fa, fi)| fa == a && &fi.path == f)
                .map(|(_, fi)| fi.unseen)
                .sum(),
            None => 0,
        };
        let (icon, msg): (&str, String) = if state.account_names.is_empty() {
            ("✉", state.l().empty_add_account.into())
        } else if state.nav.folder_loading {
            ("⟳", state.l().empty_syncing.into())
        } else if !state.rows.is_empty() {
            // 底层有数据但显示为空 = 被过滤掉了，提示切回而非误导“没有邮件”
            let f = match state.nav.list_filter {
                ListFilter::All => "",
                ListFilter::Unread => state.l().filter_unread,
                ListFilter::Flagged => state.l().filter_starred,
            };
            ("∅", crate::lang::f(state.l().empty_no_match, &[&f]))
        } else if cur_unseen > 0 {
            ("∅", crate::lang::f(state.l().empty_stale, &[&cur_unseen]))
        } else {
            ("✉", state.l().empty_no_mail.into())
        };
        let stale = !state.account_names.is_empty()
            && !state.nav.folder_loading
            && state.rows.is_empty()
            && cur_unseen > 0;
        let mut empty_col = column![
            text(icon).size(28).style(theme::secondary_text),
            text(msg).size(13).style(theme::secondary_text),
        ]
        .spacing(6)
        .align_x(iced::alignment::Alignment::Center);
        if stale {
            empty_col = empty_col.push(
                button(text(state.l().resync).size(13))
                    .on_press(Message::ManualRefresh)
                    .padding([3, 10])
                    .style(theme::outline_button),
            );
        }
        list = list.push(
            container(empty_col)
                .width(Length::Fill)
                .padding([24, 0])
                .center(Length::Fill),
        );
    }
    // 满页提示：未到底提示可继续滚动加载；到底后给出总数确认
    if !is_drafts && state.rows.len() >= smail_core::UI_LIST_LIMIT as usize {
        let msg = if state.nav.viewing_unified {
            crate::lang::f(state.l().unified_cap, &[&state.rows.len()])
        } else if state.nav.load_more_done {
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
            .direction(theme::scroller())
            .id(iced::widget::Id::new(MAIL_LIST_ID))
            .height(Length::Fill)
            .on_scroll(|vp: scrollable::Viewport| {
                Message::ListScrolled(vp.absolute_offset().y, vp.bounds().height)
            }),
    )
    .width(state.list_w);

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
                    button(text(state.l().oauth_open).size(13))
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
                    text(state.l().conn_lost).size(13),
                    text(disconnected.join("、")).size(13),
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
                    .size(13)
                    .style(theme::secondary_text),
                    Space::new().width(Length::Fill),
                    button(text(state.l().ai_privacy_go).size(13))
                        .on_press(Message::AiPrivacyAck)
                        .padding([3, 10])
                        .style(theme::accent_button),
                    button(text(state.l().cancel).size(13))
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
    // 拖拽幽灵浮层盖在整幅主界面上（非拖拽帧 canvas 程序不画任何东西）
    let ghost = {
        let drag = state.drag.as_ref().filter(|d| d.active);
        let (at, subject, dest, in_selection) = match drag {
            Some(d) => (
                d.cursor,
                d.src.subject.clone(),
                d.hover.as_ref().map(|t| t.label.clone()),
                state.batch.selection.contains(&(d.src.acct, d.src.hash)),
            ),
            None => (None, String::new(), None, false),
        };
        let n_extra = if in_selection {
            state
                .batch
                .selection
                .iter()
                .filter(|(a, _)| *a == cur_acct.unwrap_or(usize::MAX))
                .count()
                .saturating_sub(1)
        } else {
            0
        };
        canvas(crate::drag::GhostChip {
            at,
            subject,
            n_extra,
            dest,
            dark: matches!(state.prefs.ui_theme, iced::Theme::Dark),
        })
        .width(Length::Fill)
        .height(Length::Fill)
    };
    stack![
        column![
            root,
            row![sidebar, split_sidebar, mail_list, split_list, reader]
        ]
        .spacing(1)
        .height(Length::Fill),
        ghost,
    ]
    .into()
}

/// 拖拽分割条：8px 可抓取 + 悬停变左右箭头光标（macOS 分栏拖拽的
/// 必备可供性——此前 5px 无光标提示，用户既找不到也想不到能拖）
fn split_handle(kind: crate::SplitKind) -> Element<'static, Message> {
    mouse_area(
        container(Space::new().width(Length::Fill))
            .width(8)
            .height(Length::Fill)
            .style(|t| container::Style {
                background: Some(theme::hover_tint(t).into()),
                ..Default::default()
            }),
    )
    .on_press(Message::SplitPress(kind))
    .interaction(iced::mouse::Interaction::ResizingHorizontally)
    .into()
}

/// 行右键包裹：右键打开该行的上下文菜单（左键行为不受影响）
/// 行包装：左键按下登记拖拽候选（iced button 在松手瞬间才发 on_press，
/// 拖拽手势的「按下」必须走 mouse_area——分割条同款时机）；右键开菜单。
/// 未成拖拽时松开还原为 click（候选携带的原点击消息）
fn wrap_row_menu<'a>(
    row_el: impl Into<Element<'a, Message>>,
    acct: usize,
    hash: i64,
    click: Message,
) -> Element<'a, Message> {
    mouse_area(row_el.into())
        .on_press(Message::DragPress(acct, hash, Box::new(click)))
        .on_right_press(Message::ContextMenuOpen(acct, hash))
        .into()
}

/// 右键菜单行上下文（8 个散参聚合，避开 clippy 参数数门禁）
struct MenuCtx<'a> {
    state: &'a State,
    acct: usize,
    hash: i64,
    flagged: bool,
    from_addr: &'a str,
    subject: &'a str,
    folder_kind: smail_core::FolderKind,
    folder_path: &'a str,
}

/// 右键菜单行：动作与阅读页操作条同源（点击后关菜单再转发）。
/// 发件人相关（复制地址/加入通讯录/附件转发/静音）对齐 Apple Mail 邮件菜单；
/// 「再次发送」仅在已发送文件夹显示
fn context_menu_row(c: &MenuCtx<'_>) -> Element<'static, Message> {
    // 解构到局部名：函数体保持既有写法不动
    let state = c.state;
    let acct = c.acct;
    let hash = c.hash;
    let flagged = c.flagged;
    let from_addr = c.from_addr;
    let subject = c.subject;
    let folder_kind = c.folder_kind;
    let folder_path = c.folder_path;
    let item = |label: String, msg: Message| -> Element<'static, Message> {
        button(container(text(label).size(13)).width(Length::Fill))
            .on_press(Message::ContextMenuPick(Box::new(msg)))
            .padding([5, 12])
            .width(Length::Fill)
            .style(theme::ghost_button)
            .into()
    };
    let star_label = if flagged {
        "★ 取消星标".to_string()
    } else {
        "☆ 星标".to_string()
    };
    let sender_addr = crate::addr_of(from_addr);
    let is_sent = folder_kind == smail_core::FolderKind::Sent;
    let muted = state.prefs.muted.contains(&sender_addr);
    let mute_label = if muted {
        format!("取消静音（{sender_addr}）")
    } else {
        format!("静音此发件人（{sender_addr}）")
    };
    let cur_folder = state.nav.cur.as_ref().map(|(_, f)| f.clone());
    let subject_key = crate::subject_key(subject);
    let thread_ignored = state.prefs.ignored.iter().any(|i| i == &subject_key);
    container(
        column![
            item("↩ 回复".into(), Message::Reply(acct, hash)),
            item("↩↩ 全部回复".into(), Message::ReplyAll(acct, hash)),
            item("➦ 转发".into(), Message::Forward(acct, hash)),
            item(
                "作为附件转发".into(),
                Message::ForwardAsAttachment(acct, hash)
            ),
            // 代转：原信原样重投（Resent-*），正文/头不动
            item(
                state.l().redirect_menu.into(),
                Message::RedirectPrompt(acct, hash)
            ),
            // 忽略/取消忽略此会话（按规范主题键）
            item(
                if thread_ignored {
                    "取消忽略此会话".into()
                } else {
                    "忽略此会话".into()
                },
                if thread_ignored {
                    Message::UnignoreThread(subject_key)
                } else {
                    Message::IgnoreThread(subject_key)
                }
            ),
            (is_sent).then(|| item("再次发送".into(), Message::Resend(acct, hash))),
            item(star_label, Message::ToggleStar(acct, hash, !flagged)),
            item("标为未读".into(), Message::MarkUnread(acct, hash)),
            item("移动到…".into(), Message::MoveTo(acct, hash)),
            item("归档".into(), Message::ArchiveCurrent(acct, hash)),
            item("标为垃圾".into(), Message::SpamCurrent(acct, hash)),
            item("删除".into(), Message::AskDelete(acct, hash)),
            item(
                format!("复制发件人地址（{sender_addr}）"),
                Message::ClipboardCopy(sender_addr.clone()),
            ),
            item(
                "将发件人加入通讯录".into(),
                Message::ContactAddFromSender(acct, hash),
            ),
            item(mute_label, Message::ToggleMuteSender(sender_addr.clone()),),
            item(
                "提醒我（1 小时后）".into(),
                Message::RemindMe(acct, hash, 1),
            ),
            (cur_folder.as_deref() == Some(folder_path)).then(|| {
                item(
                    format!("导出此文件夹（{} 封）", state.rows.len()),
                    Message::ExportFolderRequest(acct, folder_path.to_string()),
                )
            }),
            item("关闭菜单".into(), Message::ContextMenuClose),
        ]
        .spacing(1)
        .padding(4),
    )
    .width(230)
    .style(theme::context_menu)
    .into()
}

// ---------------------------------------------------------------------------
// M7 辅助：选择框 / 批量操作条
// ---------------------------------------------------------------------------

fn select_checkbox(state: &State, acct: usize, hash: i64) -> Element<'_, Message> {
    let on = state.batch.selection.contains(&(acct, hash));
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
    let n = state.batch.selection.len();
    // 两行分组（单行塞 11 个按钮，窄列表必溢出竖排文字）：
    // 第一行「计数 + 选择态」，第二行「动作」
    let bar = column![
        row![
            text(crate::lang::f(state.l().selected_n, &[&n])).size(13),
            Space::new().width(Length::Fill),
            button(text(state.l().clear).size(13))
                .on_press(Message::ClearSelection)
                .padding([2, 8])
                .style(theme::ghost_button),
        ]
        .spacing(4),
        row![
            button(text(state.l().select_all).size(13))
                .on_press(Message::SelectAll)
                .padding([2, 8])
                .style(theme::ghost_button),
            button(text(state.l().mark_read).size(13))
                .on_press(Message::BatchSeen(true))
                .padding([2, 8])
                .style(theme::ghost_button),
            button(text(state.l().mark_unread).size(13))
                .on_press(Message::BatchSeen(false))
                .padding([2, 8])
                .style(theme::ghost_button),
            button(text(state.l().star).size(13))
                .on_press(Message::BatchStar)
                .padding([2, 8])
                .style(theme::ghost_button),
            Space::new().width(Length::Fill),
        ]
        .spacing(4),
        row![
            button(text(state.l().batch_move).size(13))
                .on_press(Message::BatchMoveOpen)
                .padding([2, 8])
                .style(theme::ghost_button),
            button(text(state.l().batch_archive).size(13))
                .on_press(Message::BatchArchive)
                .padding([2, 8])
                .style(theme::ghost_button),
            button(text(state.l().batch_spam).size(13))
                .on_press(Message::BatchSpam)
                .padding([2, 8])
                .style(theme::ghost_button),
            button(text(state.l().batch_delete).size(13))
                .on_press(Message::AskBatchDelete)
                .padding([2, 8])
                .style(theme::ghost_button),
            // 批量导出勾选邮件为 .eml（目录选择后后台写出）
            button(text(state.l().batch_export).size(13))
                .on_press(Message::BatchExportSelected)
                .padding([2, 8])
                .style(theme::ghost_button),
            Space::new().width(Length::Fill),
            button(
                text(if state.ai.busy == Some(crate::ai::BusyKind::Organize) {
                    state.l().ai_working
                } else {
                    state.l().ai_organize_btn
                })
                .size(13),
            )
            .on_press_maybe(if state.ai.busy.is_some() {
                None
            } else {
                Some(Message::AiOrganizeGo)
            })
            .padding([2, 8])
            .style(theme::ghost_button),
        ]
        .spacing(4),
    ]
    .spacing(4);
    let mut col = column![container(bar).padding([4, 6]).style(theme::reader_meta)].spacing(2);

    // 移动目标列表：选择涉及的各账号，分组列出（排除各自当前文件夹）
    if state.batch.move_open {
        let mut accts: Vec<usize> = state.batch.selection.iter().map(|(a, _)| *a).collect();
        accts.sort_unstable();
        accts.dedup();
        let multi = accts.len() > 1;
        let mut picks = column![].spacing(2);
        let mut n = 0usize;
        for acct in accts {
            let cur = state
                .nav
                .cur
                .as_ref()
                .filter(|(a, _)| *a == acct)
                .map(|(_, c)| c.clone());
            for (_fa, f) in state.nav.folders.iter().filter(|(a, _)| *a == acct) {
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
                    button(text(label).size(13))
                        .on_press(Message::BatchMoveDest(acct, f.path.clone()))
                        .padding([2, 8])
                        .style(theme::ghost_button),
                );
            }
        }
        if n == 0 {
            picks = picks.push(
                text(state.l().no_dest)
                    .size(13)
                    .style(theme::secondary_text),
            );
        }
        col = col.push(
            container(scrollable(picks).direction(theme::scroller()).height(110))
                .padding(4)
                .style(theme::reader_meta),
        );
    }

    // 批量删除确认（服务器永久删除，不可撤销）
    if state.batch.pending_delete {
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
                    .size(13),
                    button(text(state.l().perm_delete).size(13))
                        .on_press(Message::BatchDeleteConfirm)
                        .style(button::danger)
                        .padding([2, 8]),
                    button(text(state.l().cancel).size(13))
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
    iced::widget::text_input(state.l().search_ph, &state.nav.search_text)
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
        .size(13)
        .width(Length::Fill)
        .padding(3)
        .into()
}
