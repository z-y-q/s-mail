//! 读信区视图。

use iced::widget::{button, column, container, row, scrollable, text, Space};
use iced::{Element, Length};

use crate::{theme, unix_to_local, Message, State};

pub(crate) fn reader_view(state: &State) -> Element<'_, Message> {
    if !state.in_search && state.cur.as_ref().map(|(_, c)| c == "__drafts__") == Some(true) {
        return container(text(state.l().draft_hint).size(14))
            .center(Length::Fill)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    let Some((acct, reading)) = &state.reading else {
        return container(
            column![
                text("✉️").size(34),
                text(state.l().pick_mail).size(13),
                text(state.l().pick_mail_hint)
                    .size(11)
                    .style(theme::secondary_text),
            ]
            .spacing(8)
            .align_x(iced::alignment::Alignment::Center),
        )
        .center(Length::Fill)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
    };
    let hash = reading.hash;
    let (seen, flagged) = state
        .rows
        .iter()
        .find(|(a, r)| a == acct && r.envelope_hash == hash)
        .map(|(_, r)| (r.seen, r.flagged))
        .unwrap_or((true, false));

    let move_open = state.move_picker == Some((*acct, hash));
    // 摘要标签：忙 / 会话聚合 / 已缓存 / 常态（一眼看得出会得到什么）
    let in_thread = state.threaded && !state.in_search && !state.viewing_unified;
    let thread_root_here = in_thread
        && state.mail_display.iter().any(|d| {
            d.acct == *acct
                && d.row.envelope_hash == hash
                && matches!(
                    &d.kind,
                    crate::DispKind::ThreadRoot { count, .. } if *count > 1
                )
        });
    let summary_label = if state.ai.busy == Some(crate::ai::BusyKind::Summarize) {
        state.l().ai_working
    } else if thread_root_here {
        state.l().ai_summary_thread
    } else if state.ai.summary_cache.contains_key(&(*acct, hash)) {
        if state.ai.summary_visible {
            state.l().ai_summary_hide
        } else {
            state.l().ai_summary_cached
        }
    } else {
        state.l().ai_summary_btn
    };
    let actions = row![
        button(text(summary_label).size(12))
            .on_press(Message::AiSummarize(*acct, hash))
            .padding([3, 8])
            .style(theme::ghost_button),
        button(
            text(if seen {
                state.l().mark_unread_t
            } else {
                state.l().mark_read_t
            })
            .size(12)
        )
        .on_press(Message::MarkUnread(*acct, hash))
        .padding([3, 8])
        .style(theme::ghost_button),
        button(
            text(if flagged {
                state.l().unstar
            } else {
                state.l().star_t
            })
            .size(12),
        )
        .on_press(Message::ToggleStar(*acct, hash, !flagged))
        .padding([3, 8])
        .style(theme::ghost_button),
        button(
            text(if move_open {
                state.l().cancel_move
            } else {
                state.l().move_to
            })
            .size(12)
        )
        .on_press(Message::MoveTo(*acct, hash))
        .padding([3, 8])
        .style(move |t, st| {
            if move_open {
                theme::accent_button(t, st)
            } else {
                theme::ghost_button(t, st)
            }
        }),
        button(text(state.l().archive).size(12))
            .on_press(Message::ArchiveCurrent(*acct, hash))
            .padding([3, 8])
            .style(theme::ghost_button),
        button(text(state.l().spam).size(12))
            .on_press(Message::SpamCurrent(*acct, hash))
            .padding([3, 8])
            .style(theme::ghost_button),
        button(text(state.l().ai_smart_reply).size(12))
            .on_press_maybe(if state.ai.busy.is_some() {
                None
            } else {
                Some(Message::AiSmartReply(*acct, hash))
            })
            .padding([3, 8])
            .style(theme::ghost_button),
        button(text(state.l().reply).size(12))
            .on_press(Message::Reply(*acct, hash))
            .padding([3, 8])
            .style(theme::ghost_button),
        button(text(state.l().reply_all).size(12))
            .on_press(Message::ReplyAll(*acct, hash))
            .padding([3, 8])
            .style(theme::ghost_button),
        button(text(state.l().forward).size(12))
            .on_press(Message::Forward(*acct, hash))
            .padding([3, 8])
            .style(theme::ghost_button),
        button(text(state.l().delete).size(12))
            .on_press(Message::AskDelete(*acct, hash))
            .padding([3, 8])
            .style(theme::ghost_button),
        button(
            text(if state.ai.busy == Some(crate::ai::BusyKind::Summarize) {
                state.l().ai_working
            } else {
                state.l().ai_summary_btn
            })
            .size(12),
        )
        .on_press(Message::AiSummarize(*acct, hash))
        .padding([3, 8])
        .style(theme::ghost_button),
        Space::new().width(Length::Fill),
    ]
    .spacing(4);

    // 删除确认（IMAP 端是 \Deleted + EXPUNGE，不可撤销，必须二次确认）
    let confirm: Option<Element<Message>> = state
        .pending_delete
        .filter(|(a, h)| *a == *acct && *h == hash)
        .map(|_| {
            row![
                text(state.l().del_q).size(13),
                button(text(state.l().perm_delete).size(12))
                    .on_press(Message::DeleteMail(*acct, hash))
                    .style(button::danger)
                    .padding([2, 8]),
                button(text(state.l().cancel).size(12))
                    .on_press(Message::CancelDelete)
                    .padding([2, 8]),
                Space::new().width(Length::Fill),
            ]
            .spacing(8)
            .into()
        });

    let mut attachments_col = column![].spacing(4);
    // 一键退订（List-Unsubscribe）+ 导出原始 .eml
    if let Some(url) = &reading.unsubscribe {
        attachments_col = attachments_col.push(
            row![
                button(text(state.l().unsub).size(12))
                    .on_press(Message::Unsubscribe(url.clone()))
                    .padding([3, 8])
                    .style(theme::ghost_button),
                text(state.l().unsub_note)
                    .size(10)
                    .style(theme::secondary_text),
            ]
            .spacing(6),
        );
    }
    attachments_col = attachments_col.push(
        button(text(state.l().save_eml).size(12))
            .on_press(Message::ExportEmlRequest(*acct, hash))
            .padding([3, 8])
            .style(theme::ghost_button),
    );
    if !reading.attachments.is_empty() {
        attachments_col = attachments_col.push(
            text(crate::lang::f(
                state.l().attach_n,
                &[&reading.attachments.len()],
            ))
            .size(13),
        );
        for (name, size) in &reading.attachments {
            attachments_col = attachments_col.push(
                row![
                    text(format!("{}（{} B）", name, size)).size(12),
                    button(text(state.l().save).size(12))
                        .on_press(Message::SaveAttachment(*acct, hash, name.clone()))
                        .padding([2, 8]),
                ]
                .spacing(8),
            );
        }
    }

    // HTML 邮件卡片：随主题适配（深色用深底 + Dark 主题渲染 markdown；
    // 早期固定浅色是因为内容按 Light 解析会黑底黑字——根因是渲染主题写死，
    // 卡片底色与 markdown 主题一起切换后两态都自洽）
    let dark = matches!(state.ui_theme, iced::Theme::Dark);
    let (card_bg, card_fg, md_theme) = if dark {
        (
            iced::Color::from_rgb(0.13, 0.13, 0.15),
            iced::Color::from_rgb(0.92, 0.92, 0.94),
            iced::Theme::Dark,
        )
    } else {
        (
            iced::Color::from_rgb(1.0, 1.0, 0.98),
            iced::Color::from_rgb(0.1, 0.1, 0.12),
            iced::Theme::Light,
        )
    };
    let body_view: Element<Message> = match &reading.md {
        Some(md) => container(
            iced::widget::markdown::view(
                md.items(),
                iced::widget::markdown::Settings::with_text_size(14, md_theme),
            )
            .map(Message::LinkClicked),
        )
        .padding(12)
        .style(move |_theme| container::Style {
            background: Some(card_bg.into()),
            text_color: Some(card_fg),
            border: iced::border::rounded(8),
            ..Default::default()
        })
        .into(),
        None => text(reading.body.as_str()).size(13).into(),
    };

    // 信头元信息行：发件人 + 右对齐日期（Apple Mail 式；数据从列表行取）
    let meta_row = state
        .rows
        .iter()
        .find(|(a, r)| *a == *acct && r.envelope_hash == hash)
        .map(|(_, r)| {
            let name = r
                .from_addr
                .split('<')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let name = if name.is_empty() {
                r.from_addr.clone()
            } else {
                name
            };
            row![
                text(name)
                    .size(13)
                    .font(theme::ui_font(iced::font::Weight::Medium)),
                Space::new().width(Length::Fill),
                text(unix_to_local(r.date_unix))
                    .size(11)
                    .style(theme::secondary_text),
            ]
        });
    let mut reader_col = column![text(reading.subject.clone())
        .size(17)
        .font(theme::ui_font(iced::font::Weight::Semibold))]
    .spacing(10);
    if let Some(meta) = meta_row {
        reader_col = reader_col.push(meta);
    }
    // M5-c：PGP 状态徽章（语义着色：绿=好 红=坏 灰=未验证；符号前缀保色弱可辨）
    if let Some(badge) = &reading.pgp {
        reader_col = reader_col.push(
            container(text(badge.clone()).size(12))
                .padding([3, 8])
                .style(move |t| theme::status_badage(t, theme::badge_kind(badge))),
        );
    }
    // AI 摘要卡片（缓存命中且可见时）
    if state.ai.summary_visible {
        if let Some(sum) = state.ai.summary_cache.get(&(*acct, reading.hash)) {
            reader_col = reader_col.push(
                container(
                    row![
                        column![text(state.l().ai_summary_btn)
                            .size(12)
                            .font(theme::ui_font(iced::font::Weight::Semibold))]
                        .width(Length::Fill),
                        button(text(state.l().err_close).size(11))
                            .on_press(Message::AiSummaryClose)
                            .padding([2, 6])
                            .style(theme::ghost_button),
                    ]
                    .spacing(6),
                )
                .padding([6, 10])
                .width(Length::Fill)
                .style(theme::reader_meta),
            );
            reader_col = reader_col.push(
                container(text(sum.to_string()).size(12).style(theme::secondary_text))
                    .padding([2, 10])
                    .width(Length::Fill)
                    .style(theme::reader_meta),
            );
        }
    }
    // 动作区：极淡底圆角分组（proxity=关联；无硬分隔线）
    reader_col = reader_col.push(container(actions).style(theme::reader_meta));
    // M7：移动目标面板（该账号全部文件夹，排除当前文件夹）
    if move_open {
        let cur_folder = state
            .rows
            .iter()
            .find(|(a, r)| a == acct && r.envelope_hash == hash)
            .map(|(_, r)| r.folder.clone());
        let mut picks = column![].spacing(2);
        let mut n = 0;
        for (_fa, f) in state.folders.iter().filter(|(a, _)| a == acct) {
            if Some(&f.path) == cur_folder.as_ref() {
                continue;
            }
            n += 1;
            picks = picks.push(
                button(
                    text(format!(
                        "→ {}",
                        crate::lang::folder_display(state.l(), f.kind, &f.path)
                    ))
                    .size(12),
                )
                .on_press(Message::MoveToDest(*acct, hash, f.path.clone()))
                .padding([2, 8])
                .style(theme::ghost_button),
            );
        }
        if n == 0 {
            picks = picks.push(
                text(state.l().no_dest)
                    .size(12)
                    .style(theme::secondary_text),
            );
        }
        reader_col = reader_col.push(
            container(scrollable(picks).height(120))
                .padding(6)
                .style(theme::reader_meta),
        );
    }
    // 在垃圾文件夹浏览时提供「移回收件箱」（非垃圾快捷动作；v1 无自学习）
    let in_junk = state
        .rows
        .iter()
        .find(|(a, r)| a == acct && r.envelope_hash == hash)
        .map(|(_, r)| {
            let l = r.folder.to_lowercase();
            l.contains("junk") || l.contains("spam") || r.folder.contains("垃圾")
        })
        .unwrap_or(false);
    if in_junk {
        reader_col = reader_col.push(
            container(
                row![
                    button(text(state.l().not_junk).size(12))
                        .on_press(Message::MoveToDest(*acct, hash, "INBOX".into()))
                        .padding([3, 8])
                        .style(theme::accent_button),
                    Space::new().width(Length::Fill),
                ]
                .spacing(6),
            )
            .padding([4, 0])
            .width(Length::Fill)
            .style(theme::reader_meta),
        );
    }
    if let Some(c) = confirm {
        reader_col = reader_col.push(c);
    }
    // 图片区：内嵌图自动显示；远程图按需加载（隐私默认禁）
    if !reading.images.is_empty() {
        reader_col = reader_col.push(
            column(
                reading
                    .images
                    .iter()
                    .map(|h| {
                        Element::from(
                            container(iced::widget::image(h.clone()))
                                .padding(2)
                                .width(Length::Fill),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .spacing(4),
        );
    }
    if !reading.remote_pending.is_empty() {
        let label = if reading.remote_loading {
            state.l().img_loading.to_string()
        } else {
            crate::lang::f(state.l().show_remote, &[&reading.remote_pending.len()])
        };
        reader_col = reader_col.push(
            button(text(label).size(11))
                .on_press_maybe(if reading.remote_loading {
                    None
                } else {
                    Some(Message::LoadRemoteImages(*acct, hash))
                })
                .padding([3, 8])
                .style(theme::ghost_button),
        );
    }
    reader_col = reader_col.push(attachments_col).push(body_view);
    container(scrollable(reader_col))
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
