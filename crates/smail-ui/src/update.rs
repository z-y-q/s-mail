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

use iced::widget::text_editor;
use iced::Task;

use crate::{
    ai, build_account,
    compose::{self, open_compose_with, split_last_segment, text_editor_edit_paste, ReplyKind},
    extract_forward_attachments, handle_event, handle_key, notify, open_in_browser,
    persist_and_reload, scroll_list_to_top, setup, AiField, Compose, DispKind, Field, ListFilter,
    Message, Screen, SetupField, SetupForm, State, Theme, ROW_H,
};
use smail_core::{Cmd, Event, SessionCmd, TaggedEvent};

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
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::UnifiedInbox);
            }
            scroll_list_to_top()
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
        Message::Tray(action) => match action {
            crate::tray::TrayAction::Open => {
                // 通知跳转：macOS 通知点击 → 激活 app（走 tray Open 路径）
                // → 有最近通知的邮件则跳转过去
                if let Some(click) = crate::notify::take_last() {
                    return update(state, Message::NotificationClicked(click));
                }
                // iced 0.14 无窗口句柄；System Events 按进程名把自己拉到前台
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("osascript")
                    .arg("-e")
                    .arg(
                        "tell application \"System Events\" to set frontmost of first process whose name is \"s-mail\" to true",
                    )
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                Task::none()
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
                state.send(acct, Cmd::ListDrafts);
            } else if folder == "__outbox__" {
                state.nav.cur = Some((acct, folder));
                state.rows.clear();
                state.selected = None;
                state.reading = None;
                state.send(acct, Cmd::ListOutbox);
            } else {
                state.nav.cur = Some((acct, folder.clone()));
                state.rows.clear();
                state.nav.list_offset = 0.0;
                state.reading = None;
                state.selected = None; // 快捷键/阅读器不再指向旧文件夹邮件
                state.expanded.clear(); // 展开状态不跨文件夹（hash 可能同名冲突）
                state.send(acct, Cmd::SelectFolder(folder));
            }
            state.rebuild_display();
            state.nav.load_more_pending = false;
            state.nav.load_more_done = false;
            scroll_list_to_top()
        }
        Message::RowSelected(acct, hash) => {
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
        Message::SetupOpen => {
            state.screen = Screen::Setup(SetupForm::new());
            // 通讯录管理数据随设置屏拉取（按账号广播聚合）
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::ContactsFull);
            }
            Task::none()
        }
        Message::SetupClose => {
            // 允许零账号逛主页（空状态引导添加）
            state.screen = Screen::Mail;
            Task::none()
        }
        Message::SetupPreset(p) => {
            if let Screen::Setup(f) = &mut state.screen {
                f.preset = p;
                f.apply_preset();
            }
            Task::none()
        }
        Message::SetupField(field, value) => {
            if let Screen::Setup(f) = &mut state.screen {
                f.esc_guard = false; // 有编辑动作即重置 Esc 守卫
                match field {
                    SetupField::Name => f.name = value,
                    SetupField::Email => f.email = value,
                    SetupField::Password => f.password = value,
                    SetupField::ClientId => f.client_id = value,
                    SetupField::ClientSecret => f.client_secret = value,
                    SetupField::ImapHost => f.imap_host = value,
                    SetupField::ImapPort => f.imap_port = value,
                    SetupField::SmtpHost => f.smtp_host = value,
                    SetupField::SmtpPort => f.smtp_port = value,
                }
                f.test_result = None;
            }
            Task::none()
        }
        Message::SetupTest => {
            let Some(form) = (match &state.screen {
                Screen::Setup(f) => Some(f),
                _ => None,
            }) else {
                return Task::none();
            };
            if form.preset.is_oauth() {
                let msg = state.l().oauth_no_test.to_string();
                if let Screen::Setup(f) = &mut state.screen {
                    f.test_result = Some(Err(msg));
                }
                return Task::none();
            }
            let Ok(port) = form.imap_port.parse::<u16>() else {
                let msg = state.l().err_port.to_string();
                if let Screen::Setup(f) = &mut state.screen {
                    f.err = Some(msg);
                }
                return Task::none();
            };
            let cfg = smail_core::ImapConfig {
                host: form.imap_host.clone(),
                port,
                username: form.email.clone(),
                password: Some(form.password.clone()),
                starttls: port == 143,
                tls: port != 143,
                use_oauth2: false,
            };
            if let Screen::Setup(f) = &mut state.screen {
                f.testing = true;
            }
            // 阻塞连接测试丢给执行器线程（dogfood 取舍；不冻 UI）
            Task::perform(
                async move { smail_core::test_connection(&cfg) },
                Message::SetupTestResult,
            )
        }
        Message::SetupTestResult(res) => {
            if let Screen::Setup(f) = &mut state.screen {
                f.testing = false;
                f.test_result = Some(res);
            }
            Task::none()
        }
        Message::SetupSave => {
            let Some(form) = (match &state.screen {
                Screen::Setup(f) => Some(f),
                _ => None,
            }) else {
                return Task::none();
            };
            // 防重入：验证在途时重复点保存会在回包后推入双账号
            //（persist 失败不切屏时两个回包都能过重复检查）
            if form.save_pending {
                return Task::none();
            }
            // 表单校验（含重复邮箱检查）
            let acct = match build_account(form) {
                Ok(a) => a,
                Err(e) => {
                    if let Screen::Setup(f) = &mut state.screen {
                        f.err = Some(e);
                    }
                    return Task::none();
                }
            };
            if state
                .accounts
                .iter()
                .any(|a| a.imap.username == acct.imap.username)
            {
                let msg = crate::lang::f(state.l().err_dup, &[&acct.imap.username]);
                if let Screen::Setup(f) = &mut state.screen {
                    f.err = Some(msg);
                }
                return Task::none();
            }
            // OAuth 无法预验证（授权前连不上）→ 直接保存，连上时自动设备流
            // plain：首次必须连接验证通过；失败后主按钮变「仍要保存」再点一次放行
            if form.preset.is_oauth() || form.force_save {
                let mut accounts = state.accounts.clone();
                accounts.push(acct);
                if persist_and_reload(state, accounts, None) {
                    state.screen = Screen::Mail;
                }
                return Task::none();
            }
            if let Screen::Setup(f) = &mut state.screen {
                f.save_pending = true;
                f.err = None;
            }
            let cfg = acct.imap.clone();
            Task::perform(
                async move { smail_core::test_connection(&cfg) },
                Message::SetupSaveVerify,
            )
        }
        Message::SetupSaveVerify(res) => {
            let Some(form) = (match &state.screen {
                Screen::Setup(f) => Some(f),
                _ => None,
            }) else {
                return Task::none();
            };
            match res {
                Ok(_) => {
                    if let Ok(acct) = build_account(form) {
                        let mut accounts = state.accounts.clone();
                        accounts.push(acct);
                        if persist_and_reload(state, accounts, None) {
                            state.screen = Screen::Mail;
                        }
                    }
                }
                Err(e) => {
                    let msg = crate::lang::f(state.l().err_conn, &[&e]);
                    if let Screen::Setup(f) = &mut state.screen {
                        f.save_pending = false;
                        f.force_save = true;
                        f.err = Some(msg);
                    }
                }
            }
            Task::none()
        }
        Message::SetupDelete(i) => {
            // B6：先清理钥匙串条目（删完配置后 marker 无处可查，条目会孤儿化）。
            // 数据目录（index.db/bodies/meli cache/oauth token）由 persist_and_reload
            // 的后台 drop 线程在 Session join 完成后确定性删除（田版：睡 500ms 赌
            // Drop 完成的竞赛——慢网络下必输，目录泄漏）
            let delete_dir = state
                .accounts
                .get(i)
                .map(|a| std::path::PathBuf::from(&state.data_root).join(&a.slug));
            if let Some(a) = state.accounts.get(i) {
                let username = a.imap.username.clone();
                let mut mids: Vec<String> = vec![];
                if let Some(p) = &a.imap.password {
                    if smail_core::keychain::is_marker(p) {
                        mids.push(smail_core::keychain::account_key(&username, "imap"));
                    }
                }
                if let Some(s) = &a.smtp {
                    if let Some(p) = &s.password {
                        if smail_core::keychain::is_marker(p) {
                            mids.push(smail_core::keychain::account_key(&username, "smtp"));
                        }
                    }
                }
                for m in mids {
                    if let Err(e) = smail_core::keychain::delete(&m) {
                        state.error = Some(format!("钥匙串条目清理失败（可手动删）：{e}"));
                    }
                }
            }
            let mut accounts = state.accounts.clone();
            if i < accounts.len() {
                accounts.remove(i);
                persist_and_reload(state, accounts, delete_dir);
            }
            if state.accounts.is_empty() {
                state.screen = Screen::Setup(SetupForm::new());
            }
            Task::none()
        }
        Message::OAuthOpenUrl => {
            if let Some((url, _)) = &state.oauth_prompt {
                smail_core::oauth::open_url(url);
            }
            Task::none()
        }
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
        Message::NotificationClicked(click) => {
            // 跳转：切文件夹（本地先行立刻出列表）→ 打开正文
            let notify::NotifyClick { acct, folder, hash } = click;
            let _ = update(state, Message::FolderSelected(acct, folder));
            state.selected = Some((acct, hash));
            state.body_req = Some((acct, hash));
            state.send(acct, Cmd::OpenMessage(hash));
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
        Message::MarkUnread(acct, hash) => {
            state.send(acct, Cmd::MarkSeen(hash, false));
            Task::none()
        }
        Message::ToggleStar(acct, hash, on) => {
            state.send(acct, Cmd::ToggleFlagged(hash, on));
            Task::none()
        }
        Message::AskDelete(acct, hash) => {
            state.pending_delete = Some((acct, hash));
            Task::none()
        }
        Message::Reply(acct, hash) => {
            open_compose_with(state, acct, hash, ReplyKind::Reply);
            Task::none()
        }
        Message::ReplyAll(acct, hash) => {
            open_compose_with(state, acct, hash, ReplyKind::ReplyAll);
            Task::none()
        }
        Message::Forward(acct, hash) => {
            open_compose_with(state, acct, hash, ReplyKind::Forward);
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
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            extract_forward_attachments(&eml, &staging)
                        })
                        .await
                        .unwrap_or_default()
                    },
                    Message::ForwardAttachmentsReady,
                );
            }
            Task::none()
        }
        Message::ForwardAttachmentsReady(paths) => {
            if let Screen::Compose(c) = &mut state.screen {
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
            if let Some((a, f)) = state.nav.cur.clone() {
                if !state.nav.in_search && f != "__drafts__" {
                    state.send(a, Cmd::ForceRefresh(f));
                }
            }
            Task::none()
        }
        Message::SaveAttachment(acct, hash, name) => {
            state.send(acct, Cmd::SaveAttachment { hash, name });
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
                    state.screen = Screen::Compose(Compose {
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
                }
            }
            Task::none()
        }
        // ---------------- M7：移动 / 批量 / 文件夹 / 联系人 ----------------
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
        Message::AiSummarize(acct, hash) => ai::start_summarize(state, acct, hash),
        Message::AiSummaryReady {
            acct,
            hash,
            id,
            result,
        } => {
            ai::summary_ready(state, acct, hash, id, result);
            Task::none()
        }
        Message::AiSummaryClose => {
            state.ai.summary_visible = false;
            Task::none()
        }
        Message::AiDraftGo => {
            let instr = match &state.screen {
                Screen::Compose(c) => c.ai_instruction.clone(),
                _ => String::new(),
            };
            ai::start_draft(state, instr, None)
        }
        Message::AiDraftReady { id, result } => {
            ai::draft_ready(state, id, result);
            Task::none()
        }
        Message::AiDraftApply => {
            ai::draft_apply_overwrite(state);
            Task::none()
        }
        Message::AiDraftDiscard => {
            ai::draft_discard_overwrite(state);
            Task::none()
        }
        Message::AiInstructionInput(s) => {
            if let Screen::Compose(c) = &mut state.screen {
                c.ai_instruction = s;
            }
            Task::none()
        }
        Message::AiRewrite(instr) => ai::start_rewrite(state, instr),
        Message::AiDraftUndo => {
            ai::draft_undo(state);
            Task::none()
        }
        Message::AiSmartReply(acct, hash) => ai::smart_reply(state, acct, hash),
        Message::AiOrganizeGo => ai::organize_go(state),
        Message::AiPlanReady { id, result } => {
            ai::plan_ready(state, id, result);
            Task::none()
        }
        Message::AiPlanToggle(i) => {
            if let Some(groups) = &mut state.ai.plan {
                if let Some(g) = groups.get_mut(i) {
                    g.checked = !g.checked;
                }
            }
            Task::none()
        }
        Message::AiPlanExpand(i) => {
            if let Some(groups) = &mut state.ai.plan {
                if let Some(g) = groups.get_mut(i) {
                    g.expanded = !g.expanded;
                }
            }
            Task::none()
        }
        Message::AiPlanApply => {
            ai::plan_apply(state);
            Task::none()
        }
        Message::AiPlanCancel => {
            state.ai.plan = None;
            state.ai.plan_loading = false;
            state.ai.plan_ctx.clear();
            Task::none()
        }
        Message::AiPrivacyAck => {
            state.ai.ack = true;
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            let pending = state.ai.privacy_pending.take();
            match pending {
                Some(m) => update(state, m),
                None => Task::none(),
            }
        }
        Message::AiPrivacyCancel => {
            state.ai.privacy_pending = None;
            Task::none()
        }
        Message::AiFormOpen => {
            state.ai.form = Some(ai::form_from_cfg(state));
            Task::none()
        }
        Message::AiFormClose => {
            state.ai.form = None;
            Task::none()
        }
        Message::AiFormPreset(p) => {
            if let Some(f) = &mut state.ai.form {
                f.preset = p;
                f.apply_preset();
            }
            Task::none()
        }
        Message::AiFormToggleEnabled(v) => {
            if let Some(f) = &mut state.ai.form {
                f.enabled = v;
            }
            Task::none()
        }
        Message::AiFormField(field, value) => {
            if let Some(f) = &mut state.ai.form {
                f.err = None;
                match field {
                    AiField::BaseUrl => f.base_url = value,
                    AiField::Model => f.model = value,
                    AiField::ApiKey => f.api_key = value,
                }
            }
            Task::none()
        }
        Message::AiFormKeychain => {
            ai::form_keychain_move(state);
            Task::none()
        }
        Message::AiFormTest => {
            let Some(f) = &state.ai.form else {
                return Task::none();
            };
            if f.base_url.trim().is_empty() || f.model.trim().is_empty() {
                let err = "AI 需要填 API 地址与模型".to_string();
                if let Some(f) = &mut state.ai.form {
                    f.err = Some(err);
                }
                return Task::none();
            }
            // 表单值优先，api_key 空则回落已存配置（钥匙串标记场景）
            let raw_key = if f.api_key.trim().is_empty() {
                state.ai.cfg.api_key.clone()
            } else {
                f.api_key.trim().to_string()
            };
            let (base_url, model) = (f.base_url.trim().to_string(), f.model.trim().to_string());
            let chat = match if smail_core::keychain::is_marker(&raw_key) {
                smail_core::keychain::resolve(&raw_key)
            } else {
                Ok(raw_key)
            } {
                Ok(k) => smail_ai::Chat {
                    base_url,
                    api_key: k,
                    model,
                },
                Err(e) => {
                    if let Some(f) = &mut state.ai.form {
                        f.err = Some(e);
                    }
                    return Task::none();
                }
            };
            if let Some(f) = &mut state.ai.form {
                f.testing = true;
                f.test_result = None;
            }
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || chat.ping())
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                },
                Message::AiFormTestReady,
            )
        }
        Message::AiFormTestReady(res) => {
            if let Some(f) = &mut state.ai.form {
                f.testing = false;
                f.test_result = Some(res);
            }
            Task::none()
        }
        Message::AiFormSave => {
            ai::form_save(state);
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
        Message::ContactDelete(acct, id) => {
            state.send(acct, Cmd::DeleteContact(id));
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
                    to,
                    cc: String::new(),
                    bcc: String::new(),
                    subject: "Unsubscribe".into(),
                    body: text_editor::Content::new(),
                    attachments: vec![],
                });
            }
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
        Message::PgpEditOpen(i) => {
            // 预填现有配置（未配=全空表单）
            let f = match state.accounts.get(i).and_then(|a| a.pgp.as_ref()) {
                Some(p) => setup::PgpForm {
                    key_path: p.key_path.clone(),
                    passphrase: p.passphrase.clone().unwrap_or_default(),
                    pubring_path: p.pubring_path.clone().unwrap_or_default(),
                    wkd: p.wkd,
                    err: None,
                },
                None => setup::PgpForm {
                    key_path: String::new(),
                    passphrase: String::new(),
                    pubring_path: String::new(),
                    wkd: false,
                    err: None,
                },
            };
            state.pgp_edit = Some((i, f));
            Task::none()
        }
        Message::PgpEditWkd(on) => {
            if let Some((_, f)) = &mut state.pgp_edit {
                f.wkd = on;
            }
            Task::none()
        }
        Message::PgpEditField(field, value) => {
            if let Some((_, f)) = &mut state.pgp_edit {
                match field {
                    setup::PgpField::KeyPath => f.key_path = value,
                    setup::PgpField::Passphrase => f.passphrase = value,
                    setup::PgpField::PubringPath => f.pubring_path = value,
                }
                f.err = None;
            }
            Task::none()
        }
        Message::PgpEditPick(field) => Task::perform(
            async {
                rfd::AsyncFileDialog::new()
                    .set_title("选择 PGP 文件")
                    .pick_file()
                    .await
                    .map(|f| f.path().to_path_buf())
            },
            move |p| Message::PgpEditPicked(field, p),
        ),
        Message::PgpEditPicked(field, Some(path)) => {
            if let Some((_, f)) = &mut state.pgp_edit {
                let s = path.display().to_string();
                match field {
                    setup::PgpField::KeyPath => f.key_path = s,
                    setup::PgpField::PubringPath => f.pubring_path = s,
                    setup::PgpField::Passphrase => {}
                }
            }
            Task::none()
        }
        Message::PgpEditPicked(_, None) => Task::none(),
        Message::PgpEditSave => {
            let Some((i, f)) = state.pgp_edit.take() else {
                return Task::none();
            };
            let key = f.key_path.trim().to_string();
            let pubring = f.pubring_path.trim().to_string();
            if key.is_empty() && !pubring.is_empty() {
                state.pgp_edit = Some((
                    i,
                    setup::PgpForm {
                        err: Some("只填公钥环没有私钥：验签需要私钥配套，或两者都清空".into()),
                        ..f
                    },
                ));
                return Task::none();
            }
            let pgp = if key.is_empty() {
                None // 全空 = 移除 PGP
            } else {
                Some(smail_core::pgp::PgpCfg {
                    key_path: key,
                    passphrase: if f.passphrase.is_empty() {
                        None
                    } else {
                        Some(f.passphrase.clone())
                    },
                    pubring_path: if pubring.is_empty() {
                        None
                    } else {
                        Some(pubring)
                    },
                    wkd: f.wkd,
                })
            };
            let mut accounts = state.accounts.clone();
            if let Some(a) = accounts.get_mut(i) {
                a.pgp = pgp;
                // 会话重启（persist_and_reload）后生效；缓存随 ServiceInner 重建自动失效
                persist_and_reload(state, accounts, None);
            }
            Task::none()
        }
        Message::PgpEditCancel => {
            state.pgp_edit = None;
            Task::none()
        }
        Message::SignatureEditOpen(i) => {
            let sig = state
                .accounts
                .get(i)
                .and_then(|a| a.signature.clone())
                .unwrap_or_default();
            let mut content = text_editor::Content::new();
            if !sig.is_empty() {
                content.perform(compose::text_editor_edit_paste(&sig));
            }
            state.signature_edit = Some((i, content));
            Task::none()
        }
        Message::SignatureEditAction(action) => {
            if let Some((_, content)) = &mut state.signature_edit {
                content.perform(action);
            }
            Task::none()
        }
        Message::SignatureEditSave => {
            let Some((i, content)) = state.signature_edit.take() else {
                return Task::none();
            };
            let text = content.text().to_string();
            if let Some(a) = state.accounts.get_mut(i) {
                a.signature = if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                };
            }
            // 签名是纯客户端字段（compose 预填）：写盘即生效，无需重启会话
            let accounts = state.accounts.clone();
            if let Err(e) = smail_core::save_accounts(&state.cfg_path, &accounts) {
                state.error = Some(crate::lang::f(state.l().err_save_cfg, &[&e]));
            }
            Task::none()
        }
        Message::SignatureEditCancel => {
            state.signature_edit = None;
            Task::none()
        }
        Message::KeychainMove(i) => {
            let Some(a) = state.accounts.get(i) else {
                return Task::none();
            };
            let username = a.imap.username.clone();
            let plain = |p: &Option<String>| {
                p.as_deref()
                    .map(|s| !s.is_empty() && !smail_core::keychain::is_marker(s))
                    .unwrap_or(false)
            };
            let imap_plain = plain(&a.imap.password);
            let smtp_plain = a.smtp.as_ref().map(|s| plain(&s.password)).unwrap_or(false);
            if !imap_plain && !smtp_plain {
                state.error = Some(state.l().err_no_plain_pwd.into());
                return Task::none();
            }
            // 先入钥匙串，全部成功才改配置（失败一半时 config 明文仍在，不丢密码）
            let store_result = (|| -> Result<(), String> {
                if imap_plain {
                    let pw = a.imap.password.clone().unwrap_or_default();
                    smail_core::keychain::store(
                        &smail_core::keychain::account_key(&username, "imap"),
                        &pw,
                    )?;
                }
                if smtp_plain {
                    let pw = a
                        .smtp
                        .as_ref()
                        .and_then(|s| s.password.clone())
                        .unwrap_or_default();
                    smail_core::keychain::store(
                        &smail_core::keychain::account_key(&username, "smtp"),
                        &pw,
                    )?;
                }
                Ok(())
            })();
            match store_result {
                Ok(()) => {
                    let mut accounts = state.accounts.clone();
                    if let Some(a) = accounts.get_mut(i) {
                        if plain(&a.imap.password) {
                            a.imap.password =
                                Some(smail_core::keychain::marker_for(&username, "imap"));
                        }
                        if let Some(s) = &mut a.smtp {
                            if plain(&s.password) {
                                s.password =
                                    Some(smail_core::keychain::marker_for(&username, "smtp"));
                            }
                        }
                    }
                    // 会话重启后在 start_for 处从钥匙串解析回内存
                    persist_and_reload(state, accounts, None);
                }
                Err(e) => state.error = Some(format!("写入钥匙串失败：{e}")),
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
