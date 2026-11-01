//! UI 状态机回归测试：历轮人工排查修掉的 bug 类全部固化成用例，
//! 防止后续重构复发。覆盖：视图切换残留清理、异步迟到守卫、
//! 死引用清理、翻页触发条件、整理计划白名单校验。

use crate::{
    ai, update, BatchOps, FolderEdit, ListFilter, Message, Nav, Reading, RowEntry, Screen, State,
};
use smail_core::{Event, FolderInfo, FolderKind};
use smail_store::MessageRow;

fn row(hash: i64, seen: bool, flagged: bool) -> MessageRow {
    MessageRow {
        envelope_hash: hash,
        folder: "INBOX".into(),
        message_id: String::new(),
        subject: format!("s{hash}"),
        from_addr: "a@b.c".into(),
        date_unix: hash,
        seen,
        flagged,
        has_body: true,
        in_reply_to: String::new(),
        preview: format!("第{hash}封邮件的正文预览"),
        size: 0,
        att_count: 0,
    }
}

fn folder(path: &str, kind: FolderKind) -> FolderInfo {
    FolderInfo {
        path: path.into(),
        kind,
        unseen: 0,
        total: 0,
    }
}

/// 无会话测试态（send 全部 no-op；update/handle_event 可直接驱动）
fn ts() -> State {
    State {
        prefs: crate::Prefs {
            ui_theme: iced::Theme::Light,
            theme_follow: true,
            lang: crate::lang::Lang::Zh,
            win_geom: None,
            win_save_at: None,
            last_view: None,
            muted: vec![],
            ignored: vec![],
            vip: vec![],
            templates: vec![],
            undo_secs: 0,
            layout: 0,
            hidden_toolbar: vec![],
            keys: Default::default(),
            flag_colors: Default::default(),
        },
        session: None,
        oauth_prompt: None,
        cmd_tx: None,
        read_txs: vec![],
        events: None,
        account_names: vec!["a@x".into(), "b@x".into()],
        error: None,
        setup_hint: None,
        nav: Nav {
            folders: vec![(0, folder("INBOX", FolderKind::Inbox))],
            cur: Some((0, "INBOX".into())),
            in_search: false,
            viewing_unified: false,
            search_query: String::new(),
            search_text: String::new(),
            threaded: false,
            load_more_pending: false,
            load_more_done: false,
            list_filter: ListFilter::All,
            sort_key: crate::SortKey::Date,
            list_asc: false,
            list_offset: 0.0,
            list_view_h: crate::list::DEFAULT_VIEW_H,
            folder_loading: false,
        },
        unified_contrib: Default::default(),
        search_snippets: Default::default(),
        search_contrib: Default::default(),
        rows: vec![],
        mail_display: vec![],
        drafts: vec![],
        selected: None,
        reading: None,
        body_req: None,
        pending_delete: None,
        expanded: Default::default(),
        screen: Screen::Mail,
        statuses: Default::default(),
        batch: BatchOps {
            selection: Default::default(),
            move_open: false,
            pending_delete: false,
            move_picker: None,
            redirect_prompt: None,
            redirect_to: String::new(),
            redirect_pending: None,
        },
        folder_edit: FolderEdit {
            new_folder: None,
            rename_folder: None,
            pending_delete: None,
        },
        contacts: vec![],
        outbox_rows: vec![],
        outbox_counts: Default::default(),
        conn_ok: Default::default(),
        pending_empty_trash: false,
        contacts_full: vec![],
        pgp_edit: None,
        signature_edit: None,
        sync_edit: None,
        rule_edit: None,
        compose: None,
        compose_window: None,
        main_window: None,
        sidebar_w: 200.0,
        list_w: 380.0,
        split_drag: None,
        drag: None,
        drag_targets: crate::drag::new_target_rects(),
        context_menu: None,
        contact_search: String::new(),
        sender_stats: vec![],
        template_edit: None,
        undo_send: None,
        export_folder: None,
        reminders: vec![],
        local_stores: Default::default(),
        export_target: None,
        accounts: vec![],
        cfg_path: std::path::PathBuf::from("/tmp/smail-test-config.toml"),
        data_root: std::path::PathBuf::from("/tmp"),
        ai: ai::AiUiState::new(Default::default(), std::path::Path::new("/tmp")),
        html: crate::state::HtmlView::new(),
        #[cfg(target_os = "macos")]
        webview: None,
    }
}

#[test]
fn search_submit_clears_batch_state() {
    let mut s = ts();
    s.batch.selection.insert((0, 1));
    s.batch.move_open = true;
    s.batch.pending_delete = true;
    s.batch.move_picker = Some((0, 1));
    s.pending_delete = Some((0, 1));
    s.nav.search_text = "周报".into();
    let _ = update(&mut s, Message::SearchSubmit);
    assert!(s.batch.selection.is_empty(), "搜索前批量选择必须清空");
    assert!(!s.batch.move_open && !s.batch.pending_delete);
    assert!(s.batch.move_picker.is_none() && s.pending_delete.is_none());
}

#[test]
fn folder_selected_clears_transient_refs() {
    let mut s = ts();
    s.batch.selection.insert((0, 1));
    s.pending_delete = Some((0, 1));
    s.body_req = Some((0, 1));
    s.selected = Some((0, 1));
    let _ = update(&mut s, Message::FolderSelected(0, "INBOX".into()));
    assert!(s.batch.selection.is_empty() && s.pending_delete.is_none() && s.body_req.is_none());
    assert!(s.selected.is_none());
}

/// 空态三态的旗标流转：选中文件夹置「同步中」，FolderLoaded 回来解除；
/// 未读徽标与空列表的矛盾期显示「正在同步」而不是「没有邮件」
#[test]
fn folder_loading_set_on_select_and_cleared_on_load() {
    let mut s = ts();
    let _ = update(&mut s, Message::FolderSelected(0, "INBOX".into()));
    assert!(s.nav.folder_loading, "选中后列表在途");
    let _ = update(
        &mut s,
        Message::Ev(smail_core::TaggedEvent {
            account: 0,
            event: Event::FolderLoaded {
                folder: "INBOX".into(),
                rows: vec![],
            },
        }),
    );
    assert!(!s.nav.folder_loading, "列表落地后不再处于同步中");
    // 伪文件夹（草稿/发件箱）不走 FolderLoaded，选中即解除
    let _ = update(&mut s, Message::FolderSelected(0, "__drafts__".into()));
    assert!(!s.nav.folder_loading, "草稿视图无服务器同步");
}

/// 切文件夹直读本地索引：服务线程堵在长同步里时（命令队列不被消费），
/// 有缓存行的文件夹必须立即上屏，folder_loading 不亮——「一同步就看不见
/// 邮件」的回归钉子
#[test]
fn folder_selected_reads_local_cache_immediately() {
    // 建一个带两行 INBOX 缓存的真实索引库
    let dir = std::env::temp_dir().join(format!("smail-ui-local-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("acct")).unwrap();
    {
        let store = smail_store::Store::open(&dir.join("acct").join("index.db")).unwrap();
        store.upsert(&row(1, false, false)).unwrap();
        store.upsert(&row(2, true, false)).unwrap();
    }
    let mut s = ts();
    s.accounts = vec![smail_core::AccountConfig {
        name: "a".into(),
        slug: "acct".into(),
        identity: "a@x".into(),
        imap: smail_core::ImapConfig {
            host: "h".into(),
            port: 993,
            username: "a@x".into(),
            password: None,
            starttls: false,
            tls: true,
            use_oauth2: false,
        },
        smtp: None,
        oauth: None,
        rules: vec![],
        pgp: None,
        signature: None,
        sync_config: Default::default(),
    }];
    s.account_names = vec!["a@x".into()];
    s.data_root = dir.clone();
    let _ = update(&mut s, Message::FolderSelected(0, "INBOX".into()));
    assert_eq!(s.rows.len(), 2, "本地缓存行必须立即上屏");
    assert!(!s.nav.folder_loading, "有缓存行不亮「同步中」");
    // 切到无缓存的文件夹：回到「正在同步」态
    let _ = update(&mut s, Message::FolderSelected(0, "Archive".into()));
    assert!(
        s.rows.is_empty() && s.nav.folder_loading,
        "无缓存显示同步中"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn late_body_loaded_dropped_without_body_req() {
    let mut s = ts();
    let _ = update(
        &mut s,
        Message::Ev(smail_core::TaggedEvent {
            account: 0,
            event: Event::BodyLoaded {
                hash: 9,
                attachment_images: vec![],
                attachment_previews: vec![],
                subject: "迟到的正文".into(),
                body: "x".into(),
                to_addrs: String::new(),
                cc_addrs: String::new(),
                unsubscribe: None,
                inline_images: vec![],
                remote_images: vec![],
                is_html: false,
                attachments: vec![],
                pgp: None,
                high_priority: false,
            },
        }),
    );
    assert!(s.reading.is_none(), "无在途请求的迟到正文必须丢弃");
}

#[test]
fn folder_loaded_prunes_dead_refs() {
    let mut s = ts();
    s.rows = vec![(0, row(1, false, false)), (0, row(2, true, false))];
    s.batch.selection.insert((0, 1));
    s.batch.selection.insert((0, 99)); // 对端已删
    s.pending_delete = Some((0, 99));
    s.selected = Some((0, 99));
    let _ = update(
        &mut s,
        Message::Ev(smail_core::TaggedEvent {
            account: 0,
            event: Event::FolderLoaded {
                folder: "INBOX".into(),
                rows: vec![row(1, false, false), row(2, true, false)],
            },
        }),
    );
    assert!(s.batch.selection.contains(&(0, 1)), "存活邮件保留");
    assert!(!s.batch.selection.contains(&(0, 99)), "死 hash 清理");
    assert!(s.pending_delete.is_none(), "确认条随死引用清理");
    assert!(s.selected.is_none());
}

#[test]
fn images_loaded_wrong_account_not_filled() {
    let mut s = ts();
    s.reading = Some((
        1,
        Reading {
            hash: 5,
            attach_images: vec![],
            attach_previews: vec![],
            subject: "s".into(),
            body: "b".into(),
            to_addrs: String::new(),
            cc_addrs: String::new(),
            unsubscribe: None,
            images: vec![],
            remote_pending: vec!["http://x/y.png".into()],
            remote_loading: true,
            md: None,
            attachments: vec![],
            pgp: None,
            high_priority: false,
        },
    ));
    let _ = update(
        &mut s,
        Message::Ev(smail_core::TaggedEvent {
            account: 0, // 事件来自账号0，阅读在账号1
            event: Event::ImagesLoaded {
                hash: 5,
                images: vec![("u".into(), vec![0x89, b'P', b'N', b'G'])],
            },
        }),
    );
    let (_, r) = s.reading.as_ref().unwrap();
    assert!(r.images.is_empty(), "跨账号 hash 相同也不能回填");
    assert!(r.remote_loading, "加载态不被无关事件复位");
}

#[test]
fn pagination_triggers_on_display_bottom() {
    let mut s = ts();
    // 数据层恰为页边界；会话视图关（平铺）→ 显示行数 == rows
    let n = smail_core::UI_LIST_LIMIT as usize;
    let rows: Vec<RowEntry> = (0..n as i64).map(|i| (0, row(i, true, false))).collect();
    s.rows = rows;
    s.rebuild_display();
    assert_eq!(s.mail_display.len(), n);
    let content_h = n as f32 * crate::list::ROW_H;
    let view_h = s.nav.list_view_h;
    let _ = update(&mut s, Message::ListScrolled(content_h - view_h, view_h));
    assert!(s.nav.load_more_pending, "触底且满页边界必须触发翻页");
}

#[test]
fn delete_mail_closes_only_that_reading() {
    let mut s = ts();
    s.rows = vec![(0, row(1, false, false)), (0, row(2, false, false))];
    s.rebuild_display();
    // 正在读 2，删的是 1
    s.selected = Some((0, 2));
    s.reading = Some((
        0,
        Reading {
            hash: 2,
            attach_images: vec![],
            attach_previews: vec![],
            subject: "s2".into(),
            body: "b".into(),
            to_addrs: String::new(),
            cc_addrs: String::new(),
            unsubscribe: None,
            images: vec![],
            remote_pending: vec![],
            remote_loading: false,
            md: None,
            attachments: vec![],
            pgp: None,
            high_priority: false,
        },
    ));
    let _ = update(&mut s, Message::DeleteMail(0, 1));
    assert!(s.reading.is_some(), "删除别的邮件不能关正在读的");
    assert!(!s.rows.iter().any(|(a, r)| *a == 0 && r.envelope_hash == 1));
}

#[test]
fn plan_ready_whitelist_and_membership() {
    use std::collections::HashMap;
    let mut s = ts();
    s.ai.cfg.enabled = true;
    s.ai.in_flight = 7;
    s.ai.next_id = 8;
    s.ai.busy = Some(ai::BusyKind::Organize);
    s.batch.selection.insert((0, 1));
    s.batch.selection.insert((0, 2));
    let mut ctx = HashMap::new();
    ctx.insert(0usize, (0usize, 1i64));
    ctx.insert(1usize, (0usize, 2i64));
    s.ai.plan_ctx = ctx;
    s.nav.folders = vec![
        (0, folder("INBOX", FolderKind::Inbox)),
        (0, folder("归档", FolderKind::Archive)),
        (0, folder("Junk", FolderKind::Junk)),
    ];
    let groups = vec![
        // 合法移动（归档存在）
        smail_ai::PlanGroup {
            title: "归档".into(),
            op: smail_ai::PlanOp::MoveTo("归档".into()),
            ids: vec![(0, 1)],
        },
        // Junk 目标：不在白名单（调用侧 folders 已剔除），整组该账号丢弃
        smail_ai::PlanGroup {
            title: "垃圾".into(),
            op: smail_ai::PlanOp::MoveTo("Junk".into()),
            ids: vec![(0, 2)],
        },
    ];
    ai::plan_ready(&mut s, 7, Ok(groups));
    let plan = s.ai.plan.as_ref().expect("应产出预览");
    assert_eq!(plan.len(), 1, "Junk 目标组必须被文件夹白名单拦截");
    assert!(matches!(&plan[0].group.op, smail_ai::PlanOp::MoveTo(f) if f == "归档"));
}

#[test]
fn folder_selected_remembers_last_view() {
    let mut s = ts();
    let _ = update(&mut s, Message::FolderSelected(0, "INBOX".into()));
    assert_eq!(
        s.prefs.last_view,
        Some(("a@x".into(), "INBOX".into())),
        "切文件夹要记忆（账号名, 文件夹），供启动恢复"
    );
    let _ = update(&mut s, Message::UnifiedInboxSelected);
    assert_eq!(
        s.prefs.last_view,
        Some(("__unified__".into(), "__all_inbox__".into()))
    );
}

#[test]
fn cur_is_trash_folder_detection() {
    let mut s = ts();
    s.nav.folders = vec![
        (0, folder("INBOX", FolderKind::Inbox)),
        (0, folder("Trash", FolderKind::Trash)),
    ];
    s.nav.cur = Some((0, "Trash".into()));
    assert!(s.cur_is_trash_folder(), "Trash 视图应判定为废纸篓");
    s.nav.cur = Some((0, "INBOX".into()));
    assert!(!s.cur_is_trash_folder());
    s.nav.cur = None;
    assert!(!s.cur_is_trash_folder(), "无当前视图不误判");
}

#[test]
fn esc_clears_empty_trash_confirm() {
    let mut s = ts();
    s.pending_empty_trash = true;
    let _ = update(
        &mut s,
        Message::KeyPressed(iced::keyboard::Key::Named(
            iced::keyboard::key::Named::Escape,
        )),
    );
    assert!(!s.pending_empty_trash, "Esc 应关清空废纸篓确认条");
}

#[test]
fn esc_in_search_from_outbox_keeps_view() {
    let mut s = ts();
    s.nav.cur = Some((0, "__outbox__".into()));
    s.nav.in_search = true;
    s.nav.search_text = "q".into();
    let _ = update(
        &mut s,
        Message::KeyPressed(iced::keyboard::Key::Named(
            iced::keyboard::key::Named::Escape,
        )),
    );
    assert!(!s.nav.in_search);
    assert_eq!(s.nav.cur, Some((0, "__outbox__".into())), "发件箱视图保持");
}

#[test]
fn esc_in_search_from_drafts_does_not_crash() {
    let mut s = ts();
    s.nav.cur = Some((0, "__drafts__".into()));
    s.nav.in_search = true;
    s.nav.search_text = "q".into();
    let _ = update(
        &mut s,
        Message::KeyPressed(iced::keyboard::Key::Named(
            iced::keyboard::key::Named::Escape,
        )),
    );
    assert!(!s.nav.in_search, "Esc 应退出搜索");
    assert_eq!(s.nav.cur, Some((0, "__drafts__".into())), "草稿视图保持");
}

/// 规则编辑草稿：下标→结构双向转换、校验拦截、高级规则安全构造
#[test]
fn rule_draft_roundtrip_and_validation() {
    use crate::setup::{rule_template, RuleDraft};
    // 模板：可编辑形态，value 空 → 校验必须拦下（空值规则会匹配一切）
    let mut d = rule_template();
    assert!(RuleDraft::editable(&d.rule));
    assert!(d.validate().is_err(), "空匹配值必须被拦下");
    d.set_value("parallels".into());
    d.set_field(1); // 主题
    d.set_predicate(2); // 开头是
    d.set_action(2); // 移动到文件夹
    d.set_move_dest("推广".into());
    assert!(d.validate().is_ok());
    // 下标改动真实落进 Rule 结构
    assert_eq!(d.rule.all_of[0].field, smail_core::RuleField::Subject);
    assert_eq!(
        d.rule.all_of[0].predicate,
        smail_core::RulePredicate::StartsWith
    );
    assert_eq!(
        d.rule.actions[0],
        smail_core::RuleAction::Move("推广".into())
    );
    // 往返：from_rule 还原下拉下标
    let d2 = RuleDraft::from_rule(d.rule.clone());
    assert_eq!((d2.field_idx, d2.predicate_idx, d2.action_idx), (1, 2, 2));

    // 高级规则（多条件多动作）：from_rule 安全构造不 panic，
    // editable=false，摘要把「且」与多动作显示出来
    let mut adv = d.rule.clone();
    adv.all_of.push(adv.all_of[0].clone());
    adv.actions.push(smail_core::RuleAction::Flag);
    assert!(!RuleDraft::editable(&adv));
    let summary = RuleDraft::summary(&adv);
    assert!(summary.contains("且"), "{summary}");
    assert!(summary.contains("标星标"), "{summary}");
    let d3 = RuleDraft::from_rule(adv);
    assert_eq!(d3.rule.all_of.len(), 2, "高级结构原样保留");
}

// ===========================================================================
// UI 流程真实验证（本轮补：此前只有编译+服务层 e2e，UI 路径没跑过）
// 每条测试驱动真实 update()，断言状态迁移与外发命令——不是编译即通过
// ===========================================================================

use std::sync::mpsc;

/// 挂真实命令通道的基座：state.cmd_tx 指向测试 receiver，外发命令可断言
fn ts_with_cmds() -> (State, mpsc::Receiver<smail_core::SessionCmd>) {
    let mut s = ts();
    let (tx, rx) = mpsc::channel();
    s.cmd_tx = Some(tx);
    (s, rx)
}

fn body_loaded(hash: i64) -> Event {
    Event::BodyLoaded {
        hash,
        attachment_images: vec![],
        attachment_previews: vec![],
        subject: "s".into(),
        body: "b".into(),
        to_addrs: String::new(),
        cc_addrs: String::new(),
        unsubscribe: None,
        inline_images: vec![],
        remote_images: vec![],
        is_html: false,
        attachments: vec![],
        pgp: None,
        high_priority: false,
    }
}

/// 代转续发：未读邮件右键代转（body_req 为空）时 BodyLoaded 到达必须续发
/// RedirectNow——此前被在途守卫提前拦截，续发永远走不到（真实缺陷）
#[test]
fn ui_redirect_pending_continues_without_body_req() {
    let mut s = ts();
    s.batch.redirect_pending = Some((0, 1, "x@y".into()));
    s.body_req = None; // 用户没打开过这封信
    let _ = update(
        &mut s,
        Message::Ev(smail_core::TaggedEvent {
            account: 0,
            event: body_loaded(1),
        }),
    );
    assert!(
        s.batch.redirect_pending.is_none(),
        "挂起被消费=续发路径走到；否则 body_req 守卫仍在上游拦截"
    );
}

/// 撤销发送全流程：点发送 → QueueHold 外发+挂起+关窗；撤销 → 删行+快照恢复
#[test]
fn ui_undo_send_roundtrip() {
    let (mut s, rx) = ts_with_cmds();
    s.prefs.undo_secs = 5;
    s.compose = Some(crate::compose::Compose {
        ai_instruction: String::new(),
        ai_busy: false,
        ai_overwrite_ask: false,
        ai_pending: None,
        ai_rewrite_mode: false,
        ai_prev: None,
        autosave_at: None,
        draft_id: None,
        account: 0,
        pgp_sign: false,
        pgp_encrypt: false,
        high_priority: false,
        confirm_discard: false,
        schedule_open: false,
        schedule_input: String::new(),
        to: "x@y".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: "撤销测试".into(),
        body: {
            let mut c = iced::widget::text_editor::Content::new();
            c.perform(iced::widget::text_editor::Action::Edit(
                iced::widget::text_editor::Edit::Paste(std::sync::Arc::new("正文内容".into())),
            ));
            c
        },
        attachments: vec![],
        preview: false,
        preview_md: None,
    });
    let _ = update(&mut s, Message::SendClicked);
    assert!(s.compose.is_none(), "发送后关写信窗");
    let u = s.undo_send.as_ref().expect("撤销挂起已建");
    let hold_id = u.id;
    let held = rx.try_recv().expect("QueueHold 已外发");
    let smail_core::SessionCmd::Route { cmd, .. } = held else {
        panic!("应为 Route 命令");
    };
    let smail_core::Cmd::QueueHold {
        id, to, subject, ..
    } = cmd
    else {
        panic!("应为 QueueHold 命令");
    };
    assert_eq!(id, hold_id);
    assert_eq!(to, "x@y");
    assert_eq!(subject, "撤销测试");

    // 撤销：删行命令 + 快照恢复写信窗
    let _ = update(&mut s, Message::UndoSendClicked);
    assert!(s.undo_send.is_none(), "挂起已清");
    let c = s.compose.as_ref().expect("快照已恢复");
    assert_eq!(c.to, "x@y");
    assert_eq!(c.body.text(), "正文内容");
    let smail_core::SessionCmd::Route { cmd, .. } = rx.try_recv().expect("删行命令已外发")
    else {
        panic!("应为 Route");
    };
    assert!(matches!(cmd, smail_core::Cmd::DiscardOutbox(id) if id == hold_id));
}

/// 定时发送：合法时刻入 hold（send_at=解析值）；格式错/过去时刻快速失败
#[test]
fn ui_schedule_confirm_flows() {
    let (mut s, rx) = ts_with_cmds();
    s.compose = Some(crate::compose::Compose {
        ai_instruction: String::new(),
        ai_busy: false,
        ai_overwrite_ask: false,
        ai_pending: None,
        ai_rewrite_mode: false,
        ai_prev: None,
        autosave_at: None,
        draft_id: None,
        account: 0,
        pgp_sign: false,
        pgp_encrypt: false,
        high_priority: false,
        confirm_discard: false,
        schedule_open: true,
        schedule_input: "2030-01-01 09:30".into(),
        to: "x@y".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: "定时".into(),
        body: iced::widget::text_editor::Content::new(),
        attachments: vec![],
        preview: false,
        preview_md: None,
    });
    let _ = update(&mut s, Message::ScheduleConfirm);
    assert!(s.compose.is_none(), "定时后关窗");
    let smail_core::SessionCmd::Route { cmd, .. } = rx.try_recv().expect("QueueHold 已外发")
    else {
        panic!("应为 Route");
    };
    let smail_core::Cmd::QueueHold { send_at, .. } = cmd else {
        panic!("应为 QueueHold");
    };
    assert_eq!(send_at, crate::civil_to_unix(2030, 1, 1, 9, 30));

    // 过去时刻：快速失败不关窗不外发
    let (mut s2, rx2) = ts_with_cmds();
    s2.compose = Some(crate::compose::Compose {
        ai_instruction: String::new(),
        ai_busy: false,
        ai_overwrite_ask: false,
        ai_pending: None,
        ai_rewrite_mode: false,
        ai_prev: None,
        autosave_at: None,
        draft_id: None,
        account: 0,
        pgp_sign: false,
        pgp_encrypt: false,
        high_priority: false,
        confirm_discard: false,
        schedule_open: true,
        schedule_input: "2020-01-01 09:30".into(),
        to: "x@y".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: "过去".into(),
        body: iced::widget::text_editor::Content::new(),
        attachments: vec![],
        preview: false,
        preview_md: None,
    });
    let _ = update(&mut s2, Message::ScheduleConfirm);
    assert!(s2.compose.is_some(), "失败时面板保留");
    assert!(s2.error.is_some(), "报具体原因");
    assert!(rx2.try_recv().is_err(), "不外发");
}

/// 自定义工具栏/快捷键/布局切换：状态与持久化载荷真实翻转
#[test]
fn ui_toolbar_keybind_layout_toggles() {
    let mut s = ts();
    let _ = update(&mut s, Message::ToolbarToggle("theme".into()));
    assert!(s.prefs.hidden_toolbar.contains(&"theme".into()));
    let _ = update(&mut s, Message::ToolbarToggle("theme".into()));
    assert!(!s.prefs.hidden_toolbar.contains(&"theme".into()));

    let _ = update(&mut s, Message::KeyBindInput("reply".into(), "w".into()));
    assert_eq!(s.prefs.keys.get("reply").map(String::as_str), Some("w"));
    let _ = update(&mut s, Message::KeyBindInput("reply".into(), String::new()));
    assert!(!s.prefs.keys.contains_key("reply"), "清空=恢复默认");

    assert_eq!(s.prefs.layout, 0);
    let _ = update(&mut s, Message::ToggleLayout);
    assert_eq!(s.prefs.layout, 1);
    let _ = update(&mut s, Message::ToggleLayout);
    assert_eq!(s.prefs.layout, 0);
}

/// 模板插入：贴到光标处 + Esc 确认丢弃置位
#[test]
fn ui_template_insert_pastes() {
    let mut s = ts();
    s.prefs.templates = vec![crate::lang::TemplateCfg {
        name: "t".into(),
        body: "HELLO".into(),
    }];
    s.compose = Some(crate::compose::Compose {
        ai_instruction: String::new(),
        ai_busy: false,
        ai_overwrite_ask: false,
        ai_pending: None,
        ai_rewrite_mode: false,
        ai_prev: None,
        autosave_at: None,
        draft_id: None,
        account: 0,
        pgp_sign: false,
        pgp_encrypt: false,
        high_priority: false,
        confirm_discard: false,
        schedule_open: false,
        schedule_input: String::new(),
        to: "x@y".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: String::new(),
        body: {
            let mut c = iced::widget::text_editor::Content::new();
            c.perform(iced::widget::text_editor::Action::Edit(
                iced::widget::text_editor::Edit::Paste(std::sync::Arc::new("abc".into())),
            ));
            c
        },
        attachments: vec![],
        preview: false,
        preview_md: None,
    });
    let _ = update(&mut s, Message::TemplateInsert(0));
    let c = s.compose.as_ref().unwrap();
    assert_eq!(c.body.text(), "abcHELLO", "贴到光标（粘贴后光标在尾）");
    assert!(c.confirm_discard, "有内容后 Esc 先确认丢弃");
}

/// VIP/忽略/旗标：右键动作真实改变过滤与渲染载荷
#[test]
fn ui_vip_ignore_flag_actions() {
    let mut s = ts();
    s.rows = vec![(0, row(1, false, false))];
    s.rows[0].1.subject = "Re: 项目进度".into();
    s.rebuild_display();
    assert_eq!(s.mail_display.len(), 1);

    let _ = update(&mut s, Message::VipAdd("a@x".into()));
    assert!(s.prefs.vip.contains(&"a@x".into()));
    let _ = update(&mut s, Message::VipRemove("a@x".into()));
    assert!(!s.prefs.vip.contains(&"a@x".into()));

    // 忽略：按规范主题键过滤列表；取消忽略恢复
    let _ = update(
        &mut s,
        Message::IgnoreThread(crate::subject_key("Re: 项目进度")),
    );
    assert!(s.mail_display.is_empty(), "忽略后列表隐藏");
    let _ = update(
        &mut s,
        Message::UnignoreThread(crate::subject_key("转发: Re: 项目进度")),
    );
    assert_eq!(s.mail_display.len(), 1, "取消忽略（嵌套前缀同键）恢复");

    // 旗标颜色循环：无→red→…→gray→移除
    let _ = update(&mut s, Message::CycleFlagColor(0, 1));
    assert_eq!(
        s.prefs.flag_colors.get("1").map(String::as_str),
        Some("red")
    );
    for _ in 0..6 {
        let _ = update(&mut s, Message::CycleFlagColor(0, 1));
    }
    assert!(
        !s.prefs.flag_colors.contains_key("1"),
        "循环一圈回到无=移除"
    );
}

/// 布局+优先级+PGP 开关：撰写窗开关真实翻转
#[test]
fn ui_compose_toggles() {
    let mut s = ts();
    s.compose = Some(crate::compose::Compose {
        ai_instruction: String::new(),
        ai_busy: false,
        ai_overwrite_ask: false,
        ai_pending: None,
        ai_rewrite_mode: false,
        ai_prev: None,
        autosave_at: None,
        draft_id: None,
        account: 0,
        pgp_sign: false,
        pgp_encrypt: false,
        high_priority: false,
        confirm_discard: false,
        schedule_open: false,
        schedule_input: String::new(),
        to: "x@y".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: String::new(),
        body: iced::widget::text_editor::Content::new(),
        attachments: vec![],
        preview: false,
        preview_md: None,
    });
    let _ = update(&mut s, Message::ToggleHighPriority);
    assert!(s.compose.as_ref().unwrap().high_priority);
    let _ = update(&mut s, Message::TogglePgpEncrypt);
    let c = s.compose.as_ref().unwrap();
    assert!(c.pgp_encrypt && c.pgp_sign, "加密隐含签名");
    // 分屏态编辑正文 → 触发实时解析（preview_md 回填路径）；空正文合理拒绝入分屏
    let _ = update(&mut s, Message::ComposePreviewToggle);
    assert!(!s.compose.as_ref().unwrap().preview, "空正文不入分屏");
    let c = s.compose.as_mut().unwrap();
    c.body.perform(iced::widget::text_editor::Action::Edit(
        iced::widget::text_editor::Edit::Paste(std::sync::Arc::new("有内容".into())),
    ));
    let _ = update(&mut s, Message::ComposePreviewToggle);
    assert!(s.compose.as_ref().unwrap().preview, "有正文可入分屏");
}
