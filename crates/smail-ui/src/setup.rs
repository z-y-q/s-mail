//! 账号配置视图（M6）
//!
//! ## 职责
//!
//! - **首次设置引导**：应用启动无配置时自动进入
//! - **服务商预设**：QQ/163/Gmail/Outlook 自动填充参数
//! - **表单验证**：邮箱格式、端口号、必填项检查
//! - **OAuth 授权**：设备流引导（显示 URL + 验证码）
//! - **配置持久化**：生成 config.toml + 写入系统钥匙串
//!
//! ## 服务商预设
//!
//! | 预设 | IMAP | SMTP | 认证方式 |
//! |------|------|------|---------|
//! | QQ 邮箱 | imap.qq.com:993 | smtp.qq.com:465 | 密码/授权码 |
//! | 163 邮箱 | imap.163.com:993 | smtp.163.com:465 | 密码/授权码 |
//! | Gmail | imap.gmail.com:993 | smtp.gmail.com:465 | OAuth 2.0 |
//! | Outlook | outlook.office365.com:993 | smtp.office365.com:587 | OAuth 2.0 |
//! | 自定义 | 手动填写 | 手动填写 | 密码/OAuth |
//!
//! ## 表单字段
//!
//! - **基础信息**：姓名、邮箱地址
//! - **IMAP 配置**：服务器、端口、TLS、用户名、密码
//! - **SMTP 配置**：服务器、端口、TLS、用户名、密码
//! - **OAuth**（Gmail/Outlook）：自动触发设备流授权

use iced::widget::{button, column, container, row, scrollable, text, text_input, Space};
use iced::{Element, Length};

use crate::{theme, Message, State};

/// 服务商预设（自动填 host/port/TLS/oauth scope）
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Preset {
    QQ,
    N163,
    Gmail,
    Outlook,
    Custom,
}

impl Preset {
    fn title(&self) -> &'static str {
        match self {
            Preset::QQ => "QQ 邮箱",
            Preset::N163 => "163 邮箱",
            Preset::Gmail => "Gmail",
            Preset::Outlook => "Outlook",
            Preset::Custom => "自定义",
        }
    }
    pub(crate) fn is_oauth(&self) -> bool {
        matches!(self, Preset::Gmail | Preset::Outlook)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PgpField {
    KeyPath,
    Passphrase,
    PubringPath,
}

/// 账号 PGP 内联编辑表单（空字段=不设；三字段全空=移除 PGP）
pub(crate) struct PgpForm {
    pub(crate) key_path: String,
    pub(crate) passphrase: String,
    pub(crate) pubring_path: String,
    /// WKD 自动发现公钥（收件人缺公钥时向其域名查询）
    pub(crate) wkd: bool,
    pub(crate) err: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SetupField {
    Name,
    Email,
    Password,
    ClientId,
    ClientSecret,
    ImapHost,
    ImapPort,
    SmtpHost,
    SmtpPort,
}

pub(crate) struct SetupForm {
    pub(crate) preset: Preset,
    pub(crate) name: String,
    pub(crate) email: String,
    pub(crate) password: String,
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
    pub(crate) imap_host: String,
    pub(crate) imap_port: String,
    pub(crate) smtp_host: String,
    pub(crate) smtp_port: String,
    /// None=未测/测试中；Some(Ok(n))/Some(Err(e))
    pub(crate) test_result: Option<std::result::Result<usize, String>>,
    pub(crate) err: Option<String>,
    pub(crate) testing: bool,
    /// 保存前自动验证进行中
    pub(crate) save_pending: bool,
    /// 验证失败后置位：主按钮变「仍要保存」，再点一次直接保存
    pub(crate) force_save: bool,
    /// Esc 退出守卫：表单有内容时首次 Esc 仅提示，再按才退出（防习惯性 Esc 丢表单）
    pub(crate) esc_guard: bool,
}

impl SetupForm {
    pub(crate) fn new() -> Self {
        Self {
            preset: Preset::QQ,
            name: String::new(),
            email: String::new(),
            password: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            imap_host: String::new(),
            imap_port: String::new(),
            smtp_host: String::new(),
            smtp_port: String::new(),
            test_result: None,
            err: None,
            testing: false,
            save_pending: false,
            force_save: false,
            esc_guard: false,
        }
    }
    pub(crate) fn apply_preset(&mut self) {
        let (ih, ip, sh, sp) = match self.preset {
            Preset::QQ => ("imap.qq.com", "993", "smtp.qq.com", "465"),
            Preset::N163 => ("imap.163.com", "993", "smtp.163.com", "465"),
            Preset::Gmail => ("imap.gmail.com", "993", "smtp.gmail.com", "465"),
            Preset::Outlook => ("outlook.office365.com", "993", "smtp.office365.com", "587"),
            Preset::Custom => return,
        };
        self.imap_host = ih.into();
        self.imap_port = ip.into();
        self.smtp_host = sh.into();
        self.smtp_port = sp.into();
        self.test_result = None;
    }
}

/// 标签 + 输入框整行（唯一实现——compose/setup 共用，宽度类 bug 只会发生一次）
pub(crate) fn labeled_field<'a>(
    label: &'a str,
    label_w: f32,
    placeholder: &'a str,
    value: &'a str,
    secure: bool,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    row![
        container(text(label.to_string()).size(12))
            .width(Length::Fixed(label_w))
            .align_y(iced::alignment::Vertical::Center)
            .style(|t| {
                let mut st = theme::reader_meta(t);
                st.border = iced::border::rounded(6);
                st
            })
            .padding([4, 6]),
        text_input(placeholder, value)
            .on_input(on_input)
            .size(13)
            .padding(6)
            .secure(secure)
            .width(Length::Fill),
    ]
    .spacing(8)
    .width(Length::Fill)
    .into()
}

fn setup_field<'a>(
    label: &'a str,
    placeholder: &'a str,
    value: &'a str,
    fld: SetupField,
    secure: bool,
) -> Element<'a, Message> {
    labeled_field(label, 72.0, placeholder, value, secure, move |v| {
        Message::SetupField(fld, v)
    })
}

/// M6：账号配置屏（首次运行引导 / 管理）
pub(crate) fn setup_view<'a>(state: &'a State, f: &'a SetupForm) -> Element<'a, Message> {
    let first_run = state.accounts.is_empty();

    let presets = [
        Preset::QQ,
        Preset::N163,
        Preset::Gmail,
        Preset::Outlook,
        Preset::Custom,
    ];
    let mut preset_row = row![].spacing(6);
    for p in presets {
        let active = f.preset == p;
        let title = if matches!(p, Preset::Custom) {
            state.l().preset_custom
        } else {
            p.title()
        };
        preset_row = preset_row.push(
            button(text(title).size(13))
                .on_press(Message::SetupPreset(p))
                .padding([5, 10])
                .style(move |t, st| {
                    if active {
                        theme::accent_button(t, st)
                    } else {
                        theme::ghost_button(t, st)
                    }
                }),
        );
    }

    let mut form_col = column![
        text(if first_run {
            state.l().welcome
        } else {
            state.l().add_hdr
        })
        .size(20)
        .font(theme::ui_font(iced::font::Weight::Semibold)),
        preset_row,
        setup_field(
            state.l().email_label,
            "you@example.com",
            &f.email,
            SetupField::Email,
            false
        ),
        setup_field(
            state.l().name_label,
            state.l().name_ph,
            &f.name,
            SetupField::Name,
            false
        ),
    ]
    .spacing(10);

    if f.preset.is_oauth() {
        form_col = form_col
            .push(
                text(state.l().oauth_note)
                    .size(12)
                    .style(theme::secondary_text),
            )
            .push(setup_field(
                "client_id",
                "Google Cloud / Entra ID",
                &f.client_id,
                SetupField::ClientId,
                false,
            ))
            .push(setup_field(
                "client_secret",
                state.l().secret_ph,
                &f.client_secret,
                SetupField::ClientSecret,
                true,
            ));
    } else {
        form_col = form_col.push(setup_field(
            state.l().pwd_label,
            state.l().pwd_ph,
            &f.password,
            SetupField::Password,
            true,
        ));
    }
    form_col = form_col
        .push(setup_field(
            state.l().imap_label,
            "imap.example.com",
            &f.imap_host,
            SetupField::ImapHost,
            false,
        ))
        .push(setup_field(
            state.l().imap_port_label,
            "993",
            &f.imap_port,
            SetupField::ImapPort,
            false,
        ))
        .push(setup_field(
            state.l().smtp_label,
            "smtp.example.com",
            &f.smtp_host,
            SetupField::SmtpHost,
            false,
        ))
        .push(setup_field(
            state.l().smtp_port_label,
            "465",
            &f.smtp_port,
            SetupField::SmtpPort,
            false,
        ));

    match &f.test_result {
        Some(Ok(n)) => {
            form_col = form_col.push(
                container(text(crate::lang::f(state.l().test_ok, &[&n])).size(12))
                    .padding([3, 8])
                    .style(|t| theme::status_badage(t, theme::BadgeKind::Good)),
            );
        }
        Some(Err(e)) => {
            form_col = form_col.push(
                container(text(format!("✗ {e}")).size(12))
                    .padding([3, 8])
                    .style(|t| theme::status_badage(t, theme::BadgeKind::Bad)),
            );
        }
        None => {}
    }
    if let Some(e) = &f.err {
        form_col = form_col.push(
            container(text(e.clone()).size(12))
                .padding([3, 8])
                .style(|t| theme::status_badage(t, theme::BadgeKind::Bad)),
        );
    }
    if f.esc_guard {
        form_col = form_col.push(
            container(text(state.l().esc_again).size(12))
                .padding([3, 8])
                .style(|t| theme::status_badage(t, theme::BadgeKind::Warn)),
        );
    }

    let save_label = if f.save_pending {
        state.l().verifying
    } else if f.force_save {
        state.l().force_save
    } else {
        state.l().save_use
    };
    let mut actions = row![
        button(
            text(if f.testing {
                state.l().testing
            } else {
                state.l().test_conn
            })
            .size(13)
        )
        .on_press_maybe(if f.testing || f.save_pending {
            None
        } else {
            Some(Message::SetupTest)
        })
        .padding([5, 12])
        .style(theme::ghost_button),
        button(text(save_label).size(13))
            .on_press_maybe(if f.save_pending {
                None
            } else {
                Some(Message::SetupSave)
            })
            .padding([5, 14])
            .style(theme::accent_button),
    ]
    .spacing(8);
    actions = actions.push(
        button(
            text(if first_run {
                state.l().look_around
            } else {
                state.l().back
            })
            .size(13),
        )
        .on_press(Message::SetupClose)
        .padding([5, 12])
        .style(theme::ghost_button),
    );

    let mut list_col = column![text(state.l().have_accounts)
        .size(13)
        .font(theme::ui_font(iced::font::Weight::Semibold))]
    .spacing(4);
    for (i, a) in state.accounts.iter().enumerate() {
        let pgp_on = a.pgp.is_some();
        // B6：任一密码已是 keychain 标记即视为已入钥匙串
        let kc_on = a
            .imap
            .password
            .as_deref()
            .map(smail_core::keychain::is_marker)
            .unwrap_or(false)
            || a.smtp
                .as_ref()
                .and_then(|s| s.password.as_deref())
                .map(smail_core::keychain::is_marker)
                .unwrap_or(false);
        list_col = list_col.push(
            row![
                text(format!(
                    "{} · {}{}{}",
                    a.name,
                    a.imap.username,
                    if pgp_on { " · PGP✓" } else { "" },
                    if kc_on { " · 🔑✓" } else { "" },
                ))
                .size(12),
                Space::new().width(Length::Fill),
                button(text(if kc_on { "🔑✓" } else { "🔑" }).size(12))
                    .on_press(Message::KeychainMove(i))
                    .padding([2, 8])
                    .style(move |t, st| {
                        if kc_on {
                            theme::accent_button(t, st)
                        } else {
                            theme::ghost_button(t, st)
                        }
                    }),
                button(text("PGP").size(12))
                    .on_press(Message::PgpEditOpen(i))
                    .padding([2, 8])
                    .style(move |t, st| {
                        if pgp_on {
                            theme::accent_button(t, st)
                        } else {
                            theme::ghost_button(t, st)
                        }
                    }),
                button(text(state.l().signature_btn).size(12))
                    .on_press(Message::SignatureEditOpen(i))
                    .padding([2, 8])
                    .style(theme::ghost_button),
                button(text(state.l().delete_short).size(12))
                    .on_press(Message::SetupDelete(i))
                    .padding([2, 8])
                    .style(theme::ghost_button),
            ]
            .spacing(8),
        );
        // PGP 内联编辑（同文件夹新建/改名的两态模式）
        if let Some((ea, f)) = &state.pgp_edit {
            if *ea == i {
                list_col = list_col.push(pgp_edit_form(state.l(), f));
            }
        }
        // 签名内联编辑（多行 text_editor；空=移除）
        if let Some((sa, content)) = &state.signature_edit {
            if *sa == i {
                list_col = list_col.push(
                    container(
                        column![
                            text(state.l().signature_label).size(11),
                            iced::widget::text_editor(content)
                                .on_action(Message::SignatureEditAction)
                                .height(80)
                                .padding(6),
                            row![
                                button(text(state.l().save).size(12))
                                    .on_press(Message::SignatureEditSave)
                                    .padding([3, 10])
                                    .style(theme::accent_button),
                                button(text(state.l().cancel).size(12))
                                    .on_press(Message::SignatureEditCancel)
                                    .padding([3, 8])
                                    .style(theme::ghost_button),
                            ]
                            .spacing(8),
                        ]
                        .spacing(6),
                    )
                    .padding(8)
                    .width(Length::Fill)
                    .style(theme::reader_meta),
                );
            }
        }
    }

    // 通讯录（收信自动收集；按账号分组展示 + 删除）
    let mut contacts_col = column![text(state.l().contacts_hdr)
        .size(13)
        .font(theme::ui_font(iced::font::Weight::Semibold))]
    .spacing(4);
    if state.contacts_full.is_empty() {
        contacts_col = contacts_col.push(
            text(state.l().no_contacts)
                .size(11)
                .style(theme::secondary_text),
        );
    }
    for (acct, rows) in &state.contacts_full {
        let acct_name = state
            .account_names
            .get(*acct)
            .cloned()
            .unwrap_or_else(|| crate::lang::f(state.l().acct_n, &[&acct]));
        contacts_col = contacts_col.push(
            text(crate::lang::f(
                state.l().contacts_n,
                &[&acct_name, &rows.len()],
            ))
            .size(11)
            .style(theme::secondary_text),
        );
        for c in rows.iter().take(50) {
            let display = if c.name.is_empty() {
                c.email.clone()
            } else {
                format!("{} <{}>", c.name, c.email)
            };
            contacts_col = contacts_col.push(
                row![
                    text(display).size(12),
                    Space::new().width(Length::Fill),
                    button(text(state.l().delete_short).size(10))
                        .on_press(Message::ContactDelete(*acct, c.id))
                        .padding([1, 6])
                        .style(theme::ghost_button),
                ]
                .spacing(4),
            );
        }
        if rows.len() > 50 {
            contacts_col = contacts_col.push(
                text(crate::lang::f(state.l().contacts_more, &[&rows.len()]))
                    .size(10)
                    .style(theme::secondary_text),
            );
        }
    }

    // ---- AI 助手（独立 ai.toml；执行边界见 smail-ai crate 注释） ----
    let mut ai_col = column![text(state.l().ai_section)
        .size(13)
        .font(theme::ui_font(iced::font::Weight::Semibold))]
    .spacing(4);
    let ai_status = if state.ai.cfg.enabled && !state.ai.cfg.model.is_empty() {
        crate::lang::f(state.l().ai_status_on, &[&state.ai.cfg.model])
    } else {
        state.l().ai_status_off.to_string()
    };
    ai_col = ai_col.push(
        row![
            text(ai_status).size(11).style(theme::secondary_text),
            Space::new().width(Length::Fill),
            button(
                text(if state.ai.form.is_some() {
                    state.l().cancel
                } else {
                    state.l().ai_setup
                })
                .size(12)
            )
            .on_press(if state.ai.form.is_some() {
                Message::AiFormClose
            } else {
                Message::AiFormOpen
            })
            .padding([2, 8])
            .style(theme::ghost_button),
        ]
        .spacing(8),
    );
    if let Some(f) = &state.ai.form {
        let mut preset_row = row![].spacing(6);
        for p in smail_ai::Preset::ALL {
            let active = f.preset == p;
            let title = if matches!(p, smail_ai::Preset::Custom) {
                state.l().preset_custom
            } else {
                p.title()
            };
            preset_row = preset_row.push(
                button(text(title).size(12))
                    .on_press(Message::AiFormPreset(p))
                    .padding([3, 8])
                    .style(move |t, st| {
                        if active {
                            theme::accent_button(t, st)
                        } else {
                            theme::ghost_button(t, st)
                        }
                    }),
            );
        }
        let mut form = column![preset_row].spacing(6);
        form = form.push(
            row![
                iced::widget::checkbox(f.enabled)
                    .label(state.l().ai_enabled_label)
                    .on_toggle(Message::AiFormToggleEnabled)
                    .size(13)
                    .text_size(12),
                Space::new().width(Length::Fill),
            ]
            .spacing(6),
        );
        form = form.push(labeled_field(
            state.l().ai_base_url_label,
            72.0,
            "https://api.openai.com/v1",
            &f.base_url,
            false,
            move |v| Message::AiFormField(crate::AiField::BaseUrl, v),
        ));
        form = form.push(labeled_field(
            state.l().ai_model_label,
            72.0,
            "gpt-4o-mini",
            &f.model,
            false,
            move |v| Message::AiFormField(crate::AiField::Model, v),
        ));
        // API Key（secure；空=保留原值；🔑 迁入钥匙串）
        let key_ph = if smail_core::keychain::is_marker(&state.ai.cfg.api_key) {
            "🔑 已在钥匙串（留空保留）"
        } else {
            "sk-…"
        };
        form = form.push(
            row![
                labeled_field(
                    state.l().ai_api_key_label,
                    72.0,
                    key_ph,
                    &f.api_key,
                    true,
                    move |v| Message::AiFormField(crate::AiField::ApiKey, v),
                ),
                button(text(state.l().ai_keychain_btn).size(11))
                    .on_press(Message::AiFormKeychain)
                    .padding([3, 8])
                    .style(theme::ghost_button),
            ]
            .spacing(6),
        );
        match &f.test_result {
            Some(Ok(r)) => {
                form = form.push(
                    container(text(crate::lang::f(state.l().ai_test_ok, &[r])).size(12))
                        .padding([3, 8])
                        .style(|t| theme::status_badage(t, theme::BadgeKind::Good)),
                );
            }
            Some(Err(e)) => {
                form = form.push(
                    container(text(format!("✗ {e}")).size(12))
                        .padding([3, 8])
                        .style(|t| theme::status_badage(t, theme::BadgeKind::Bad)),
                );
            }
            None => {}
        }
        if let Some(e) = &f.err {
            form = form.push(
                container(text(e.clone()).size(12))
                    .padding([3, 8])
                    .style(|t| theme::status_badage(t, theme::BadgeKind::Bad)),
            );
        }
        form = form.push(
            row![
                button(
                    text(if f.testing {
                        state.l().testing
                    } else {
                        state.l().test_conn
                    })
                    .size(13)
                )
                .on_press_maybe(if f.testing {
                    None
                } else {
                    Some(Message::AiFormTest)
                })
                .padding([4, 12])
                .style(theme::ghost_button),
                button(text(state.l().save_use).size(13))
                    .on_press(Message::AiFormSave)
                    .padding([4, 14])
                    .style(theme::accent_button),
                Space::new().width(Length::Fill),
            ]
            .spacing(8),
        );
        ai_col = ai_col.push(container(form).padding(8).style(theme::reader_meta));
    }

    container(scrollable(
        column![
            column![form_col, actions].spacing(12),
            Space::new().height(16),
            list_col,
            Space::new().height(10),
            contacts_col,
            Space::new().height(10),
            ai_col,
            text(state.l().cfg_note)
                .size(11)
                .style(theme::secondary_text),
        ]
        .spacing(8)
        .max_width(560),
    ))
    .center_x(Length::Fill)
    .padding(24)
    .into()
}

/// PGP 内联编辑表单（三字段 + 文件选择 + 保存/取消）
fn pgp_edit_form<'a>(l: &crate::lang::Lex, f: &'a PgpForm) -> Element<'a, Message> {
    let mut col = column![
        text(l.pgp_note).size(11).style(theme::secondary_text),
        labeled_field(l.key_label, 72.0, l.key_ph, &f.key_path, false, move |v| {
            Message::PgpEditField(PgpField::KeyPath, v)
        }),
        row![button(text(l.pick_file).size(11))
            .on_press(Message::PgpEditPick(PgpField::KeyPath))
            .padding([2, 8])
            .style(theme::ghost_button),]
        .spacing(4),
        labeled_field(
            l.passphrase_label,
            72.0,
            l.passphrase_ph,
            &f.passphrase,
            true,
            move |v| Message::PgpEditField(PgpField::Passphrase, v)
        ),
        labeled_field(
            l.pubring_label,
            72.0,
            l.pubring_ph,
            &f.pubring_path,
            false,
            move |v| Message::PgpEditField(PgpField::PubringPath, v)
        ),
        row![iced::widget::checkbox(f.wkd)
            .label(l.wkd_label)
            .on_toggle(Message::PgpEditWkd)
            .size(12)
            .text_size(11),],
        row![
            button(text(l.pick_file).size(11))
                .on_press(Message::PgpEditPick(PgpField::PubringPath))
                .padding([2, 8])
                .style(theme::ghost_button),
            Space::new().width(Length::Fill),
            button(text(l.save).size(12))
                .on_press(Message::PgpEditSave)
                .padding([3, 10])
                .style(theme::accent_button),
            button(text(l.cancel).size(12))
                .on_press(Message::PgpEditCancel)
                .padding([3, 6]),
        ]
        .spacing(4),
    ]
    .spacing(6);
    if let Some(e) = &f.err {
        col = col.push(
            container(text(e.clone()).size(11))
                .padding([3, 8])
                .style(|t| theme::status_badage(t, theme::BadgeKind::Bad)),
        );
    }
    container(col)
        .padding(8)
        .width(Length::Fill)
        .style(theme::reader_meta)
        .into()
}

/// M6：表单 → AccountConfig（校验失败给人话错误）
pub(crate) fn build_account(
    f: &SetupForm,
) -> std::result::Result<smail_core::AccountConfig, String> {
    let email = f.email.trim().to_string();
    if !email.contains('@') || email.len() < 5 {
        return Err("邮箱地址无效".into());
    }
    if f.preset.is_oauth() && f.client_id.trim().is_empty() {
        return Err("OAuth 需要 client_id（Google Cloud / Entra ID 控制台创建）".into());
    }
    if !f.preset.is_oauth() && f.password.is_empty() {
        return Err("密码/授权码不能为空".into());
    }
    let imap_port: u16 = f
        .imap_port
        .parse()
        .map_err(|_| "IMAP 端口无效".to_string())?;
    let smtp_port: u16 = f
        .smtp_port
        .parse()
        .map_err(|_| "SMTP 端口无效".to_string())?;
    if f.imap_host.trim().is_empty() || f.smtp_host.trim().is_empty() {
        return Err("服务器地址不能为空".into());
    }
    let name = if f.name.trim().is_empty() {
        email.split('@').next().unwrap_or("账号").to_string()
    } else {
        f.name.trim().to_string()
    };
    let oauth = match f.preset {
        Preset::Gmail => Some(smail_core::oauth::OAuthCfg {
            provider: "google".into(),
            client_id: f.client_id.trim().into(),
            client_secret: Some(f.client_secret.trim().into()),
            scopes: vec!["https://mail.google.com/".into()],
            tenant: None,
            device_endpoint: None,
            token_endpoint: None,
        }),
        Preset::Outlook => Some(smail_core::oauth::OAuthCfg {
            provider: "microsoft".into(),
            client_id: f.client_id.trim().into(),
            client_secret: if f.client_secret.trim().is_empty() {
                None
            } else {
                Some(f.client_secret.trim().into())
            },
            scopes: vec![
                "https://outlook.office365.com/IMAP.AccessAsUser.All".into(),
                "https://outlook.office365.com/SMTP.Send".into(),
            ],
            tenant: Some("consumers".into()),
            device_endpoint: None,
            token_endpoint: None,
        }),
        _ => None,
    };
    let pw = if f.preset.is_oauth() {
        None
    } else {
        Some(f.password.clone())
    };
    let smtp_tls = match smtp_port {
        465 => "ssl",
        587 => "starttls",
        _ => "none",
    };
    let cfg = smail_core::AccountConfig {
        name: name.clone(),
        slug: smail_core::slug_for(&name, &email, 0), // 重载后 load_accounts 会去重
        identity: email.clone(),
        imap: smail_core::ImapConfig {
            host: f.imap_host.trim().into(),
            port: imap_port,
            username: email.clone(),
            password: pw.clone(),
            starttls: imap_port == 143,
            tls: imap_port != 143,
            use_oauth2: false,
        },
        smtp: Some(smail_core::SmtpConfig {
            host: f.smtp_host.trim().into(),
            port: smtp_port,
            username: email,
            password: pw,
            from: Some(f.email.trim().into()),
            tls: Some(smtp_tls.into()),
            auth: None,
        }),
        oauth,
        pgp: None,
        rules: vec![],
        signature: None,
        sync_config: Default::default(),
    };
    Ok(cfg)
}
