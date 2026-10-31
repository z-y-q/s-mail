//! 消息处理（update 函数）
//!
//! ## 职责
//!
//! - **Message 路由**：处理所有 UI 消息并返回 Task
//! - **状态变更**：响应用户操作更新 State
//! - **命令发送**：通过 cmd_tx 向服务线程发送 Cmd
//! - **任务编排**：组装异步任务（文件选择、后台解析、网络请求）
//!
//! ## 辅助函数
//!
//! - `close_reading_if_gone`：批量删除后精确判断是否关闭阅读器
//! - `selection_by_acct`：按账号分组批量选择（路由到各服务线程）

use iced::Task;

use crate::{
    handle_event, handle_key, notify, scroll_list_to_top, ListFilter, Message, State, Theme, ROW_H,
};
use smail_core::{Cmd, Event, SessionCmd, TaggedEvent};

mod account_setup;
mod ai_ops;
mod compose_ops;
mod mail_ops;
/// 把主窗带回前台：关了就重开，开着就还原聚焦（最小化先还原）。
/// 托盘「打开 s-mail」、左键点托盘、通知点击三条路径共用。
/// 旧实现走 System Events 的进程前台化：无障碍权限缺失时静默失败，
/// 且无法让已关闭的窗口重现——「点托盘不出界面，要重启才行」的根因
fn show_main_window(state: &mut State) -> Task<Message> {
    match state.main_window {
        Some(id) => Task::batch([
            iced::window::minimize(id, false),
            iced::window::gain_focus(id),
        ]),
        None => {
            let (id, task) = iced::window::open(iced::window::Settings {
                size: iced::Size::new(1280.0, 760.0),
                min_size: Some(iced::Size::new(980.0, 560.0)),
                exit_on_close_request: false,
                ..Default::default()
            });
            state.main_window = Some(id);
            task.map(move |_| Message::MainWindowOpened(id))
        }
    }
}

pub(crate) fn update(state: &mut State, message: Message) -> Task<Message> {
    // 错误横幅 show-once：任何新的用户动作都冲掉上一条错误（滚动/主题等被动消息除外）
    if !matches!(
        message,
        Message::ListScrolled(..)
            | Message::SystemTheme(_)
            | Message::Ev(TaggedEvent {
                event: Event::Folders(_),
                ..
            })
    ) {
        state.error = None;
    }
    // 原生网页视图的屏幕级显隐不变式（设置屏/错误横幅让位；缓存幂等）
    crate::webview::sync_screen(state);
    match message {
        Message::ErrorDismiss => {
            state.error = None;
            Task::none()
        }
        Message::Ev(ev) => handle_event(state, ev),
        Message::UnifiedInboxSelected => {
            state.prefs.last_view = Some(("__unified__".into(), "__all_inbox__".into()));
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            state.nav.in_search = false;
            state.nav.viewing_unified = true;
            state.nav.search_text.clear();
            state.nav.cur = Some((0, "__all_inbox__".into()));
            state.rows.clear();
            state.unified_contrib.clear();
            state.batch.selection.clear();
            state.nav.list_offset = 0.0;
            state.reading = None;
            state.selected = None; // 残留 selected 会让快捷键作用于不可见邮件
            state.body_req = None;
            state.batch.move_picker = None;
            state.pending_delete = None;
            state.batch.move_open = false;
            state.batch.pending_delete = false;
            state.rebuild_display();
            state.nav.load_more_pending = false;
            state.nav.load_more_done = false;
            state.nav.folder_loading = false; // 统一视图行来自 UnifiedRows，无 FolderLoaded
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::UnifiedInbox);
            }
            scroll_list_to_top()
        }
        Message::Tray(action) => match action {
            crate::tray::TrayAction::Open => {
                // 先把主窗带回前台（关了就重开），再处理通知跳转
                let show = show_main_window(state);
                if let Some(click) = crate::notify::take_last() {
                    return show.chain(update(state, Message::NotificationClicked(click)));
                }
                show
            }
            crate::tray::TrayAction::Quit => iced::exit(),
        },
        Message::SetListFilter(f) => {
            state.nav.list_filter = f;
            state.rebuild_display();
            Task::none()
        }
        Message::ToggleListSort => {
            state.nav.list_asc = !state.nav.list_asc;
            state.rebuild_display();
            Task::none()
        }
        Message::CycleSortKey => {
            state.nav.sort_key = state.nav.sort_key.next();
            state.rebuild_display();
            Task::none()
        }
        Message::FolderSelected(acct, folder) => {
            // 记忆上次视图（按账号名存，账号增删/改名后索引漂移不影响）
            if let Some(name) = state.account_names.get(acct).cloned() {
                state.prefs.last_view = Some((name, folder.clone()));
                crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            }
            state.nav.in_search = false;
            state.nav.viewing_unified = false;
            state.nav.search_text.clear();
            state.nav.list_offset = 0.0; // 虚拟化窗口回顶；部件本身靠 scroll_to（见下）
            state.batch.selection.clear(); // 选择不跨文件夹（批量操作按当前列表语义）
            state.batch.move_open = false;
            state.batch.pending_delete = false;
            state.batch.move_picker = None;
            state.pending_delete = None; // 确认条不跨文件夹（确认会删不可见邮件）
            state.body_req = None; // 在途正文不再属于新列表
            if folder == "__drafts__" {
                state.nav.cur = Some((acct, folder));
                state.rows.clear();
                state.selected = None;
                state.reading = None;
                state.nav.folder_loading = false;
                state.send(acct, Cmd::ListDrafts);
            } else if folder == "__outbox__" {
                state.nav.cur = Some((acct, folder));
                state.rows.clear();
                state.selected = None;
                state.reading = None;
                state.nav.folder_loading = false;
                state.send(acct, Cmd::ListOutbox);
            } else if folder == "__smart_unread__" || folder == "__smart_flagged__" {
                // 智能邮箱虚拟视图：跨账号聚合本地索引，无服务端往返
                state.nav.cur = Some((acct, folder.clone()));
                state.selected = None;
                state.reading = None;
                state.nav.folder_loading = false;
                state.smart_fill(&folder);
                state.rebuild_display();
            } else {
                state.nav.cur = Some((acct, folder.clone()));
                // 本地索引直读（离线优先的另一半）：服务线程可能正堵在长同步上
                //（重连全量拉取/IDLE 变更，命令队列几十秒不被消费），
                // 有缓存行就立即显示，FolderLoaded 回包再全量替换
                state.rows = state.local_folder_rows(acct, &folder);
                state.nav.list_offset = 0.0;
                state.reading = None;
                state.selected = None; // 快捷键/阅读器不再指向旧文件夹邮件
                state.expanded.clear(); // 展开状态不跨文件夹（hash 可能同名冲突）
                                        // 无缓存（首次进入/库未建）才显示「正在同步」；有缓存行列表即刻可用
                state.nav.folder_loading = state.rows.is_empty();
                state.send(acct, Cmd::SelectFolder(folder));
            }
            state.rebuild_display();
            state.nav.load_more_pending = false;
            state.nav.load_more_done = false;
            scroll_list_to_top()
        }
        Message::RowSelected(acct, hash) => {
            // 直通 Reader 线程：主循环堵在长同步里时正文照样秒出（缓存命中）
            state.request_body(acct, hash);
            state.body_req = Some((acct, hash));
            state.send(acct, Cmd::OpenMessage(hash));
            Task::none()
        }
        Message::ToggleThreading => {
            state.nav.threaded = !state.nav.threaded;
            state.rebuild_display();
            Task::none()
        }
        Message::ToggleUiTheme => {
            // 手动切换后不再跟随系统（Agency：用户显式选择优先）
            state.prefs.theme_follow = false;
            state.prefs.ui_theme = match state.prefs.ui_theme {
                Theme::Dark => Theme::Light,
                _ => Theme::Dark,
            };
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            Task::none()
        }
        Message::ToggleLang => {
            state.prefs.lang = state.prefs.lang.toggle();
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            // 状态/错误文案跟随：广播到全部服务线程（立即生效，无需重启）
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::SetLang(matches!(
                    state.prefs.lang,
                    crate::lang::Lang::Zh
                )));
            }
            Task::none()
        }
        // ---------------- M6 账号配置 ----------------
        Message::WindowGeometry(ev) => {
            // 只关心主窗口尺寸/位置；节流写盘（拖动时事件风暴）
            let geom = match ev {
                iced::window::Event::Resized(size) => state.prefs.win_geom.map(|g| {
                    let mut n = g;
                    n[0] = size.width;
                    n[1] = size.height;
                    n
                }),
                iced::window::Event::Moved(pos) => state.prefs.win_geom.map(|g| {
                    let mut n = g;
                    n[2] = pos.x;
                    n[3] = pos.y;
                    n
                }),
                iced::window::Event::Opened { position, size } => Some([
                    size.width,
                    size.height,
                    position.map(|p| p.x).unwrap_or(100.0),
                    position.map(|p| p.y).unwrap_or(100.0),
                ]),
                _ => None,
            };
            if let Some(g) = geom {
                state.prefs.win_geom = Some(g);
                let due = state
                    .prefs
                    .win_save_at
                    .map(|t| t.elapsed().as_secs() >= 2)
                    .unwrap_or(true);
                if due {
                    state.prefs.win_save_at = Some(std::time::Instant::now());
                    crate::lang::save_prefs(&state.data_root, &state.current_prefs());
                }
            }
            // 原样渲染子视图跟随窗口缩放（有变化才真正调系统接口）
            crate::webview::sync_bounds(state);
            Task::none()
        }
        Message::SystemTheme(mode) => {
            if state.prefs.theme_follow {
                state.prefs.ui_theme = match mode {
                    iced::theme::Mode::Dark => Theme::Dark,
                    _ => Theme::Light,
                };
            }
            Task::none()
        }
        Message::KeyPressed(key) => handle_key(state, key),
        Message::ThreadRootClicked(acct, root, latest) => {
            let e = state.expanded.entry(root).or_insert(false);
            *e = !*e;
            state.rebuild_display(); // 展开变化影响会话子行集
            state.selected = Some((acct, latest));
            state.reading = None;
            state.body_req = Some((acct, latest));
            state.request_body(acct, latest);
            state.send(acct, Cmd::OpenMessage(latest));
            Task::none()
        }
        Message::ListScrolled(y, view_h) => {
            state.nav.list_offset = y;
            // 实测校正（初始估计值只在首个滚动事件前生效）
            if (view_h - state.nav.list_view_h).abs() > 1.0 {
                state.nav.list_view_h = view_h;
            }
            // 触底翻页：行数恰为页边界（500 倍数）才可能是被截断的满页；
            // 穷尽判定靠 FolderLoaded 后行数不再增长。
            // 内容高度必须用显示模型（会话聚合/过滤后远小于 rows），
            // 否则触底条件永远达不到——会话视图翻页从不触发。
            // 过滤态不开自动翻页：显示行数≪数据行数，任何滚动都会立刻触底，
            // 连环 LoadMore 把整个文件夹抱下来（深度筛选用搜索，它是服务端执行）
            if !state.nav.in_search
                && state.nav.list_filter == ListFilter::All
                && !state.nav.load_more_pending
                && !state.nav.load_more_done
            {
                if let Some((a, f)) = state.nav.cur.clone() {
                    if f != "__drafts__" && f != "__outbox__" && f != "__all_inbox__" {
                        let n = state.rows.len();
                        let content_h = state.mail_display.len() as f32 * ROW_H;
                        if n > 0
                            && n.is_multiple_of(smail_core::UI_LIST_LIMIT as usize)
                            && !state.mail_display.is_empty()
                            && y + view_h >= content_h - 2.0 * ROW_H
                        {
                            state.nav.load_more_pending = true;
                            state.send(a, Cmd::LoadMore(f));
                        }
                    }
                }
            }
            Task::none()
        }
        Message::NotificationClicked(click) => {
            // 跳转：切文件夹（本地先行立刻出列表）→ 打开正文
            let notify::NotifyClick { acct, folder, hash } = click;
            let _ = update(state, Message::FolderSelected(acct, folder));
            state.selected = Some((acct, hash));
            state.body_req = Some((acct, hash));
            state.request_body(acct, hash);
            state.send(acct, Cmd::OpenMessage(hash));
            Task::none()
        }
        Message::SearchInput(s) => {
            state.nav.search_text = s;
            Task::none()
        }
        Message::SearchSubmit if state.account_names.is_empty() => {
            state.error = Some(state.l().err_need_account_search.into());
            Task::none()
        }
        Message::SearchSubmit => {
            let q = state.nav.search_text.trim().to_string();
            if q.is_empty() {
                return Task::none();
            }
            state.nav.in_search = true;
            state.nav.viewing_unified = false;
            state.nav.search_query = q.clone();
            state.search_contrib.clear();
            state.search_snippets.clear(); // 换词清旧片段
            state.rows.clear();
            state.rebuild_display();
            state.reading = None;
            state.selected = None; // 搜索是独立视图，残留 selected 误导快捷键
            state.body_req = None;
            // 批量状态同样不跨视图：搜索前选中的邮件不属于搜索结果，
            // 残留会让「批量移动/删除」误作用于旧列表的邮件
            state.batch.selection.clear();
            state.batch.move_open = false;
            state.batch.pending_delete = false;
            state.batch.move_picker = None;
            state.pending_delete = None;
            state.nav.list_offset = 0.0;
            state.nav.folder_loading = false; // 搜索结果走 SearchResults，无 FolderLoaded
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::SearchAll(q));
            }
            scroll_list_to_top()
        }
        Message::ManualRefresh => {
            // 只对“当前账号+当前文件夹”有意义；草稿/搜索态忽略；
            // 统一收件箱是伪文件夹——刷新动作改为重拉聚合，否则向服务线程发伪名报错
            if state.nav.viewing_unified {
                if let Some(tx) = &state.cmd_tx {
                    let _ = tx.send(SessionCmd::UnifiedInbox);
                }
                return Task::none();
            }
            if state.cur_is_smart() {
                // 智能视图：重跑本地聚合（把服务线程同步进库的最新状态收进来）
                if let Some((_, f)) = state.nav.cur.clone() {
                    state.smart_fill(&f);
                    state.rebuild_display();
                }
                return Task::none();
            }
            if let Some((a, f)) = state.nav.cur.clone() {
                if !state.nav.in_search && f != "__drafts__" {
                    state.send(a, Cmd::ForceRefresh(f));
                }
            }
            Task::none()
        }
        Message::ContactDelete(acct, id) => {
            state.send(acct, Cmd::DeleteContact(id));
            Task::none()
        }
        // ---------------- 三栏分割条拖拽 ----------------
        Message::SplitPress(kind) => {
            // 起始宽度取当前值（否则二次拖拽会从默认宽跳变）
            let start_w = match kind {
                crate::SplitKind::Sidebar => state.sidebar_w,
                crate::SplitKind::List => state.list_w,
            };
            state.split_drag = Some((kind, None, start_w));
            Task::none()
        }
        Message::SplitMoved(x) => {
            if let Some((kind, start_x, start_w)) = &mut state.split_drag {
                // 起点 x 由拖拽后的第一条 CursorMoved 填充
                let sx = match start_x {
                    Some(v) => *v,
                    None => {
                        *start_x = Some(x);
                        x
                    }
                };
                let new_w = kind.clamp_width(*start_w + (x - sx));
                match kind {
                    crate::SplitKind::Sidebar => state.sidebar_w = new_w,
                    crate::SplitKind::List => state.list_w = new_w,
                }
            }
            Task::none()
        }
        Message::SplitReleased => {
            state.split_drag = None;
            // 松手即持久化列宽（一次写盘，拖拽过程不写）
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            // 列宽变化改变阅读窗几何：原样渲染子视图跟随
            crate::webview::sync_bounds(state);
            Task::none()
        }
        #[cfg(target_os = "macos")]
        Message::HtmlHandlesReady(ns_view) => crate::webview::handles_ready(state, ns_view),
        #[cfg(target_os = "macos")]
        Message::ToggleHtmlRender(acct, hash) => crate::webview::toggle(state, acct, hash),
        // ---------------- 邮件拖拽移动（状态机见 drag.rs） ----------------
        Message::DragPress(acct, hash, click) => {
            // 从列表行取拖拽所需字段（folder/subject），登记候选
            if let Some((_, r)) = state
                .rows
                .iter()
                .find(|(a, r)| *a == acct && r.envelope_hash == hash)
            {
                state.drag = Some(crate::drag::MailDrag::new(
                    acct,
                    hash,
                    r.folder.clone(),
                    r.subject.clone(),
                    (*click).clone(),
                ));
            }
            Task::none()
        }
        Message::DragMoved(p) => {
            let Some(drag) = &mut state.drag else {
                return Task::none();
            };
            // 起点 x/y 由第一条 CursorMoved 填充（on_press 不带坐标）
            let press = match drag.press {
                Some(v) => v,
                None => {
                    drag.press = Some(p);
                    p
                }
            };
            drag.cursor = Some(p);
            drag.last_move = Some(std::time::Instant::now());
            if !drag.active && crate::drag::beyond_drag_threshold(press, p) {
                drag.active = true;
            }
            drag.hover = if drag.active {
                // 跨账号目标不算命中（移动命令按账号连接路由）
                crate::drag::target_at(&state.drag_targets.borrow(), p)
                    .filter(|t| t.acct == drag.src.acct && t.path != drag.src.folder)
            } else {
                None
            };
            Task::none()
        }
        Message::DragReleased => {
            let Some(drag) = state.drag.take() else {
                return Task::none();
            };
            if !drag.active {
                // 未超阈值：普通点击（原样转发行点击行为）
                return update(state, *drag.click);
            }
            // 成拖拽：投递到悬停目标（多选整批，单封直达）
            if let Some(t) = drag.hover {
                let multi = state.batch.selection.len() > 1
                    && state
                        .batch
                        .selection
                        .contains(&(drag.src.acct, drag.src.hash));
                if multi {
                    return update(state, Message::BatchMoveDest(drag.src.acct, t.path));
                }
                return update(
                    state,
                    Message::MoveToDest(drag.src.acct, drag.src.hash, t.path),
                );
            }
            Task::none()
        }
        Message::DragTick => {
            // 释放事件丢失兜底：进行中的拖拽静默超时即取消（真拖拽鼠标持续在动，
            // 候选态不参与——按住不动再松开仍是合法点击）
            let stale_drag = state
                .drag
                .as_ref()
                .map(|d| d.active && crate::drag::stale(d.last_move))
                .unwrap_or(false);
            if stale_drag {
                state.drag = None;
            }
            Task::none()
        }
        Message::DragCancel => {
            // 窗口失焦：拖拽语义已在别的应用/子视图上，候选与进行中一并清
            state.drag = None;
            Task::none()
        }
        // ---------------- 行右键菜单 ----------------
        Message::ContextMenuOpen(acct, hash) => {
            state.context_menu = Some((acct, hash));
            Task::none()
        }
        Message::ContextMenuClose => {
            state.context_menu = None;
            Task::none()
        }
        Message::ContextMenuPick(inner) => {
            state.context_menu = None;
            update(state, *inner)
        }
        Message::ClipboardCopy(text) => {
            state.statuses.insert(
                state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0),
                format!("已复制：{text}"),
            );
            iced::clipboard::write(text)
        }
        Message::RemindMe(acct, hash, hours) => {
            // 到期时间 = now + hours；点击通知跳回该邮件
            let due = std::time::Instant::now() + std::time::Duration::from_secs(hours * 3600);
            let (folder, subject) = state
                .rows
                .iter()
                .find(|(a, r)| *a == acct && r.envelope_hash == hash)
                .map(|(_, r)| (r.folder.clone(), r.subject.clone()))
                .unwrap_or_default();
            state.reminders.push(crate::state::MailReminder {
                due,
                click: crate::notify::NotifyClick { acct, folder, hash },
                subject,
            });
            state
                .statuses
                .insert(acct, format!("已设置提醒（{hours} 小时后）"));
            Task::none()
        }
        Message::CheckReminders => {
            // 到期的提醒发系统通知（点击跳转），从队列移除
            let now = std::time::Instant::now();
            let due: Vec<_> = state
                .reminders
                .iter()
                .filter(|r| r.due <= now)
                .cloned()
                .collect();
            for r in due {
                crate::notify::notify_mail("提醒", &r.subject, r.click.clone());
                state.reminders.retain(|x| x.click != r.click);
            }
            Task::none()
        }
        Message::ExportAccountRequest(acct) => {
            // 收集该账号全部文件夹的全部邮件（本地缓存为准）
            let folders: Vec<(usize, String)> = state
                .nav
                .folders
                .iter()
                .filter(|(a, _)| *a == acct)
                .map(|(a, f)| (*a, f.path.clone()))
                .collect();
            let mut all: Vec<(String, i64)> = vec![];
            for (fa, fp) in &folders {
                for (a, r) in state.local_folder_rows(*fa, fp) {
                    let _ = a;
                    all.push((fp.clone(), r.envelope_hash));
                }
            }
            if all.is_empty() {
                state.error = Some("本地没有可导出的邮件".into());
                return Task::none();
            }
            let _n = all.len();
            Task::perform(
                async move {
                    rfd::AsyncFileDialog::new()
                        .set_title("选择导出目录（将按文件夹建子目录）")
                        .pick_folder()
                        .await
                        .map(|f| f.path().to_path_buf())
                },
                move |dir: Option<std::path::PathBuf>| {
                    Message::ExportAccountPicked(acct, dir, all.clone())
                },
            )
        }
        Message::ExportAccountPicked(acct, Some(dir), all) => {
            let slug = state
                .accounts
                .get(acct)
                .map(|a| a.slug.clone())
                .unwrap_or_default();
            let bodies = state.data_root.join(&slug).join("bodies");
            let dir2 = dir.clone();
            let n_total = all.len();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        let mut done = 0usize;
                        for (folder, h) in &all {
                            let Ok(bytes) = std::fs::read(bodies.join(format!("{h}.eml"))) else {
                                continue;
                            };
                            let sub = dir2.join(folder.replace('/', "_"));
                            let _ = std::fs::create_dir_all(&sub);
                            let ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            let name = format!("{ts}_{h}.eml");
                            if std::fs::write(sub.join(name), &bytes).is_ok() {
                                done += 1;
                            }
                        }
                        (done, n_total)
                    })
                    .await
                    .unwrap_or((0, n_total))
                },
                move |(done, total)| Message::ExportFolderDone { done, total },
            )
        }
        Message::ExportAccountPicked(_, None, _) => Task::none(),
        Message::ToggleMuteSender(addr) => {
            // 切换静音并立即持久化（通知过滤在 NewMail 事件处）
            let now_muted = state.prefs.muted.contains(&addr);
            if now_muted {
                state.prefs.muted.retain(|a| *a != addr);
                state.statuses.insert(
                    state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0),
                    format!("已取消静音：{addr}"),
                );
            } else {
                state.prefs.muted.push(addr.clone());
                state.statuses.insert(
                    state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0),
                    format!("已静音：{addr}（新邮件不再通知）"),
                );
            }
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            Task::none()
        }
        // ---------------- 对齐 Apple Mail 邮箱菜单 ----------------
        Message::MailboxPrev | Message::MailboxNext => {
            let prev = matches!(message, Message::MailboxPrev);
            let acct = state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
            let seq = state.mailbox_cycle(acct);
            if seq.len() < 2 {
                return Task::none();
            }
            let cur = state
                .nav
                .cur
                .as_ref()
                .map(|(_, f)| f.clone())
                .unwrap_or_else(|| "INBOX".into());
            let idx = seq.iter().position(|p| *p == cur).unwrap_or(0);
            let next = if prev {
                (idx + seq.len() - 1) % seq.len()
            } else {
                (idx + 1) % seq.len()
            };
            update(state, Message::FolderSelected(acct, seq[next].clone()))
        }
        Message::MarkAllRead => {
            // 当前邮箱全部标为已读：批量已读管线复用（服务器批量 STORE）
            let hashes: Vec<i64> = state
                .rows
                .iter()
                .filter(|(a, _)| Some(*a) == state.nav.cur.as_ref().map(|(x, _)| *x))
                .map(|(_, r)| r.envelope_hash)
                .collect();
            if hashes.is_empty() {
                return Task::none();
            }
            let acct = state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
            state.send(
                acct,
                Cmd::BatchFlags {
                    hashes,
                    seen: Some(true),
                    flagged: None,
                },
            );
            Task::none()
        }
        Message::ApplyRulesFolder(acct) => {
            // 手动批量应用规则：当前真实文件夹全量重放（伪视图无文件夹语义）
            let Some((a, folder)) = state.nav.cur.clone() else {
                return Task::none();
            };
            if a != acct
                || state.nav.in_search
                || state.nav.viewing_unified
                || state.cur_is_smart()
                || folder == "__drafts__"
                || folder == "__outbox__"
            {
                return Task::none();
            }
            let hint = format!("{}…", state.l().apply_rules);
            state.statuses.insert(acct, hint);
            state.send(acct, Cmd::ApplyRules(folder));
            Task::none()
        }
        Message::ExpandAllThreads => {
            state.expand_all_threads();
            Task::none()
        }
        Message::CollapseAllThreads => {
            state.collapse_all_threads();
            Task::none()
        }
        Message::ExportFolderDone { done, total } => {
            state.export_folder = None;
            state.statuses.insert(
                state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0),
                format!("已导出 {done}/{total} 封到所选目录"),
            );
            Task::none()
        }
        Message::ContactSearchInput(s) => {
            state.contact_search = s;
            Task::none()
        }
        // ---------------- 多窗口：主窗/写信窗生命周期 ----------------
        Message::MainWindowOpened(id) => {
            state.main_window = Some(id);
            Task::none()
        }
        Message::ComposeWindowOpened(id) => {
            state.compose_window = Some(id);
            Task::none()
        }
        Message::WindowCloseRequested(id) => {
            if state.compose_window == Some(id) {
                // 红叉：自动存草稿再关（用户数据无损；与写信窗内「存草稿」同语义）
                if let Some(c) = &state.compose {
                    let acct = c.account;
                    state.send(
                        acct,
                        Cmd::SaveDraft {
                            id: c.draft_id,
                            to: c.to.clone(),
                            cc: c.cc.clone(),
                            bcc: c.bcc.clone(),
                            subject: c.subject.clone(),
                            body: c.body.text().to_string(),
                        },
                    );
                    state.statuses.insert(acct, "草稿已保存".into());
                }
                if let Some(win_id) = state.compose_window.take() {
                    state.compose = None;
                    return iced::window::close(win_id);
                }
                Task::none()
            } else if state.main_window == Some(id) {
                // 主窗红叉：只关窗，应用驻留托盘（「打开 s-mail」随时重开）。
                // 网页子视图挂在旧窗上随之失效——先丢（否则子视图指向已销毁
                // 的父窗），句柄与挂起态一并复位。
                // 旧实现关旧窗同时立刻开新窗：开窗撞上关窗回收，新窗可能
                // 不可见——托盘点了不出界面的另一个根因
                crate::webview::close(state);
                state.main_window = None;
                iced::window::close(id)
            } else {
                Task::none()
            }
        }
        m @ (Message::SetupOpen
        | Message::SetupClose
        | Message::SetupPreset(..)
        | Message::SetupField(..)
        | Message::SetupTest
        | Message::SetupTestResult(..)
        | Message::SetupSaveVerify(..)
        | Message::SetupSave
        | Message::SetupDelete(..)
        | Message::OAuthOpenUrl
        | Message::PgpEditOpen(..)
        | Message::PgpEditField(..)
        | Message::PgpEditPick(..)
        | Message::PgpEditPicked(..)
        | Message::PgpEditSave
        | Message::PgpEditCancel
        | Message::PgpEditWkd(..)
        | Message::SignatureEditOpen(..)
        | Message::SignatureEditAction(..)
        | Message::SignatureEditSave
        | Message::SignatureEditCancel
        | Message::SyncEditOpen(..)
        | Message::SyncEditInput(..)
        | Message::SyncEditSave
        | Message::SyncEditCancel
        | Message::RuleEditOpen(..)
        | Message::RuleEditField(..)
        | Message::RuleEditPredicate(..)
        | Message::RuleEditValue(..)
        | Message::RuleEditAction(..)
        | Message::RuleEditMoveDest(..)
        | Message::RuleEditAdd
        | Message::RuleEditDelete(..)
        | Message::RuleEditSave
        | Message::RuleEditCancel
        | Message::KeychainMove(..)) => account_setup::handle(state, m),
        m @ (Message::AiSummarize(..)
        | Message::AiSummaryReady { .. }
        | Message::AiSummaryClose
        | Message::AiDraftGo
        | Message::AiDraftReady { .. }
        | Message::AiDraftApply
        | Message::AiDraftDiscard
        | Message::AiInstructionInput(..)
        | Message::AiRewrite(..)
        | Message::AiDraftUndo
        | Message::AiSmartReply(..)
        | Message::AiOrganizeGo
        | Message::AiPlanReady { .. }
        | Message::AiPlanToggle(..)
        | Message::AiPlanExpand(..)
        | Message::AiPlanApply
        | Message::AiPlanCancel
        | Message::AiPrivacyAck
        | Message::AiPrivacyCancel
        | Message::AiFormOpen
        | Message::AiFormClose
        | Message::AiFormPreset(..)
        | Message::AiFormToggleEnabled(..)
        | Message::AiFormField(..)
        | Message::AiFormKeychain
        | Message::AiFormTest
        | Message::AiFormTestReady(..)
        | Message::AiFormSave) => ai_ops::handle(state, m),
        m @ (Message::ComposeNew
        | Message::ComposeFromDraft(..)
        | Message::ComposeClose
        | Message::ComposeAccountNext
        | Message::ComposeShortcut(_)
        | Message::ComposeField(..)
        | Message::ComposeBodyEdit(..)
        | Message::ComposeFormat(..)
        | Message::ComposePreviewToggle
        | Message::ComposePreviewReady(..)
        | Message::AutosaveTick
        | Message::SendClicked
        | Message::SaveDraftClicked
        | Message::DraftRowClicked(..)
        | Message::OutboxRetry(..)
        | Message::OutboxRemove(..)
        | Message::AddAttachment
        | Message::AttachmentsPicked(..)
        | Message::RemoveAttachment(..)
        | Message::ContactPicked(..)
        | Message::TogglePgpSign
        | Message::TogglePgpEncrypt
        | Message::ToggleHighPriority
        | Message::ComposeDiscardConfirm
        | Message::ComposeDiscardCancel) => compose_ops::handle(state, m),
        m @ (Message::Reply(..)
        | Message::ReplyAll(..)
        | Message::Forward(..)
        | Message::ForwardAsAttachment(..)
        | Message::ContactAddFromSender(..)
        | Message::Resend(..)
        | Message::ExportFolderRequest(..)
        | Message::ExportFolderPicked(..)
        | Message::BatchExportSelected
        | Message::BatchExportPicked(..)
        | Message::BatchExportDone { .. }
        | Message::IgnoreThread(..)
        | Message::UnignoreThread(..)
        | Message::CycleFlagColor(..)
        | Message::RedirectPrompt(..)
        | Message::RedirectToInput(..)
        | Message::RedirectNow(..)
        | Message::RedirectConfirm(..)
        | Message::RedirectCancel
        | Message::ForwardAttachmentsReady(..)
        | Message::AskDelete(..)
        | Message::CancelDelete
        | Message::DeleteMail(..)
        | Message::MoveTo(..)
        | Message::MoveToDest(..)
        | Message::ToggleSelect(..)
        | Message::SelectAll
        | Message::ClearSelection
        | Message::BatchSeen(..)
        | Message::BatchStar
        | Message::BatchMoveOpen
        | Message::BatchMoveDest(..)
        | Message::AskBatchDelete
        | Message::BatchDeleteConfirm
        | Message::BatchDeleteCancel
        | Message::NewFolderOpen(..)
        | Message::NewFolderInput(..)
        | Message::NewFolderConfirm
        | Message::NewFolderCancel
        | Message::RenameFolderOpen(..)
        | Message::RenameFolderInput(..)
        | Message::RenameFolderConfirm
        | Message::RenameFolderCancel
        | Message::AskFolderDelete(..)
        | Message::FolderDeleteConfirm
        | Message::FolderDeleteCancel
        | Message::MarkUnread(..)
        | Message::ToggleStar(..)
        | Message::ArchiveCurrent(..)
        | Message::BatchArchive
        | Message::SpamCurrent(..)
        | Message::BatchSpam
        | Message::Unsubscribe(..)
        | Message::SaveAttachment(..)
        | Message::ExportEmlRequest(..)
        | Message::ExportEmlPicked(..)
        | Message::LoadRemoteImages(..)
        | Message::MarkdownParsed { .. }
        | Message::LinkClicked(..)
        | Message::EmptyTrashRequest
        | Message::EmptyTrashConfirm
        | Message::EmptyTrashCancel) => mail_ops::handle(state, m),
    }
}

/// 批量移除后：仅当正在阅读的邮件已不在列表时才关阅读器
/// （多账号下不能因为批量了账号 A 就关掉账号 B 的阅读）
pub(crate) fn close_reading_if_gone(state: &mut State) {
    let hit = state
        .selected
        .zip(state.reading.as_ref().map(|(a, _)| *a))
        .map(|((sa, h), ra)| {
            ra == sa
                && !state
                    .rows
                    .iter()
                    .any(|(a, r)| *a == sa && r.envelope_hash == h)
        })
        .unwrap_or(false);
    if hit {
        state.reading = None;
        state.selected = None;
        state.body_req = None;
    }
}

/// 选择按账号分组（批量命令路由到各账号服务线程）
pub(crate) fn selection_by_acct(state: &State) -> Vec<(usize, Vec<i64>)> {
    let mut by: std::collections::BTreeMap<usize, Vec<i64>> = Default::default();
    for (a, h) in &state.batch.selection {
        by.entry(*a).or_default().push(*h);
    }
    by.into_iter().collect()
}
