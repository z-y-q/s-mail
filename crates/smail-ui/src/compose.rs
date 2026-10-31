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
    /// 富文本预览开（markdown 标记 → 排版视图；编辑正文即退出预览）
    pub(crate) preview: bool,
    /// 预览渲染内容（后台解析回填；None=解析中显示提示）
    pub(crate) preview_md: Option<iced::widget::markdown::Content>,
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
        preview: false,
        preview_md: None,
    });
    // 回复/转发：弹独立写信窗（已开则复用），主窗列表保持可见。
    // open 的 Task 必须由 update 返回链路上抛，否则窗口不会真正创建
    if state.compose_window.is_none() {
        let (win_id, task) = iced::window::open(iced::window::Settings {
            size: iced::Size::new(900.0, 640.0),
            min_size: Some(iced::Size::new(720.0, 480.0)),
            exit_on_close_request: false,
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

    // 富文本工具栏（markdown 标记套选区；发送管线统一出 HTML 版）。
    // 预览态下整排隐藏（预览只读，编辑按钮单独出现）
    let fmt_btn = |label: &'static str, fmt: MdFmt, weight: iced::font::Weight| {
        button(
            text(label.to_string())
                .size(11)
                .font(theme::ui_font(weight)),
        )
        .on_press(Message::ComposeFormat(fmt))
        .padding([2, 8])
        .style(theme::ghost_button)
    };
    let toolbar: Element<Message> = row![
        fmt_btn("B", MdFmt::Bold, iced::font::Weight::Bold),
        fmt_btn("I", MdFmt::Italic, iced::font::Weight::Normal),
        fmt_btn("</>", MdFmt::Code, iced::font::Weight::Normal),
        fmt_btn("链接", MdFmt::Link, iced::font::Weight::Normal),
        fmt_btn("引用", MdFmt::Quote, iced::font::Weight::Normal),
        fmt_btn("列表", MdFmt::Bullet, iced::font::Weight::Normal),
        fmt_btn("标题", MdFmt::Heading, iced::font::Weight::Normal),
        Space::new().width(Length::Fill),
        button(
            text(if c.preview {
                state.l().edit.to_string()
            } else {
                state.l().preview.to_string()
            })
            .size(11)
        )
        .on_press_maybe(if c.body.text().trim().is_empty() && !c.preview {
            None
        } else {
            Some(Message::ComposePreviewToggle)
        })
        .padding([2, 8])
        .style(move |t, st| {
            if c.preview {
                theme::accent_button(t, st)
            } else {
                theme::ghost_button(t, st)
            }
        }),
    ]
    .spacing(4)
    .into();

    // 正文区：编辑态 = 工具栏 + 编辑器；预览态 = markdown 排版视图
    // （与阅读窗同一渲染管线，收发两侧观感一致）
    let body_area: Element<Message> = if c.preview {
        match &c.preview_md {
            Some(md) => container(
                scrollable(
                    iced::widget::markdown::view(
                        md.items(),
                        iced::widget::markdown::Settings::with_text_size(13, {
                            if matches!(state.prefs.ui_theme, iced::Theme::Dark) {
                                iced::Theme::Dark
                            } else {
                                iced::Theme::Light
                            }
                        }),
                    )
                    .map(Message::LinkClicked),
                )
                .direction(theme::scroller())
                .height(Length::Fill),
            )
            .height(400)
            .width(Length::Fill)
            .into(),
            None => container(
                text(state.l().img_loading.to_string())
                    .size(13)
                    .style(theme::secondary_text),
            )
            .height(400)
            .width(Length::Fill)
            .center(Length::Fill)
            .into(),
        }
    } else {
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
        .spacing(2)
        .into()
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
            container(column![toolbar, body_area].spacing(2),)
                .padding(8)
                .width(Length::Fill),
            text(state.statuses.get(&c.account).cloned().unwrap_or_default()).size(11),
        ]
        .spacing(8)
        .padding(16),
    )
    .direction(theme::scroller())
    .into()
}

/// 富文本工具栏动作（markdown 标记；发送管线自动转 HTML 版）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MdFmt {
    Bold,
    Italic,
    Code,
    Link,
    Quote,
    Bullet,
    Heading,
}

impl MdFmt {
    /// 包裹类标记（选区两侧成对插入；支持撤销切换）
    fn markers(self) -> Option<(&'static str, &'static str)> {
        match self {
            MdFmt::Bold => Some(("**", "**")),
            MdFmt::Italic => Some(("*", "*")),
            MdFmt::Code => Some(("`", "`")),
            MdFmt::Link => Some(("[", "](链接地址)")),
            _ => None,
        }
    }

    /// 行首前缀（整行作用；引用/列表逐行，标题只作用首行）
    fn line_prefix(self) -> Option<&'static str> {
        match self {
            MdFmt::Quote => Some("> "),
            MdFmt::Bullet => Some("- "),
            MdFmt::Heading => Some("## "),
            _ => None,
        }
    }
}

/// (行, 列[字符]) → 全文字节偏移（iced text_editor 的 Position 语义）
fn position_to_offset(text: &str, pos: text_editor::Position) -> usize {
    let mut offset = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if i == pos.line {
            return offset
                + line
                    .chars()
                    .take(pos.column)
                    .map(char::len_utf8)
                    .sum::<usize>();
        }
        offset += line.len() + 1;
    }
    text.len()
}

/// 全文字节偏移 → (行, 列[字符])
pub(crate) fn offset_to_position(text: &str, offset: usize) -> text_editor::Position {
    let offset = offset.min(text.len());
    let line = text[..offset].matches('\n').count();
    let line_start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    text_editor::Position {
        line,
        column: text[line_start..offset].chars().count(),
    }
}

/// 富文本标记应用：返回（新全文, 新光标选区起点/终点字节偏移）。
/// 包裹类（B/I/代码/链接）：已有成对标记时撤销剥离；无选区插入空标记光标居中。
/// 行首类（引用/列表/标题）：选区覆盖的每一行加/去前缀（标题只作用首行）。
pub(crate) fn md_apply(
    text: &str,
    sel: Option<(usize, usize)>,
    fmt: MdFmt,
) -> (String, usize, usize) {
    let (start, end) = sel
        .map(|(a, b)| (a.min(b), a.max(b)))
        .unwrap_or((text.len(), text.len()));
    let (start, end) = (start.min(text.len()), end.min(text.len()));

    if let Some((pre, post)) = fmt.markers() {
        // 撤销判定：选区两侧已是同款标记 → 剥离（再点一次恢复原样）
        if text[..start].ends_with(pre) && text[end..].starts_with(post) {
            let inner = &text[start..end];
            let mut out = String::with_capacity(text.len() - pre.len() - post.len());
            out.push_str(&text[..start - pre.len()]);
            out.push_str(inner);
            out.push_str(&text[end + post.len()..]);
            let c = start - pre.len();
            return (out, c, c);
        }
        let inner = &text[start..end];
        let mut out = String::with_capacity(text.len() + pre.len() + post.len());
        out.push_str(&text[..start]);
        out.push_str(pre);
        let inner_start = out.len();
        out.push_str(inner);
        let inner_end = out.len();
        out.push_str(post);
        out.push_str(&text[end..]);
        // 链接：光标选中地址占位符（直接输入即覆盖）；其余：无选区光标居中
        if fmt == MdFmt::Link {
            // post = "](链接地址)"：占位符在 "](" 之后、")" 之前
            let (a, b) = (inner_end + 2, inner_end + post.len() - 1);
            return (out, a, b);
        }
        let (a, b) = if inner.is_empty() {
            (inner_end, inner_end)
        } else {
            (inner_start, inner_end)
        };
        return (out, a, b);
    }

    let prefix = fmt.line_prefix().expect("非包裹类必有行前缀");
    // 行边界：start 所在行首（含跨行选区的首行）到 end 所在行尾
    let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let last_line = match fmt {
        MdFmt::Heading => start, // 标题只作用光标所在行
        _ => end,
    };
    let line_end = text[last_line..]
        .find('\n')
        .map(|i| last_line + i)
        .unwrap_or(text.len());
    let block = &text[line_start..line_end];
    let already = block.lines().all(|l| l.is_empty() || l.starts_with(prefix));
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..line_start]);
    let mut new_block_end = out.len();
    for (i, line) in block.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if already {
            // 撤销：剥掉同款前缀（保留其余内容）
            out.push_str(line.strip_prefix(prefix).unwrap_or(line));
        } else if line.is_empty() && fmt != MdFmt::Heading {
            out.push_str(line); // 空行不加列表/引用前缀（避免制造垃圾行）
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
        new_block_end = out.len();
    }
    out.push_str(&text[line_end..]);
    // 光标落在被改块的末尾（新文本坐标系）
    (out, new_block_end, new_block_end)
}

/// 当前编辑器全文+选区 → md_apply 入参（字节偏移对）。
/// 无选区时返回光标所在点（成对标记就地插入，不跳到文末）
pub(crate) fn editor_selection_offsets(c: &text_editor::Content) -> (usize, usize) {
    let text = c.text();
    let cur = c.cursor();
    let caret = position_to_offset(&text, cur.position);
    let anchor = cur
        .selection
        .map(|p| position_to_offset(&text, p))
        .unwrap_or(caret);
    (anchor, caret)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_wraps_selection_and_centers_cursor_without_selection() {
        let text = "你好世界";
        // 选中「世界」（字节 6..12）
        let (out, a, b) = md_apply(text, Some((6, 12)), MdFmt::Bold);
        assert_eq!(out, "你好**世界**");
        assert_eq!(&out[a..b], "世界");
        // 无选区：插入成对标记，光标（空选区）落中间
        let (out, a, b) = md_apply("", None, MdFmt::Bold);
        assert_eq!(out, "****");
        assert_eq!((a, b), (2, 2));
    }

    #[test]
    fn bold_toggles_off_when_already_wrapped() {
        let text = "前**重点**后";
        // 选区正指「重点」，两侧已是 ** → 剥离（前=3B + **=2B → 选区起点 5）
        let s = "前**".len();
        let e = s + "重点".len();
        let (out, _, _) = md_apply(text, Some((s, e)), MdFmt::Bold);
        assert_eq!(out, "前重点后");
    }

    #[test]
    fn code_and_italic_use_single_char_markers() {
        let (out, a, b) = md_apply("x val y", Some((2, 5)), MdFmt::Code);
        assert_eq!(out, "x `val` y");
        assert_eq!(&out[a..b], "val");
        // 无选区（= 光标点）：就地插入成对标记，光标落中间
        let (out, a, b) = md_apply("速率", Some(("速率".len(), "速率".len())), MdFmt::Italic);
        assert_eq!(out, "速率**");
        assert_eq!((a, b), ("速率".len() + 1, "速率".len() + 1));
    }

    #[test]
    fn link_selects_address_placeholder() {
        let (out, a, b) = md_apply("看这里", Some((0, "看这里".len())), MdFmt::Link);
        assert_eq!(out, "[看这里](链接地址)");
        assert_eq!(&out[a..b], "链接地址");
    }

    #[test]
    fn quote_prefixes_each_selected_line_and_strips_on_toggle() {
        let text = "甲\n乙\n丙";
        let all = Some((0, text.len()));
        let (out, _, _) = md_apply(text, all, MdFmt::Quote);
        assert_eq!(out, "> 甲\n> 乙\n> 丙");
        // 再点一次：整块剥离
        let (out2, _, _) = md_apply(&out, Some((0, out.len())), MdFmt::Quote);
        assert_eq!(out2, text);
    }

    #[test]
    fn bullet_skips_blank_lines() {
        let text = "一\n\n二";
        let (out, _, _) = md_apply(text, Some((0, text.len())), MdFmt::Bullet);
        assert_eq!(out, "- 一\n\n- 二");
    }

    #[test]
    fn heading_touches_cursor_line_only() {
        let text = "第一行\n第二行";
        // 光标在第二行行首（偏移 = 第一行+换行）
        let off = "第一行".len() + 1;
        let (out, _, _) = md_apply(text, Some((off, off)), MdFmt::Heading);
        assert_eq!(out, "第一行\n## 第二行");
    }

    #[test]
    fn offsets_round_trip_through_positions() {
        let text = "ab\ncd你好\nef";
        let pos = text_editor::Position { line: 1, column: 3 };
        let off = position_to_offset(text, pos);
        // 第 1 行第 3 字符之后 = 「好」的字节起点（列按字符计，偏移按字节计）
        assert_eq!(&text[off..off + "好".len()], "好");
        let back = offset_to_position(text, off);
        assert_eq!((back.line, back.column), (1, 3));
    }
}
