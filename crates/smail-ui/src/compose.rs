//! 写信视图
//!
//! ## 职责
//!
//! - **写信表单**：收件人/抄送/主题/正文编辑
//! - **附件管理**：文件选择、列表显示、移除
//! - **发件账号切换**：多账号场景选择发件身份
//! - **AI 辅助**：智能起草、润色、翻译
//! - **PGP 加密**：签名/加密开关、WKD 自动公钥发现
//! - **草稿自动保存**：编辑静默 20 秒后自动存草稿
//!
//! ## 触发入口
//!
//! - **新邮件**：空白表单
//! - **回复**：自动填充收件人、主题、引用正文
//! - **回复全部**：保留原收件人 + 抄送列表
//! - **转发**：保留主题、正文、附件
//! - **编辑草稿**：加载草稿内容
//!
//! ## 联系人补全
//!
//! - 输入 `@` 触发补全菜单
//! - 数据源：通讯录（发件人自动提取 + 频率统计）
//! - 显示格式：`名字 <email>`
//!
//! ## AI 功能
//!
//! - **智能起草**：输入指令 → AI 生成完整邮件
//! - **改写模式**：润色/翻译等（跳过覆盖确认，有撤销兜底）
//! - **撤销 AI**：一步还原到 AI 修改前的快照

use std::sync::Arc;

use iced::widget::{button, column, container, row, scrollable, text, text_editor, Space};
use iced::{Element, Length};

use crate::setup::labeled_field;
use crate::{theme, unix_to_local, Field, Message, State};

pub(crate) struct Compose {
    /// AI 起草指令（写信屏输入框）
    pub(crate) ai_instruction: String,
    /// AI 起草进行中（按钮禁用防连点）
    pub(crate) ai_busy: bool,
    /// AI 结果覆盖确认条
    pub(crate) ai_overwrite_ask: bool,
    pub(crate) ai_pending: Option<smail_ai::DraftContent>,
    /// 改写模式（润色/翻译等）：跳过覆盖确认（有撤销兜底）
    pub(crate) ai_rewrite_mode: bool,
    /// AI 修改前的 (主题, 正文) 快照——「撤销 AI」一步还原
    pub(crate) ai_prev: Option<(String, String)>,
    /// 最后一次编辑时刻（Some=有未自动保存的修改；定时器静默 20s 后落草稿）
    pub(crate) autosave_at: Option<std::time::Instant>,
    pub(crate) draft_id: Option<i64>,
    /// 发件账号
    pub(crate) account: usize,
    /// M5-d：PGP 签名/加密开关
    pub(crate) pgp_sign: bool,
    pub(crate) pgp_encrypt: bool,
    /// Esc 时有内容 → 先确认丢弃（防手滑丢稿）
    pub(crate) confirm_discard: bool,
    pub(crate) to: String,
    pub(crate) cc: String,
    pub(crate) bcc: String,
    pub(crate) subject: String,
    pub(crate) body: text_editor::Content,
    pub(crate) attachments: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ReplyKind {
    Reply,
    ReplyAll,
    Forward,
}

/// 回复/转发：从当前阅读的邮件构造 compose（地址 + Re:/Fwd: 主题 + 引用正文）
pub(crate) fn open_compose_with(
    state: &mut State,
    acct: usize,
    hash: i64,
    kind: ReplyKind,
) -> iced::Task<crate::Message> {
    let Some((_, r)) = state
        .rows
        .iter()
        .find(|(a, r)| *a == acct && r.envelope_hash == hash)
    else {
        state.error = Some(state.l().err_orig_missing.into());
        return iced::Task::none();
    };
    let orig = r.clone();
    // 发件人 "名字 <addr>" → addr；收件人地址（回复全部时带上除自己外的其他人）
    let from_addr = orig
        .from_addr
        .split('<')
        .nth(1)
        .map(|x| x.trim_end_matches('>').to_string())
        .unwrap_or_else(|| orig.from_addr.clone());
    let my_addr = state
        .accounts
        .get(acct)
        .map(|a| a.imap.username.clone())
        .unwrap_or_default();
    let (to, subject_prefix) = match kind {
        ReplyKind::Reply => (from_addr.clone(), "Re: "),
        ReplyKind::ReplyAll => {
            // 原发件人 + 原 To/Cc（去掉自己）
            let rd = state
                .reading
                .as_ref()
                .map(|(_, r)| (r.to_addrs.clone(), r.cc_addrs.clone()));
            let (o_to, o_cc) = rd.unwrap_or_default();
            let all = format!("{from_addr}, {o_to}, {o_cc}");
            let filtered: Vec<String> = all
                .split([',', ';'])
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty() && !x.to_lowercase().contains(&my_addr.to_lowercase()))
                .collect();
            // 去重保序
            let mut seen = std::collections::HashSet::new();
            let deduped: Vec<String> = filtered
                .into_iter()
                .filter(|x| seen.insert(x.to_lowercase()))
                .collect();
            (deduped.join(", "), "Re: ")
        }
        ReplyKind::Forward => (String::new(), "Fwd: "),
    };
    // 主题（避免 Re: Re: Re: 堆叠）
    let subject = if orig
        .subject
        .to_lowercase()
        .starts_with(subject_prefix.to_lowercase().trim())
    {
        orig.subject.clone()
    } else {
        format!("{subject_prefix}{}", orig.subject)
    };
    // 引用正文（转发用完整头；回复用精简引用）
    let quote_header = format!(
        "在 {}，{} 写道：",
        unix_to_local(orig.date_unix),
        if orig.from_addr.is_empty() {
            "对方".into()
        } else {
            orig.from_addr.clone()
        }
    );
    let mut body = text_editor::Content::new();
    let reading_body = state
        .reading
        .as_ref()
        .map(|(_, rd)| rd.body.clone())
        .unwrap_or_default();
    let quoted: String = std::iter::once(quote_header)
        .chain(reading_body.lines().map(|l| format!("> {l}")))
        .collect::<Vec<_>>()
        .join("\n");
    // 回复/转发：签名在引用正文之前（邮件惯例）
    let sig = state
        .accounts
        .get(acct)
        .and_then(|a| a.signature.as_ref())
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("\n\n-- \n{s}\n"))
        .unwrap_or_default();
    body.perform(text_editor_edit_paste(&format!("{sig}\n\n{quoted}\n")));
    state.send(acct, smail_core::Cmd::ListContacts); // M7：补全数据
    state.compose = Some(Compose {
        ai_instruction: String::new(),
        ai_busy: false,
        ai_overwrite_ask: false,
        ai_pending: None,
        ai_rewrite_mode: false,
        ai_prev: None,
        autosave_at: None,
        draft_id: None,
        account: acct,
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
    // 回复/转发：弹独立写信窗（已开则复用），主窗列表保持可见。
    // open 的 Task 必须由 update 返回链路上抛，否则窗口不会真正创建
    if state.compose_window.is_none() {
        let (win_id, task) = iced::window::open(iced::window::Settings {
            size: iced::Size::new(900.0, 640.0),
            min_size: Some(iced::Size::new(720.0, 480.0)),
            ..Default::default()
        });
        state.compose_window = Some(win_id);
        return task.map(move |_| crate::Message::ComposeWindowOpened(win_id));
    }
    iced::Task::none()
}

pub(crate) fn text_editor_edit_paste(body: &str) -> iced::widget::text_editor::Action {
    iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(Arc::from(
        body.to_string(),
    )))
}

fn compose_field<'a>(
    label: &'a str,
    placeholder: &'a str,
    value: &'a str,
    f: Field,
) -> Element<'a, Message> {
    labeled_field(label, 44.0, placeholder, value, false, move |v| {
        Message::ComposeField(f, v)
    })
}

pub(crate) fn compose_view<'a>(state: &'a State, c: &'a Compose) -> Element<'a, Message> {
    let editing = c.draft_id.is_some();
    let account_name = state
        .account_names
        .get(c.account)
        .map(|s| s.as_str())
        .unwrap_or("?");
    let multi = state.account_names.len() > 1;

    let mut header = row![text(if editing {
        state.l().title_edit
    } else {
        state.l().title_new
    })
    .size(17)
    .font(theme::ui_font(iced::font::Weight::Semibold)),]
    .spacing(8);
    if multi {
        header = header.push(
            button(text(crate::lang::f(state.l().account_sw, &[&account_name])).size(13))
                .on_press(Message::ComposeAccountNext)
                .padding([3, 8]),
        );
    }
    header = header.push(Space::new().width(Length::Fill));
    // 签名/加密：真正的开关控件（以前用 ○● 字符拼按钮，看着像没渲染完）
    let sign_toggler = iced::widget::Toggler::new(c.pgp_sign)
        .label(state.l().sign_toggle.to_string())
        .on_toggle(|_| Message::TogglePgpSign)
        .size(16.0)
        .text_size(13)
        .spacing(6);
    let enc_toggler = iced::widget::Toggler::new(c.pgp_encrypt)
        .label(state.l().enc_toggle.to_string())
        .on_toggle(|_| Message::TogglePgpEncrypt)
        .size(16.0)
        .text_size(13)
        .spacing(6);
    header = header.push(sign_toggler);
    header = header.push(enc_toggler);
    header = header.push(
        button(text(state.l().send).size(13))
            .on_press(Message::SendClicked)
            .padding([6, 16])
            .style(theme::accent_button),
    );
    header = header.push(
        button(text(state.l().save_draft).size(13))
            .on_press(Message::SaveDraftClicked)
            .padding([6, 12]),
    );
    header = header.push(
        button(text(state.l().close).size(13))
            .on_press(Message::ComposeClose)
            .padding([6, 12]),
    );

    let mut attach_row = row![].spacing(6).width(Length::Fill);
    attach_row = attach_row.push(
        button(text(state.l().add_attach).size(13))
            .on_press(Message::AddAttachment)
            .padding([3, 10])
            .style(theme::ghost_button),
    );
    for (i, path) in c.attachments.iter().enumerate() {
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        attach_row = attach_row.push(
            button(text(format!("✕ {name}")).size(13))
                .on_press(Message::RemoveAttachment(i))
                .padding([2, 8]),
        );
    }

    // Esc 丢弃确认条（有内容时拦截）
    let discard_bar: Option<Element<Message>> = c.confirm_discard.then(|| {
        container(
            row![
                text(state.l().discard_q).size(13),
                button(text(state.l().discard).size(13))
                    .on_press(Message::ComposeDiscardConfirm)
                    .padding([3, 10])
                    .style(theme::accent_button),
                button(text(state.l().keep_editing).size(13))
                    .on_press(Message::ComposeDiscardCancel)
                    .padding([3, 10])
                    .style(theme::ghost_button),
                Space::new().width(Length::Fill),
            ]
            .spacing(8),
        )
        .padding([8, 16])
        .width(Length::Fill)
        .style(|t| theme::status_badage(t, theme::BadgeKind::Warn))
        .into()
    });

    // AI 覆盖确认条（起草结果落地前）
    let ai_overwrite_bar: Option<Element<Message>> = c.ai_overwrite_ask.then(|| {
        container(
            row![
                text(state.l().ai_overwrite_q).size(13),
                button(text(state.l().ai_overwrite_yes).size(13))
                    .on_press(Message::AiDraftApply)
                    .padding([3, 10])
                    .style(theme::accent_button),
                button(text(state.l().cancel).size(13))
                    .on_press(Message::AiDraftDiscard)
                    .padding([3, 10])
                    .style(theme::ghost_button),
                Space::new().width(Length::Fill),
            ]
            .spacing(8),
        )
        .padding([8, 16])
        .width(Length::Fill)
        .style(|t| theme::status_badage(t, theme::BadgeKind::Warn))
        .into()
    });

    // AI 区块（未启用时整块隐藏——不打扰纯手写用户）：
    // 指令行（起草/重写）+ 快捷改写 chips（润色/语气/长度/翻译）+ 撤销
    let ai_block: Option<iced::Element<'_, Message>> = if crate::ai::ai_enabled(state) {
        let draft_msg = if c.ai_instruction.trim().is_empty() {
            None
        } else {
            Some(Message::AiDraftGo)
        };
        let main_btn_label = if c.ai_busy {
            state.l().ai_working
        } else if c.ai_rewrite_mode {
            // 上一轮是改写：按钮语义变为「再来一版」
            state.l().ai_rewrite_btn
        } else {
            state.l().ai_draft_btn
        };
        let ai_row = row![
            iced::widget::text_input(state.l().ai_instr_ph, &c.ai_instruction)
                .on_input(Message::AiInstructionInput)
                .on_submit(Message::AiDraftGo)
                .size(13)
                .width(Length::Fill)
                .padding(6),
            button(text(main_btn_label).size(13))
                .on_press_maybe(if c.ai_busy { None } else { draft_msg })
                .padding([6, 12])
                .style(theme::accent_button),
        ]
        .spacing(8)
        .width(Length::Fill);
        let chips: [(&'static str, &'static str); 7] = [
            (state.l().ai_polish, state.l().ai_instr_polish),
            (state.l().ai_formal, state.l().ai_instr_formal),
            (state.l().ai_casual, state.l().ai_instr_casual),
            (state.l().ai_shorter, state.l().ai_instr_shorter),
            (state.l().ai_longer, state.l().ai_instr_longer),
            (state.l().ai_to_zh, state.l().ai_instr_to_zh),
            (state.l().ai_to_en, state.l().ai_instr_to_en),
        ];
        let body_empty = c.body.text().trim().is_empty() && c.subject.trim().is_empty();
        let mut chip_row = row![].spacing(4);
        for (label, instr) in chips {
            chip_row = chip_row.push(
                button(text(label).size(11))
                    .on_press_maybe(if c.ai_busy || body_empty {
                        None
                    } else {
                        Some(Message::AiRewrite(instr.into()))
                    })
                    .padding([2, 8])
                    .style(theme::ghost_button),
            );
        }
        // 单步撤销：回到 AI 介入前（连续多次 AI 操作保最早快照）
        if c.ai_prev.is_some() {
            chip_row = chip_row.push(
                button(text(state.l().ai_undo).size(11))
                    .on_press(Message::AiDraftUndo)
                    .padding([2, 8])
                    .style(theme::ghost_button),
            );
        }
        Some(
            column![ai_row, container(chip_row).padding([2, 0])]
                .spacing(4)
                .width(Length::Fill)
                .into(),
        )
    } else {
        None
    };

    let mut col = column![header.padding([8, 16])].spacing(0);
    if let Some(bar) = discard_bar {
        col = col.push(bar);
    }
    if let Some(bar) = ai_overwrite_bar {
        col = col.push(bar);
    }
    scrollable(
        column![
            col,
            compose_field_with_contacts(
                state,
                state.l().to_label,
                state.l().to_ph,
                &c.to,
                Field::To
            ),
            compose_field_with_contacts(
                state,
                state.l().cc_label,
                state.l().cc_ph,
                &c.cc,
                Field::Cc
            ),
            compose_field_with_contacts(
                state,
                state.l().bcc_label,
                state.l().bcc_ph,
                &c.bcc,
                Field::Bcc
            ),
            compose_field(
                state.l().subject_label,
                state.l().subject_ph,
                &c.subject,
                Field::Subject
            ),
            ai_block,
            attach_row,
            container(
                column![
                    // 占位提示：text_editor 没有占位符 API，空正文时在上方给一行引导
                    (c.body.text().is_empty()).then(|| {
                        text(state.l().body_ph.to_string())
                            .size(13)
                            .style(theme::secondary_text)
                    }),
                    text_editor(&c.body)
                        .on_action(Message::ComposeBodyEdit)
                        .height(400),
                    text(state.l().compose_hints.to_string())
                        .size(11)
                        .style(theme::secondary_text),
                ]
                .spacing(2),
            )
            .padding(8)
            .width(Length::Fill),
            text(state.statuses.get(&c.account).cloned().unwrap_or_default()).size(11),
        ]
        .spacing(8)
        .padding(16),
    )
    .into()
}

/// 逗号/分号（含中文全角）分隔的多地址串：返回（前缀切割点，最后一段）。
/// 补全针对最后一段，回填时保留前缀。
pub(crate) fn split_last_segment(s: &str) -> (usize, &str) {
    let mut cut = 0usize;
    for (i, ch) in s.char_indices() {
        if matches!(ch, ',' | ';' | '，' | '；') {
            cut = i + ch.len_utf8();
        }
    }
    (cut, s[cut..].trim())
}

fn compose_field_with_contacts<'a>(
    state: &'a State,
    label: &'a str,
    placeholder: &'a str,
    value: &'a str,
    f: Field,
) -> Element<'a, Message> {
    let field = compose_field(label, placeholder, value, f);
    let (_, last) = split_last_segment(value);
    if last.is_empty() {
        return column![field].into();
    }
    let needle = last.to_lowercase();
    let hits: Vec<&String> = state
        .contacts
        .iter()
        .filter(|a| a.to_lowercase().contains(&needle))
        .take(6)
        .collect();
    if hits.is_empty() {
        return column![field].into();
    }
    let mut list = column![].spacing(1);
    for h in hits {
        list = list.push(
            button(text(h.clone()).size(13))
                .on_press(Message::ContactPicked(f, h.clone()))
                .padding([2, 8])
                .style(theme::ghost_button),
        );
    }
    column![
        field,
        container(list).padding([2, 0]).style(theme::reader_meta)
    ]
    .into()
}
