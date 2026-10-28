//! 写作屏消息：编辑/自动保存/发信/附件/草稿/发件箱/PGP 开关

use crate::update::update;
use crate::{
    compose::{split_last_segment, text_editor_edit_paste},
    Compose, Field, Message, Screen, State,
};
use iced::widget::text_editor;
use iced::Task;
use smail_core::Cmd;

/// 消息组的统一入口（分发器按 or-pattern 委托；模式与处理体自 update.rs 原样搬运）
pub(crate) fn handle(state: &mut State, msg: Message) -> Task<Message> {
    match msg {
        Message::ComposeNew => {
            if state.account_names.is_empty() {
                state.error = Some(state.l().err_need_account.into());
                return Task::none();
            }
            let account = state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
            state.send(account, Cmd::ListContacts); // M7：补全数据
                                                    // 签名预填（账号配置 signature；分隔符用邮件惯用的 "-- "）
            let mut body = text_editor::Content::new();
            if let Some(sig) = state
                .accounts
                .get(account)
                .and_then(|a| a.signature.as_ref())
                .filter(|s| !s.trim().is_empty())
            {
                body.perform(text_editor_edit_paste(&format!("\n\n-- \n{sig}")));
            }
            state.screen = Screen::Compose(Compose {
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
                to: String::new(),
                cc: String::new(),
                bcc: String::new(),
                subject: String::new(),
                body,
                attachments: vec![],
            });
            Task::none()
        }
        Message::ComposeFromDraft(acct, id) => {
            if let Some(d) = state
                .drafts
                .iter()
                .find(|(a, d)| *a == acct && d.id == id)
                .map(|(_, d)| d.clone())
            {
                let mut content = text_editor::Content::new();
                content.perform(text_editor_edit_paste(&d.body));
                state.send(acct, Cmd::ListContacts); // M7：补全数据
                state.screen = Screen::Compose(Compose {
                    ai_instruction: String::new(),
                    ai_busy: false,
                    ai_overwrite_ask: false,
                    ai_pending: None,
                    ai_rewrite_mode: false,
                    ai_prev: None,
                    autosave_at: None,
                    draft_id: Some(d.id),
                    account: acct,
                    pgp_sign: false,
                    pgp_encrypt: false,
                    confirm_discard: false,
                    to: d.to_addr,
                    cc: d.cc,
                    bcc: d.bcc,
                    subject: d.subject,
                    body: content,
                    attachments: vec![],
                });
            }
            Task::none()
        }
        Message::ComposeClose => {
            // 与 Esc 同语义：有内容先弹丢弃确认（否则误点关闭直接丢并小时心血）
            if let Screen::Compose(c) = &mut state.screen {
                let has_content = !c.to.is_empty()
                    || !c.subject.is_empty()
                    || !c.body.text().trim().is_empty()
                    || !c.attachments.is_empty();
                if has_content && !c.confirm_discard {
                    c.confirm_discard = true;
                    return Task::none();
                }
            }
            state.screen = Screen::Mail;
            Task::none()
        }
        Message::ComposeAccountNext => {
            if let Screen::Compose(c) = &mut state.screen {
                c.account = (c.account + 1) % state.account_names.len().max(1);
            }
            Task::none()
        }
        Message::ComposeField(field, value) => {
            if let Screen::Compose(c) = &mut state.screen {
                c.autosave_at = Some(std::time::Instant::now());
                match field {
                    Field::To => c.to = value,
                    Field::Cc => c.cc = value,
                    Field::Bcc => c.bcc = value,
                    Field::Subject => c.subject = value,
                }
            }
            Task::none()
        }
        Message::ComposeBodyEdit(action) => {
            if let Screen::Compose(c) = &mut state.screen {
                c.autosave_at = Some(std::time::Instant::now());
                c.body.perform(action);
            }
            Task::none()
        }
        Message::AutosaveTick => {
            // 静默 20s 且有实质内容才存：不打扰快速写作，也不建空草稿。
            // 先取快照再发送（send 需要 &self，与 &mut screen 借用冲突）
            let send_now = (|| {
                let Screen::Compose(c) = &mut state.screen else {
                    return None;
                };
                let due = c
                    .autosave_at
                    .map(|t| t.elapsed().as_secs() >= 20)
                    .unwrap_or(false);
                let has_content = !c.to.is_empty()
                    || !c.subject.is_empty()
                    || !c.body.text().trim().is_empty()
                    || !c.attachments.is_empty();
                if due && has_content {
                    c.autosave_at = None; // 本轮落盘；继续编辑会重新标脏
                    Some((
                        c.account,
                        Cmd::SaveDraft {
                            id: c.draft_id,
                            to: c.to.clone(),
                            cc: c.cc.clone(),
                            bcc: c.bcc.clone(),
                            subject: c.subject.clone(),
                            body: c.body.text().to_string(),
                        },
                    ))
                } else {
                    None
                }
            })();
            if let Some((acct, cmd)) = send_now {
                state.send(acct, cmd);
            }
            Task::none()
        }
        Message::ComposeShortcut(key) => {
            // 只在写信屏生效（其他屏的 ⌘↩/⌘S 交给系统语义）
            if matches!(state.screen, Screen::Compose(_)) {
                match key {
                    crate::ComposeKey::Send => update(state, Message::SendClicked),
                    crate::ComposeKey::SaveDraft => update(state, Message::SaveDraftClicked),
                }
            } else {
                Task::none()
            }
        }
        Message::SendClicked => {
            if let Screen::Compose(c) = &state.screen {
                if c.to.trim().is_empty() {
                    state.error = Some(state.l().err_empty_to.into());
                    return Task::none();
                }
                let acct = c.account;
                state.send(
                    acct,
                    Cmd::SendMail {
                        to: c.to.clone(),
                        cc: c.cc.clone(),
                        bcc: c.bcc.clone(),
                        subject: c.subject.clone(),
                        body: c.body.text().to_string(),
                        attachments: c.attachments.clone(),
                        sign: c.pgp_sign,
                        encrypt: c.pgp_encrypt,
                    },
                );
                if let Some(id) = c.draft_id {
                    state.send(acct, Cmd::DeleteDraft(id));
                }
                state.screen = Screen::Mail;
                state.statuses.insert(acct, "发送中…".into());
            }
            Task::none()
        }
        Message::SaveDraftClicked => {
            if let Screen::Compose(c) = &state.screen {
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
                state.screen = Screen::Mail;
                state.statuses.insert(acct, "草稿已保存".into());
            }
            Task::none()
        }
        Message::DraftRowClicked(acct, id) => update(state, Message::ComposeFromDraft(acct, id)),
        Message::OutboxRetry(id) => {
            let acct = state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
            state.send(acct, Cmd::OutboxRetry(id));
            Task::none()
        }
        Message::OutboxRemove(id) => {
            let acct = state.nav.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
            state.send(acct, Cmd::OutboxRemove(id));
            Task::none()
        }
        Message::AddAttachment => Task::perform(
            async {
                rfd::AsyncFileDialog::new()
                    .set_title("选择附件")
                    .pick_files()
                    .await
                    .map(|files| files.iter().map(|f| f.path().to_path_buf()).collect())
            },
            Message::AttachmentsPicked,
        ),
        Message::AttachmentsPicked(Some(files)) => {
            if let Screen::Compose(c) = &mut state.screen {
                c.autosave_at = Some(std::time::Instant::now());
                for f in files {
                    if let Some(p) = f.to_str() {
                        c.attachments.push(p.to_string());
                    }
                }
            }
            Task::none()
        }
        Message::AttachmentsPicked(None) => Task::none(),
        Message::RemoveAttachment(i) => {
            if let Screen::Compose(c) = &mut state.screen {
                c.autosave_at = Some(std::time::Instant::now());
                if i < c.attachments.len() {
                    c.attachments.remove(i);
                }
            }
            Task::none()
        }
        Message::ContactPicked(field, addr) => {
            if let Screen::Compose(c) = &mut state.screen {
                let cur = match field {
                    Field::To => c.to.clone(),
                    Field::Cc => c.cc.clone(),
                    Field::Bcc => c.bcc.clone(),
                    Field::Subject => return Task::none(),
                };
                let (cut, _) = split_last_segment(&cur);
                let joined = format!("{}{}, ", &cur[..cut], addr);
                match field {
                    Field::To => c.to = joined,
                    Field::Cc => c.cc = joined,
                    Field::Bcc => c.bcc = joined,
                    Field::Subject => {}
                }
            }
            Task::none()
        }
        // ---------------- AI 助手 ----------------
        Message::TogglePgpSign => {
            if let Screen::Compose(c) = &mut state.screen {
                c.pgp_sign = !c.pgp_sign;
            }
            Task::none()
        }
        Message::TogglePgpEncrypt => {
            if let Screen::Compose(c) = &mut state.screen {
                c.pgp_encrypt = !c.pgp_encrypt;
                if c.pgp_encrypt {
                    c.pgp_sign = true; // 我们的加密必签名（防篡改）
                }
            }
            Task::none()
        }
        Message::ComposeDiscardConfirm => {
            state.screen = Screen::Mail;
            Task::none()
        }
        Message::ComposeDiscardCancel => {
            if let Screen::Compose(c) = &mut state.screen {
                c.confirm_discard = false;
            }
            Task::none()
        }
        _ => unreachable!("update 分发器把非本组消息路由到了 compose_ops：{msg:?}"),
    }
}
