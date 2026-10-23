//! 键盘导航与快捷键（Esc 层级链、↑↓ 选信、字母键动作、F5）（从 main.rs 拆出）。
use iced::keyboard::Key;
use iced::Task;

use crate::list::ROW_H;
use crate::{list, update, Message, Screen, State};
use smail_core::{Cmd, SessionCmd};

pub(crate) fn handle_key(state: &mut State, key: iced::keyboard::Key) -> Task<Message> {
    match key {
        Key::Named(iced::keyboard::key::Named::Escape) => {
            if let Screen::Compose(c) = &mut state.screen {
                let has_content = !c.to.is_empty()
                    || !c.subject.is_empty()
                    || !c.body.text().trim().is_empty()
                    || !c.attachments.is_empty();
                if has_content && !c.confirm_discard {
                    c.confirm_discard = true; // 先确认再丢
                } else {
                    state.screen = Screen::Mail;
                }
            } else if let Screen::Setup(f) = &mut state.screen {
                // 表单有内容时 Esc 不直接关屏（清输入框的习惯性 Esc 会丢整张表单）；
                // 首次提示，再按一次才退出，任意编辑动作重置
                let dirty = !f.email.is_empty()
                    || !f.name.is_empty()
                    || !f.password.is_empty()
                    || !f.imap_host.is_empty()
                    || !f.smtp_host.is_empty();
                if dirty && !f.esc_guard {
                    f.esc_guard = true;
                } else {
                    state.screen = Screen::Mail;
                }
            } else if state.pending_empty_trash {
                state.pending_empty_trash = false;
            } else if state.pending_delete.is_some() {
                state.pending_delete = None;
            } else if state.batch_move_open
                || state.pending_batch_delete
                || state.move_picker.is_some()
            {
                // 层级退出也要能关批量面板（否则无鼠标路径之外被困）
                state.batch_move_open = false;
                state.pending_batch_delete = false;
                state.move_picker = None;
            } else if state.new_folder.is_some() || state.rename_folder.is_some() {
                // 先关侧栏内联输入（新建/改名），不打断当前浏览
                state.new_folder = None;
                state.rename_folder = None;
            } else if state.nav.in_search {
                state.nav.in_search = false;
                state.rows.clear();
                state.rebuild_display(); // 空档期不再显示旧搜索结果（FolderLoaded 回来前）
                state.nav.list_offset = 0.0;
                // 从统一收件箱进入的搜索：退回统一视图，不向服务线程发伪文件夹名
                // （SelectFolder("__all_inbox__") 会报“不在服务器列表”且列表变空）
                let cur_folder = state.nav.cur.clone().map(|(_, f)| f);
                if state.nav.viewing_unified || cur_folder.as_deref() == Some("__all_inbox__") {
                    state.nav.viewing_unified = true;
                    state.nav.cur = Some((0, "__all_inbox__".into()));
                    if let Some(tx) = &state.cmd_tx {
                        let _ = tx.send(SessionCmd::UnifiedInbox);
                    }
                    return Task::none();
                }
                if let Some((a, f)) = state.nav.cur.clone() {
                    // 草稿箱是伪文件夹（本地表），SelectFolder 伪名会被服务器拒；
                    // 重新拉草稿列表即可
                    if f == "__drafts__" {
                        state.body_req = None;
                        state.send(a, Cmd::ListDrafts);
                        return Task::none();
                    }
                    state.body_req = None;
                    state.send(a, Cmd::SelectFolder(f));
                }
                return scroll_list_to_top();
            } else if state.nav.viewing_unified {
                // Esc 也退出统一收件箱（单账号删到入口消失时的唯一出口）
                state.nav.viewing_unified = false;
                state.nav.cur = None;
                state.rows.clear();
                state.body_req = None;
                state.rebuild_display();
                return Task::none();
            } else if !state.nav.search_text.is_empty() {
                state.nav.search_text.clear();
            }
            Task::none()
        }
        Key::Named(
            named @ (iced::keyboard::key::Named::ArrowUp | iced::keyboard::key::Named::ArrowDown),
        ) => {
            let gate = matches!(state.screen, Screen::Mail)
                && !state.nav.in_search
                && state.nav.search_text.is_empty()
                && !state
                    .nav
                    .cur
                    .as_ref()
                    .map(|(_, c)| c == "__drafts__" || c == "__outbox__")
                    .unwrap_or(false);
            if !gate {
                return Task::none();
            }
            let entries = &state.mail_display;
            if entries.is_empty() {
                return Task::none();
            }
            let cur_idx = state
                .selected
                .and_then(|(a, h)| {
                    entries
                        .iter()
                        .position(|d| d.acct == a && d.row.envelope_hash == h)
                })
                .map(|i| i as isize)
                .unwrap_or(-1);
            let next = match named {
                iced::keyboard::key::Named::ArrowUp => (cur_idx - 1).max(0),
                _ => (cur_idx + 1).min(entries.len() as isize - 1),
            } as usize;
            let d = &entries[next];
            let (acct, hash) = (d.acct, d.row.envelope_hash);
            state.selected = Some((acct, hash));
            state.reading = None;
            // 滚动跟随：让选中行保持在列表中部
            let view_h = state.nav.list_view_h;
            state.nav.list_offset = ((next as f32) * ROW_H - view_h / 2.0).max(0.0);
            state.body_req = Some((acct, hash));
            state.send(acct, Cmd::OpenMessage(hash));
            Task::none()
        }
        Key::Named(iced::keyboard::key::Named::F5) if matches!(state.screen, Screen::Mail) => {
            update(state, Message::ManualRefresh)
        }
        // 快捷键体系：读信时可用（r/a/f/s/e/Delete）。
        // gate：所有字母键要求无搜索词、无侧栏内联输入——否则打字会误触发动作
        Key::Character(ref c) if matches!(state.screen, Screen::Mail) => {
            let typing_safe = state.nav.search_text.is_empty()
                && !state.nav.in_search
                && state.new_folder.is_none()
                && state.rename_folder.is_none()
                && state.pending_folder_delete.is_none()
                && state.pending_delete.is_none()
                && !state.pending_batch_delete;
            if !typing_safe {
                return Task::none();
            }
            let Some((acct, hash)) = state.selected else {
                return Task::none();
            };
            match c.as_str() {
                // e：归档（Gmail/Apple Mail 惯例键位）
                "e" => update(state, Message::ArchiveCurrent(acct, hash)),
                // i：AI 摘要（读信态；与其他字母键同一打字安全 gate）
                "i" => update(state, Message::AiSummarize(acct, hash)),
                "r" => update(state, Message::Reply(acct, hash)),
                "a" => update(state, Message::ReplyAll(acct, hash)),
                "f" => update(state, Message::Forward(acct, hash)),
                "s" => {
                    let flagged = state
                        .rows
                        .iter()
                        .find(|(a, r)| *a == acct && r.envelope_hash == hash)
                        .map(|(_, r)| r.flagged)
                        .unwrap_or(false);
                    update(state, Message::ToggleStar(acct, hash, !flagged))
                }
                _ => Task::none(),
            }
        }
        Key::Named(iced::keyboard::key::Named::Delete) => {
            // 与字母键同 gate：侧栏内联输入编辑时 Delete 是文本操作，不触发删除确认
            if matches!(state.screen, Screen::Mail)
                && state.nav.search_text.is_empty()
                && state.new_folder.is_none()
                && state.rename_folder.is_none()
                && state.selected.is_some()
            {
                let (acct, hash) = state.selected.unwrap();
                update(state, Message::AskDelete(acct, hash))
            } else {
                Task::none()
            }
        }
        _ => Task::none(),
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 列表回顶：只重置 list_offset 不够——部件本身停在旧 offset 时，虚拟化窗口
///（按 offset=0 渲染顶部行）与视口错位，表现为切文件夹后一片空白直到手动滚动
pub(crate) fn scroll_list_to_top() -> Task<Message> {
    iced::widget::operation::scroll_to(
        iced::widget::Id::new(list::MAIL_LIST_ID),
        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
    )
}
