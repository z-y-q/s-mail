//! 设置屏消息：账号增删改/测试连接/OAuth 设备流/PGP 内联编辑/签名/钥匙串

use crate::{
    build_account,
    compose::{self},
    persist_and_reload, setup, Message, Screen, SetupField, SetupForm, State,
};
use iced::widget::text_editor;
use iced::Task;
use smail_core::SessionCmd;

/// 消息组的统一入口（分发器按 or-pattern 委托；模式与处理体自 update.rs 原样搬运）
pub(crate) fn handle(state: &mut State, msg: Message) -> Task<Message> {
    match msg {
        Message::SetupOpen => {
            state.screen = Screen::Setup(SetupForm::new());
            // 原生网页视图浮在一切画面之上：离开邮件屏必须藏（回来自动恢复）
            crate::webview::set_shown(state, false);
            // 通讯录管理数据随设置屏拉取（按账号广播聚合）
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::ContactsFull);
            }
            Task::none()
        }
        Message::SetupClose => {
            // 允许零账号逛主页（空状态引导添加）
            state.screen = Screen::Mail;
            // 回邮件屏：恢复原样渲染视图并对齐几何
            crate::webview::set_shown(state, true);
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
        Message::PgpEditWkd(on) => {
            if let Some((_, f)) = &mut state.pgp_edit {
                f.wkd = on;
            }
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
        Message::SyncEditOpen(i) => {
            // 预填当前间隔（秒）
            let cur = state
                .accounts
                .get(i)
                .map(|a| a.sync_config.folder_resync_ttl_secs.to_string())
                .unwrap_or_default();
            state.sync_edit = Some((i, cur));
            Task::none()
        }
        Message::SyncEditInput(v) => {
            if let Some((_, s)) = &mut state.sync_edit {
                *s = v;
            }
            Task::none()
        }
        Message::SyncEditSave => {
            let Some((i, raw)) = state.sync_edit.take() else {
                return Task::none();
            };
            // 快速失败：非正整数就地报错不落盘
            let Ok(secs) = raw.trim().parse::<u64>() else {
                state.error = Some(state.l().err_sync_interval.into());
                return Task::none();
            };
            if secs == 0 {
                state.error = Some(state.l().err_sync_interval.into());
                return Task::none();
            }
            let mut accounts = state.accounts.clone();
            if let Some(a) = accounts.get_mut(i) {
                a.sync_config.folder_resync_ttl_secs = secs;
                // 会话重启后服务线程读新 TTL；本地索引直读不受影响
                persist_and_reload(state, accounts, None);
            }
            Task::none()
        }
        Message::SyncEditCancel => {
            state.sync_edit = None;
            Task::none()
        }
        // ---------------- 过滤规则编辑 ----------------
        Message::RuleEditOpen(i) => {
            // 载入该账号规则草稿；高级规则保留原结构（只读显示）
            let drafts: Vec<setup::RuleDraft> = state
                .accounts
                .get(i)
                .map(|a| {
                    a.rules
                        .iter()
                        .map(|r| setup::RuleDraft::from_rule(r.clone()))
                        .collect()
                })
                .unwrap_or_default();
            state.rule_edit = Some((i, drafts));
            Task::none()
        }
        Message::RuleEditField(row, idx) => {
            if let Some((_, rows)) = &mut state.rule_edit {
                if let Some(d) = rows.get_mut(row) {
                    if setup::RuleDraft::editable(&d.rule) {
                        d.set_field(idx);
                    }
                }
            }
            Task::none()
        }
        Message::RuleEditPredicate(row, idx) => {
            if let Some((_, rows)) = &mut state.rule_edit {
                if let Some(d) = rows.get_mut(row) {
                    if setup::RuleDraft::editable(&d.rule) {
                        d.set_predicate(idx);
                    }
                }
            }
            Task::none()
        }
        Message::RuleEditValue(row, v) => {
            if let Some((_, rows)) = &mut state.rule_edit {
                if let Some(d) = rows.get_mut(row) {
                    if setup::RuleDraft::editable(&d.rule) {
                        d.set_value(v);
                    }
                }
            }
            Task::none()
        }
        Message::RuleEditAction(row, idx) => {
            if let Some((_, rows)) = &mut state.rule_edit {
                if let Some(d) = rows.get_mut(row) {
                    if setup::RuleDraft::editable(&d.rule) {
                        d.set_action(idx);
                    }
                }
            }
            Task::none()
        }
        Message::RuleEditMoveDest(row, dest) => {
            if let Some((_, rows)) = &mut state.rule_edit {
                if let Some(d) = rows.get_mut(row) {
                    d.set_move_dest(dest);
                }
            }
            Task::none()
        }
        Message::RuleEditAdd => {
            if let Some((_, rows)) = &mut state.rule_edit {
                rows.push(setup::rule_template());
            }
            Task::none()
        }
        Message::RuleEditDelete(row) => {
            if let Some((_, rows)) = &mut state.rule_edit {
                if row < rows.len() {
                    rows.remove(row);
                }
            }
            Task::none()
        }
        Message::RuleEditSave => {
            // 先在克隆上校验并归一化（trim 等），失败保留面板就地报错
            let Some((i, rows)) = &state.rule_edit else {
                return Task::none();
            };
            let mut normalized = rows.clone();
            for (n, d) in normalized.iter_mut().enumerate() {
                if let Err(e) = d.validate() {
                    state.error = Some(format!("规则 {}：{e}", n + 1));
                    return Task::none();
                }
            }
            let rules: Vec<smail_core::Rule> = normalized.iter().map(|d| d.rule.clone()).collect();
            let mut accounts = state.accounts.clone();
            if let Some(a) = accounts.get_mut(*i) {
                a.rules = rules;
                // 规则在同步时应用（apply_rules 读账号配置）：会话重启生效
                persist_and_reload(state, accounts, None);
            }
            state.rule_edit = None;
            Task::none()
        }
        Message::RuleEditCancel => {
            state.rule_edit = None;
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
        _ => unreachable!("update 分发器把非本组消息路由到了 account_setup：{msg:?}"),
    }
}
