//! 邮件操作消息：回复转发/删除/移动/批量/文件夹管理/归档垃圾/退订/导出/图片

use crate::update::{close_reading_if_gone, selection_by_acct};
use crate::{
    compose::{self, open_compose_with, ReplyKind},
    extract_forward_attachments, open_in_browser, Compose, DispKind, Message, State,
};
use iced::widget::text_editor;
use iced::Task;
use smail_core::Cmd;

/// 消息组的统一入口（分发器按 or-pattern 委托；模式与处理体自 update.rs 原样搬运）
pub(crate) fn handle(state: &mut State, msg: Message) -> Task<Message> {
    match msg {
        Message::Reply(acct, hash) => {
            open_compose_with(state, acct, hash, ReplyKind::Reply)
        }
        Message::ReplyAll(acct, hash) => {
            open_compose_with(state, acct, hash, ReplyKind::ReplyAll)
        }
        Message::Forward(acct, hash) => {
            let open_task = open_compose_with(state, acct, hash, ReplyKind::Forward);
            // 转发带附件：从本地 .eml 提取到 staging 目录后回填（发送时
            // 服务线程按路径读文件）。.eml 未缓存（没打开过就转发）则没有
            // 附件，用户可在编辑器里手动添加
            let slug = state
                .accounts
                .get(acct)
                .map(|a| a.slug.clone())
                .unwrap_or_default();
            let eml = state
                .data_root
                .join(slug)
                .join("bodies")
                .join(format!("{hash}.eml"));
            let staging = state.data_root.join("attachments-staging");
            if eml.exists() {
                let attach_task = Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            extract_forward_attachments(&eml, &staging)
                        })
                        .await
                        .unwrap_or_default()
                    },
                    Message::ForwardAttachmentsReady,
                );
                return Task::batch([open_task, attach_task]);
            }
            open_task
        }
        Message::ForwardAttachmentsReady(paths) => {
            if let Some(c) = &mut state.compose {
                for p in paths {
                    if let Some(ps) = p.to_str() {
                        if !c.attachments.contains(&ps.to_string()) {
                            c.attachments.push(ps.to_string());
                        }
                    }
                }
            }
            Task::none()
        }
        Message::AskDelete(acct, hash) => {
            state.pending_delete = Some((acct, hash));
            Task::none()
        }
        Message::CancelDelete => {
            state.pending_delete = None;
            Task::none()
        }
        Message::DeleteMail(acct, hash) => {
            state.pending_delete = None;
            state.batch.move_picker = None;
            state
                .rows
                .retain(|(a, r)| !(*a == acct && r.envelope_hash == hash));
            state.rebuild_display();
            state.batch.selection.remove(&(acct, hash)); // 后续批量不再携带死 hash
                                                         // 精确关闭：仅当正在阅读的正是被删邮件（同账号另封信不受牵连），
                                                         // 同时清 body_req 防迟到正文回弹
            close_reading_if_gone(state);
            state.send(acct, Cmd::DeleteMessage(hash));
            Task::none()
        }
        Message::MoveTo(acct, hash) => {
            // 切换面板：同封再点关闭，换封重开
            state.batch.move_picker = if state.batch.move_picker == Some((acct, hash)) {
                None
            } else {
                Some((acct, hash))
            };
            Task::none()
        }
        Message::MoveToDest(acct, hash, dest) => {
            state.batch.move_picker = None;
            // 乐观更新：立即从列表移除；服务器结果仅状态提示（失败下次同步回真）
            state
                .rows
                .retain(|(a, r)| !(*a == acct && r.envelope_hash == hash));
            state.rebuild_display();
            state.batch.selection.remove(&(acct, hash));
            if state.selected == Some((acct, hash)) {
                state.reading = None;
                state.selected = None;
                state.body_req = None; // 迟到正文不弹开已移走的邮件
            }
            state.send(acct, Cmd::MoveMessage { hash, dest });
            Task::none()
        }
        Message::ToggleSelect(acct, hash) => {
            if !state.batch.selection.remove(&(acct, hash)) {
                state.batch.selection.insert((acct, hash));
            }
            Task::none()
        }
        Message::SelectAll => {
            // 选当前可见项（过滤/会话展开后以显示模型为准，而非底层数据）
            for d in &state.mail_display {
                let hash = match &d.kind {
                    DispKind::ThreadRoot { latest, .. } => *latest,
                    _ => d.row.envelope_hash,
                };
                state.batch.selection.insert((d.acct, hash));
            }
            Task::none()
        }
        Message::ClearSelection => {
            state.batch.selection.clear();
            state.batch.move_open = false;
            state.batch.pending_delete = false;
            Task::none()
        }
        Message::BatchSeen(seen) => {
            for (acct, hashes) in selection_by_acct(state) {
                state.send(
                    acct,
                    Cmd::BatchFlags {
                        hashes,
                        seen: Some(seen),
                        flagged: None,
                    },
                );
            }
            Task::none()
        }
        Message::BatchStar => {
            for (acct, hashes) in selection_by_acct(state) {
                state.send(
                    acct,
                    Cmd::BatchFlags {
                        hashes,
                        seen: None,
                        flagged: Some(true),
                    },
                );
            }
            Task::none()
        }
        Message::BatchMoveOpen => {
            state.batch.move_open = !state.batch.move_open;
            state.batch.pending_delete = false;
            Task::none()
        }
        Message::BatchMoveDest(acct, dest) => {
            state.batch.move_open = false;
            let hashes: Vec<i64> = state
                .batch
                .selection
                .iter()
                .filter(|(a, _)| *a == acct)
                .map(|(_, h)| *h)
                .collect();
            // 先取判定再赋值（避免 reading 借用冲突）
            let reading_hit = state
                .selected
                .map(|(a, h)| a == acct && hashes.contains(&h))
                .unwrap_or(false);
            state
                .rows
                .retain(|(a, r)| !(*a == acct && hashes.contains(&r.envelope_hash)));
            state.rebuild_display();
            if reading_hit {
                state.reading = None;
                state.selected = None;
                state.body_req = None;
            }
            state.batch.selection.retain(|(a, _)| *a != acct);
            state.send(acct, Cmd::BatchMove { hashes, dest });
            Task::none()
        }
        Message::AskBatchDelete => {
            state.batch.pending_delete = true;
            state.batch.move_open = false;
            Task::none()
        }
        Message::BatchDeleteConfirm => {
            state.batch.pending_delete = false;
            for (acct, hashes) in selection_by_acct(state) {
                state
                    .rows
                    .retain(|(a, r)| !(*a == acct && hashes.contains(&r.envelope_hash)));
                state.batch.selection.retain(|(a, _)| *a != acct);
                state.send(acct, Cmd::BatchDelete { hashes });
            }
            state.rebuild_display();
            // 多账号/搜索态：只关掉真正破删的阅读（另一账号正在读的邮件不受牵连）
            close_reading_if_gone(state);
            Task::none()
        }
        Message::BatchDeleteCancel => {
            state.batch.pending_delete = false;
            Task::none()
        }
        Message::NewFolderOpen(acct) => {
            state.folder_edit.new_folder = Some((acct, String::new()));
            Task::none()
        }
        Message::NewFolderInput(name) => {
            if let Some((_, n)) = &mut state.folder_edit.new_folder {
                *n = name;
            }
            Task::none()
        }
        Message::NewFolderConfirm => {
            let Some((acct, name)) = state.folder_edit.new_folder.take() else {
                return Task::none();
            };
            let name = name.trim().to_string();
            if name.is_empty() {
                state.error = Some(state.l().err_folder_name.into());
                return Task::none();
            }
            state.send(acct, Cmd::CreateFolder(name));
            Task::none()
        }
        Message::NewFolderCancel => {
            state.folder_edit.new_folder = None;
            Task::none()
        }
        Message::RenameFolderOpen(acct, path) => {
            // 预填当前名（微调场景多；全选替换也方便）
            state.folder_edit.rename_folder = Some((acct, path.clone(), path));
            Task::none()
        }
        Message::RenameFolderInput(name) => {
            if let Some((_, _, n)) = &mut state.folder_edit.rename_folder {
                *n = name;
            }
            Task::none()
        }
        Message::RenameFolderConfirm => {
            let Some((acct, old, new)) = state.folder_edit.rename_folder.take() else {
                return Task::none();
            };
            let new = new.trim().to_string();
            if new.is_empty() {
                state.error = Some(state.l().err_folder_rename.into());
                return Task::none();
            }
            if new == old {
                return Task::none(); // 未改动，静默关行
            }
            // 乐观更新：侧栏与当前视图立即换名（服务器结果仅状态提示，失败下次同步回真）
            for (_, f) in state
                .nav
                .folders
                .iter_mut()
                .filter(|(a, f)| *a == acct && f.path == old)
            {
                f.path = new.clone();
            }
            let mut viewing_renamed = false;
            if let Some((a, c)) = &mut state.nav.cur {
                if *a == acct && *c == old {
                    *c = new.clone();
                    viewing_renamed = true;
                }
            }
            state.send(
                acct,
                Cmd::RenameFolder {
                    old_path: old,
                    new_path: new.clone(),
                },
            );
            if viewing_renamed {
                // 服务线程按序处理：先改名（含本地索引换名）再重选；
                // 本地先行立即用新名字刷新行，消除行内存的旧 folder 字段
                state.send(acct, Cmd::SelectFolder(new));
            }
            Task::none()
        }
        Message::RenameFolderCancel => {
            state.folder_edit.rename_folder = None;
            Task::none()
        }
        Message::AskFolderDelete(acct, path) => {
            state.folder_edit.pending_delete = Some((acct, path));
            Task::none()
        }
        Message::FolderDeleteConfirm => {
            let Some((acct, path)) = state.folder_edit.pending_delete.take() else {
                return Task::none();
            };
            // 正在浏览该文件夹：清视图（本地记录由服务线程清理）
            if state
                .nav
                .cur
                .as_ref()
                .map(|(a, c)| *a == acct && c == &path)
                == Some(true)
            {
                state.nav.cur = None;
                state.rows.clear();
                state.rebuild_display();
                state.reading = None;
                state.selected = None;
            }
            state
                .nav
                .folders
                .retain(|(a, f)| !(*a == acct && f.path == path));
            state.send(acct, Cmd::DeleteFolder(path));
            Task::none()
        }
        Message::FolderDeleteCancel => {
            state.folder_edit.pending_delete = None;
            Task::none()
        }
        Message::MarkUnread(acct, hash) => {
            state.send(acct, Cmd::MarkSeen(hash, false));
            Task::none()
        }
        Message::ToggleStar(acct, hash, on) => {
            state.send(acct, Cmd::ToggleFlagged(hash, on));
            Task::none()
        }
        Message::ArchiveCurrent(acct, hash) => {
            // 与移动同样乐观：立即从列表移除，归档在后台执行
            state
                .rows
                .retain(|(a, r)| !(*a == acct && r.envelope_hash == hash));
            state.rebuild_display();
            state.batch.selection.remove(&(acct, hash));
            state.batch.move_picker = None;
            state.reading = None;
            state.selected = None;
            state.body_req = None;
            state.send(acct, Cmd::Archive(vec![hash]));
            Task::none()
        }
        Message::BatchArchive => {
            for (acct, hashes) in selection_by_acct(state) {
                state
                    .rows
                    .retain(|(a, r)| !(*a == acct && hashes.contains(&r.envelope_hash)));
                state.batch.selection.retain(|(a, _)| *a != acct);
                state.send(acct, Cmd::Archive(hashes));
            }
            state.rebuild_display();
            close_reading_if_gone(state);
            Task::none()
        }
        Message::SpamCurrent(acct, hash) => {
            state
                .rows
                .retain(|(a, r)| !(*a == acct && r.envelope_hash == hash));
            state.rebuild_display();
            state.batch.selection.remove(&(acct, hash));
            state.batch.move_picker = None;
            state.reading = None;
            state.selected = None;
            state.body_req = None;
            state.send(acct, Cmd::Spam(vec![hash]));
            Task::none()
        }
        Message::BatchSpam => {
            for (acct, hashes) in selection_by_acct(state) {
                state
                    .rows
                    .retain(|(a, r)| !(*a == acct && hashes.contains(&r.envelope_hash)));
                state.batch.selection.retain(|(a, _)| *a != acct);
                state.send(acct, Cmd::Spam(hashes));
            }
            state.rebuild_display();
            close_reading_if_gone(state);
            Task::none()
        }
        Message::Unsubscribe(url) => {
            if url.starts_with("http://") || url.starts_with("https://") {
                open_in_browser(&url);
            } else if let Some(rest) = url.strip_prefix("mailto:") {
                // mailto 退订：预填写信（去 ?参数取地址；主题常见 Unsubscribe）
                let to = rest.split('?').next().unwrap_or(rest).to_string();
                if state.account_names.is_empty() {
                    return Task::none();
                }
                let account = state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
                state.compose = Some(Compose {
                    ai_instruction: String::new(),
                    ai_busy: false,
                    ai_overwrite_ask: false,
                    ai_pending: None,
                    ai_rewrite_mode: false,
                    ai_prev: None,
                    autosave_at: None,
                    draft_id: None,
                    account,
                    pgp_sign: false,
                    pgp_encrypt: false,
                    confirm_discard: false,
                    to,
                    cc: String::new(),
                    bcc: String::new(),
                    subject: "Unsubscribe".into(),
                    body: text_editor::Content::new(),
                    attachments: vec![],
                });
                // mailto 退订同样弹独立写信窗（已开则复用）
                if state.compose_window.is_none() {
                    let (win_id, task) = iced::window::open(iced::window::Settings {
                        size: iced::Size::new(900.0, 640.0),
                        min_size: Some(iced::Size::new(720.0, 480.0)),
                        ..Default::default()
                    });
                    state.compose_window = Some(win_id);
                    return task.map(move |_| Message::ComposeWindowOpened(win_id));
                }
            }
            Task::none()
        }
        Message::SaveAttachment(acct, hash, name) => {
            state.send(acct, Cmd::SaveAttachment { hash, name });
            Task::none()
        }
        Message::ExportEmlRequest(acct, hash) => {
            // 保存对话框期间用户可能切换邮件：目标在请求时钉死
            state.export_target = Some((acct, hash));
            let subject = state
                .rows
                .iter()
                .find(|(a, r)| *a == acct && r.envelope_hash == hash)
                .map(|(_, r)| r.subject.clone())
                .unwrap_or_default();
            let safe: String = subject
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let name = if safe.is_empty() {
                format!("{hash}.eml")
            } else {
                format!("{safe}.eml")
            };
            Task::perform(
                async move {
                    rfd::AsyncFileDialog::new()
                        .set_title("导出 .eml")
                        .set_file_name(name)
                        .save_file()
                        .await
                        .map(|f| f.path().to_path_buf())
                },
                Message::ExportEmlPicked,
            )
        }
        Message::ExportEmlPicked(Some(path)) => {
            // 用请求时记录的目标（不能用 selected——对话框期间可能已切换阅读）
            if let Some((acct, hash)) = state.export_target.take() {
                let p = path.display().to_string();
                state.send(acct, Cmd::ExportEml { hash, path: p });
            }
            Task::none()
        }
        Message::ExportEmlPicked(None) => {
            state.export_target = None; // 取消也清理，防下次误用旧目标
            Task::none()
        }
        Message::LoadRemoteImages(acct, hash) => {
            // pending 不清空：下载失败（回包为空）时按钮保留可重试；
            // loading 标记防连点，成功时在 ImagesLoaded 里清 pending
            if let Some((a, r)) = &mut state.reading {
                if *a == acct && r.hash == hash && !r.remote_loading && !r.remote_pending.is_empty()
                {
                    r.remote_loading = true;
                    let urls = r.remote_pending.clone();
                    state.send(acct, Cmd::LoadImages { hash, urls });
                }
            }
            Task::none()
        }
        Message::MarkdownParsed { acct, hash, md } => {
            // 过期结果丢弃：解析期间用户可能已切到别的邮件（以阅读器自身身份判定）
            if state
                .reading
                .as_ref()
                .map(|(a, r)| *a == acct && r.hash == hash)
                .unwrap_or(false)
            {
                if let Some((a, r)) = &mut state.reading {
                    if *a == acct {
                        if let Some(parsed) = md.0.lock().unwrap().take() {
                            r.md = Some(parsed);
                        }
                    }
                }
            }
            Task::none()
        }
        Message::LinkClicked(url) => {
            if url.starts_with("http://") || url.starts_with("https://") {
                open_in_browser(&url);
            } else if let Some(rest) = url.strip_prefix("mailto:") {
                // 正文里的 mailto 链接：预填写信（与退订入口同一解析规则）
                let to = rest.split('?').next().unwrap_or(rest).to_string();
                if !to.is_empty() && !state.account_names.is_empty() {
                    let subject = rest
                        .split("subject=")
                        .nth(1)
                        .map(|s| s.split('&').next().unwrap_or(s).to_string())
                        .unwrap_or_default();
                    let account = state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
                    let mut body = text_editor::Content::new();
                    if let Some(sig) = state
                        .accounts
                        .get(account)
                        .and_then(|a| a.signature.as_ref())
                        .filter(|s| !s.trim().is_empty())
                    {
                        body.perform(compose::text_editor_edit_paste(&format!("\n\n-- \n{sig}")));
                    }
                    state.compose = Some(Compose {
                        ai_instruction: String::new(),
                        autosave_at: None,
                        ai_busy: false,
                        ai_overwrite_ask: false,
                        ai_pending: None,
                        ai_rewrite_mode: false,
                        ai_prev: None,
                        draft_id: None,
                        account,
                        pgp_sign: false,
                        pgp_encrypt: false,
                        confirm_discard: false,
                        to,
                        cc: String::new(),
                        bcc: String::new(),
                        subject,
                        body,
                        attachments: vec![],
                    });
                    // 正文里的 mailto 链接：同样弹独立写信窗（已开则复用）
                    if state.compose_window.is_none() {
                        let (win_id, task) = iced::window::open(iced::window::Settings {
                            size: iced::Size::new(900.0, 640.0),
                            min_size: Some(iced::Size::new(720.0, 480.0)),
                            ..Default::default()
                        });
                        state.compose_window = Some(win_id);
                        return task.map(move |_| Message::ComposeWindowOpened(win_id));
                    }
                }
            }
            Task::none()
        }
        // ---------------- M7：移动 / 批量 / 文件夹 / 联系人 ----------------
        Message::EmptyTrashRequest => {
            state.pending_empty_trash = true;
            Task::none()
        }
        Message::EmptyTrashConfirm => {
            state.pending_empty_trash = false;
            if let Some((a, f)) = state.nav.cur.clone() {
                if state.cur_is_trash_folder() {
                    state.send(a, Cmd::EmptyTrash(f));
                }
            }
            Task::none()
        }
        Message::EmptyTrashCancel => {
            state.pending_empty_trash = false;
            Task::none()
        }
        _ => unreachable!("update 分发器把非本组消息路由到了 mail_ops：{msg:?}"),
    }
}
