//! 事件处理（服务线程 → UI）
//!
//! ## 职责
//!
//! - **事件回流处理**：TaggedEvent → State 状态更新
//! - **同步落地**：文件夹列表、邮件列表、正文加载
//! - **标记变更**：乐观更新 + 服务器确认
//! - **聚合视图合并**：搜索结果、统一收件箱
//! - **错误处理**：状态横幅、OAuth 提示
//!
//! ## 关键事件
//!
//! - `Event::Folders`：文件夹列表更新 + 托盘未读数
//! - `Event::FolderLoaded`：邮件列表加载 + 会话聚合
//! - `Event::BodyLoaded`：正文装载 + 附件图片
//! - `Event::FlagsChanged`：标记变更 + 阅读器精确关闭
//! - `Event::SearchResults`：搜索结果合并（多账号）
//! - `Event::NewMail`：新邮件通知触发
//!
//! ## 状态同步
//!
//! - **横幅管理**：新状态冲掉旧错误（show-once 语义）
//! - **OAuth 流程**：授权提示 → 清空横幅
//! - **阅读器关闭**：删除/移动邮件后精确判断是否关闭阅读器
use iced::Task;

use std::sync::Arc;

use crate::{close_reading_if_gone, is_image_bytes, Message, ParsedMd, Reading, RowEntry, State};
use smail_core::{Event, SessionCmd, TaggedEvent};

pub(crate) fn handle_event(state: &mut State, ev: TaggedEvent) -> Task<Message> {
    let acct = ev.account;
    match ev.event {
        Event::Status(s) => {
            // 任何新状态都意味着授权流程推进，清掉设备流横幅；
            // 新进展同时冲掉旧错误横幅（show-once 语义的另一半）
            state.oauth_prompt = None;
            state.error = None;
            state.statuses.insert(acct, s);
        }
        Event::Error(s) => {
            state.oauth_prompt = None;
            state.statuses.insert(acct, format!("错误：{s}"));
        }
        Event::Folders(infos) => {
            state.nav.folders.retain(|(a, _)| *a != acct);
            for info in infos {
                state.nav.folders.push((acct, info));
            }
            // 托盘未读数（各账号 INBOX 未读之和）
            let total: u32 = state
                .nav
                .folders
                .iter()
                .filter(|(_, f)| f.path.eq_ignore_ascii_case("INBOX"))
                .map(|(_, f)| f.unseen)
                .sum();
            crate::tray::set_unread(total);
        }
        Event::FolderLoaded { folder, rows } => {
            let viewing = state
                .nav
                .cur
                .as_ref()
                .map(|(a, c)| *a == acct && c.eq_ignore_ascii_case(&folder))
                .unwrap_or(false);
            // 统一收件箱模式：INBOX 同步落地 → 重新拉聚合（LocalList 不发 FolderLoaded，无循环）
            if state.nav.viewing_unified && folder.eq_ignore_ascii_case("INBOX") {
                if let Some(tx) = &state.cmd_tx {
                    let _ = tx.send(SessionCmd::UnifiedInbox);
                }
            }
            if viewing && !state.nav.in_search {
                // 翻页穷尽判定：LoadMore 回包行数没涨 = 到底了。
                // 新邮件到达（行数变化）必须重置 done，否则翻页被永久锁死
                if state.nav.load_more_pending {
                    state.nav.load_more_pending = false;
                    if rows.len() == state.rows.len() {
                        state.nav.load_more_done = true;
                    }
                } else if rows.len() != state.rows.len() {
                    state.nav.load_more_done = false;
                }
                state.rows = rows.into_iter().map(move |r| (acct, r)).collect();
                // 对端删除传播：全量替换后清死引用——批量选择/展开态/确认条/
                // 移动面板指向已消失的邮件时丢弃（跨账号条目保留）
                let alive: std::collections::HashSet<i64> = state
                    .rows
                    .iter()
                    .filter(|(a, _)| *a == acct)
                    .map(|(_, r)| r.envelope_hash)
                    .collect();
                state
                    .batch
                    .selection
                    .retain(|(a, h)| *a != acct || alive.contains(h));
                state.expanded.retain(|h, _| alive.contains(h));
                if let Some((a, h)) = state.pending_delete {
                    if a == acct && !alive.contains(&h) {
                        state.pending_delete = None;
                    }
                }
                if let Some((a, h)) = state.batch.move_picker {
                    if a == acct && !alive.contains(&h) {
                        state.batch.move_picker = None;
                    }
                }
                // selected 同样随死引用清理（无阅读态时 close_reading_if_gone
                // 不会碰它；残留会让快捷键作用于不可见邮件——回归测试苔出）
                if let Some((a, h)) = state.selected {
                    if a == acct && !alive.contains(&h) {
                        state.selected = None;
                    }
                }
                close_reading_if_gone(state);
                state.rebuild_display();
            }
        }
        Event::BodyLoaded {
            hash,
            attachment_images,
            attachment_previews,
            subject,
            body,
            to_addrs,
            cc_addrs,
            unsubscribe,
            inline_images,
            remote_images,
            is_html,
            attachments,
            pgp,
        } => {
            // 在途守卫：请求后用户可能已切文件夹/删信/另选——迟到的正文
            // 不覆写阅读器（否则弹开一封不在当前列表的邮件，快捷键随之误作用）
            if state.body_req != Some((acct, hash)) {
                return Task::none();
            }
            state.body_req = None;
            state.selected = Some((acct, hash));
            // 内嵌图直接转显示句柄；远程图挂起等用户决定（隐私默认禁）。
            // 按 magic bytes 过滤：inline 附件可能是 pdf 等非图片，
            // 塞进 image widget 会解码失败 panic
            let images: Vec<_> = inline_images
                .into_iter()
                .filter(|(_cid, bytes)| is_image_bytes(bytes))
                .map(|(_cid, bytes)| iced::widget::image::Handle::from_bytes(bytes))
                .collect();
            // HTML：markdown 后台解析（超大邮件不再卡 UI 线程），完成经
            // MarkdownParsed 回填；解析期间 reader 先显示源文本自然过渡
            let md_src = if is_html { Some(body.clone()) } else { None };
            state.reading = Some((
                acct,
                Reading {
                    hash,
                    attach_images: attachment_images
                        .into_iter()
                        .map(|(n, b)| (n, iced::widget::image::Handle::from_bytes(b)))
                        .collect(),
                    attach_previews: attachment_previews
                        .into_iter()
                        .map(|(n, b)| (n, iced::widget::image::Handle::from_bytes(b)))
                        .collect(),
                    subject,
                    body,
                    to_addrs,
                    cc_addrs,
                    unsubscribe,
                    images,
                    remote_pending: remote_images,
                    remote_loading: false,
                    md: None,
                    attachments,
                    pgp,
                },
            ));
            if let Some(src) = md_src {
                return Task::perform(
                    async move {
                        ParsedMd(Arc::new(std::sync::Mutex::new(Some(
                            iced::widget::markdown::Content::parse(&src),
                        ))))
                    },
                    move |md| Message::MarkdownParsed { acct, hash, md },
                );
            }
        }
        Event::OAuthPrompt { url, code } => {
            state.oauth_prompt = Some((url, code));
        }
        Event::MailSent { to, subject } => {
            state
                .statuses
                .insert(acct, format!("已发送 → {to}（{subject}）"));
        }
        Event::DraftsListed(list) => {
            state.drafts.retain(|(a, _)| *a != acct);
            for d in list {
                state.drafts.push((acct, d));
            }
        }
        Event::DraftSaved { id } => {
            // 自动保存回填：首存（draft_id None）拿到 id 后续更新同一行
            //（否则每轮自动保存都会新建草稿）。手动保存已切屏不受影响
            if let crate::Screen::Compose(c) = &mut state.screen {
                if c.draft_id.is_none() {
                    c.draft_id = Some(id);
                }
            }
            state.statuses.insert(acct, format!("草稿已保存（#{id}）"));
        }
        Event::DraftDeleted(_) | Event::MessageDeleted(_) => {}
        Event::FlagsChanged {
            hash,
            seen,
            flagged,
        } => {
            if let Some(r) = state
                .rows
                .iter_mut()
                .find(|(a, r)| *a == acct && r.envelope_hash == hash)
                .map(|(_, r)| r)
            {
                if let Some(v) = seen {
                    r.seen = v;
                }
                if let Some(v) = flagged {
                    r.flagged = v;
                }
            }
            // 未读圆点/加粗字体/会话聚合 unseen 徽标都依赖 flags，重建显示模型
            state.rebuild_display();
        }
        Event::ConnState(ok) => {
            state.conn_ok.insert(acct, ok);
            if ok {
                state.outbox_counts.remove(&acct); // 重连后由 flush 重报
            }
        }
        Event::OutboxListed(rows) => {
            let acct = ev.account;
            state.outbox_rows.retain(|(a, _)| *a != acct);
            for r in rows {
                state.outbox_rows.push((acct, r));
            }
        }
        Event::OutboxPending { pending, failed } => {
            state.outbox_counts.insert(acct, (pending, failed));
            // 发件箱视图打开时刷新列表（计数变化=状态流转）
            if state.nav.cur.as_ref().map(|(_, f)| f) == Some(&"__outbox__".to_string())
                && state.nav.cur.as_ref().map(|(a, _)| *a) == Some(acct)
            {
                state.send(acct, smail_core::Cmd::ListOutbox);
            }
        }
        Event::UnifiedRows { rows } => {
            state.unified_contrib.insert(acct, rows);
            if state.nav.viewing_unified {
                // 多账号按日期合并取前 500（与搜索聚合同模式）
                let mut all: Vec<RowEntry> = state
                    .unified_contrib
                    .iter()
                    .flat_map(|(a, rows)| rows.iter().map(move |r| (*a, r.clone())))
                    .collect();
                all.sort_by_key(|a| std::cmp::Reverse(a.1.date_unix));
                state.rows = all.into_iter().take(500).collect();
                state.rebuild_display();
            }
        }
        Event::SearchResults { rows, snippets, .. } => {
            // 片段按 hash 映射（跨账号搜索结果统一渲染）
            for (r, snip) in rows.iter().zip(snippets.iter()) {
                if !snip.is_empty() {
                    state.search_snippets.insert(r.envelope_hash, snip.clone());
                }
            }
            state.search_contrib.insert(acct, rows);
            if state.nav.in_search {
                state.rows = state.merged_search_rows();
                state.rebuild_display();
            }
        }
        Event::AttachmentSaved { path } => {
            state.statuses.insert(acct, format!("附件已保存：{path}"));
        }
        // ---- M7 ----
        Event::NewMail {
            from,
            subject,
            folder,
            hash,
        } => {
            // 通知跳转：事件自带 folder/hash（旧版靠 rows 反查——非当前
            // 文件夹的新邮件不在 rows 里会降级为无跳转通知）
            crate::notify::notify_mail(
                &from,
                &subject,
                crate::notify::NotifyClick {
                    acct: ev.account,
                    folder,
                    hash,
                },
            );
        }
        Event::ImagesLoaded { hash, images } => {
            // 仅当前阅读的邮件回填（过期下载丢弃；账号+hash 双比对，
            // 跨账号 envelope_hash 碰撞不互串）
            if state
                .reading
                .as_ref()
                .map(|(a, r)| *a == acct && r.hash == hash)
                .unwrap_or(false)
            {
                if let Some((_, r)) = &mut state.reading {
                    r.remote_loading = false;
                    // 全部失败时保留按钮可重试；部分成功则清挂起（v1 不细分）
                    if !images.is_empty() {
                        for (_url, bytes) in images {
                            if is_image_bytes(&bytes) {
                                r.images
                                    .push(iced::widget::image::Handle::from_bytes(bytes));
                            }
                        }
                        r.remote_pending.clear();
                    }
                }
            }
        }
        Event::ContactsFull(rows) => {
            match state.contacts_full.iter_mut().find(|(a, _)| *a == acct) {
                Some((_, v)) => *v = rows,
                None => state.contacts_full.push((acct, rows)),
            }
        }
        Event::ContactsListed(addrs) => {
            state.contacts = addrs;
        }
        Event::SyncProgress {
            folder,
            total,
            processed,
        } => {
            // 更新状态行：「正在同步 INBOX (120/500)」
            let progress_text = if processed == 0 {
                format!("同步 {} 开始…", folder)
            } else if processed >= total {
                format!("同步 {} 完成", folder)
            } else {
                format!("正在同步 {} ({}/{})", folder, processed, total)
            };
            state.statuses.insert(acct, progress_text);
        }
    }
    Task::none()
}

// ---------------------------------------------------------------------------
// subscription
// ---------------------------------------------------------------------------
