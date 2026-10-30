//! 国际化（i18n）
//!
//! ## 职责
//!
//! - **双语支持**：中文默认，可切换英文
//! - **静态字符串表**：UI 文案双语映射
//! - **偏好持久化**：语言选择存 `ui.json`
//! - **模板填充**：支持位置参数 `{}` 的动态文案
//!
//! ## 设计约定
//!
//! - **位置参数**：表内文案一律用 `{}`（调用侧 `format!(l.x, v)`）
//! - **后端文案**：Event::Status 等暂不本地化（服务线程无 UI 态）
//! - **偏好文件**：`$SMAIL_HOME/ui.json`（lang + theme）
//!
//! ## 使用方式
//!
//! ```rust
//! let l = state.l();  // 获取当前语言字符串表
//! text(l.inbox)       // 静态文案
//! text(f(l.unread_count, &[&count]))  // 带参数文案
//! ```
//!
//! ## 语言切换
//!
//! - UI 层：顶栏按钮切换 + 广播到所有服务线程
//! - 服务线程：状态/错误文案跟随语言设置

/// 运行时模板填充：表内文案用位置参数 {}（format! 要求字面量，
/// 故提供此 helper，依序替换）
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn f(tmpl: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = tmpl.to_string();
    for a in args {
        out = out.replacen("{}", &a.to_string(), 1);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lang {
    Zh,
    En,
}

impl Lang {
    pub(crate) fn toggle(self) -> Self {
        match self {
            Lang::Zh => Lang::En,
            Lang::En => Lang::Zh,
        }
    }
    /// 顶栏切换按钮显示的是「将切换到的语言」
    pub(crate) fn toggle_label(self) -> &'static str {
        match self {
            Lang::Zh => "EN",
            Lang::En => "中",
        }
    }
}

/// 偏好文件（lang + theme 手动选择；theme_follow 不落盘——跟随系统每次启动重判）
#[derive(Debug)]
pub(crate) struct UiPrefs {
    pub(crate) lang: Lang,
    pub(crate) dark: Option<bool>,
    /// AI 隐私确认（首次使用云 AI 弹一次）
    pub(crate) ai_ack: bool,
    /// 窗口几何（w, h, x, y；手动调整后记忆，启动恢复）
    pub(crate) win: Option<[f32; 4]>,
    /// 上次视图（账号名, 文件夹/伪视图名；启动恢复，离线也能直达缓存邮件）
    pub(crate) last_view: Option<(String, String)>,
    /// 三栏列宽（拖拽分割条调整后记忆；启动恢复）
    pub(crate) columns: Option<(f32, f32)>,
    /// 静音的发件人地址（新邮件通知跳过）
    pub(crate) muted: Vec<String>,
}

pub(crate) fn load_prefs(data_root: &std::path::Path) -> UiPrefs {
    let mut prefs = UiPrefs {
        lang: Lang::Zh,
        dark: None,
        ai_ack: false,
        win: None,
        last_view: None,
        columns: None,
        muted: vec![],
    };
    let Ok(raw) = std::fs::read_to_string(data_root.join("ui.json")) else {
        return prefs;
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
        if v.get("lang").and_then(|x| x.as_str()) == Some("en") {
            prefs.lang = Lang::En;
        }
        match v.get("theme").and_then(|x| x.as_str()) {
            Some("dark") => prefs.dark = Some(true),
            Some("light") => prefs.dark = Some(false),
            _ => {}
        }
        if v.get("ai_ack").and_then(|x| x.as_bool()) == Some(true) {
            prefs.ai_ack = true;
        }
        if let Some(a) = v.get("win").and_then(|x| x.as_array()) {
            let nums: Vec<f32> = a
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect();
            if nums.len() == 4 {
                prefs.win = Some([nums[0], nums[1], nums[2], nums[3]]);
            }
        }
        if let Some(a) = v.get("columns").and_then(|x| x.as_array()) {
            let nums: Vec<f32> = a
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect();
            if nums.len() == 2 {
                prefs.columns = Some((nums[0], nums[1]));
            }
        }
        if let Some(a) = v.get("muted").and_then(|x| x.as_array()) {
            prefs.muted = a
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
        }
    }
    prefs
}

pub(crate) fn save_prefs(data_root: &std::path::Path, p: &UiPrefs) {
    let obj = serde_json::json!({
        "lang": if matches!(p.lang, Lang::En) { "en" } else { "zh" },
        "theme": if matches!(p.dark, Some(true)) { "dark" } else { "light" },
        "ai_ack": p.ai_ack,
        "win": p.win.map(|w| serde_json::json!(w)),
        "last_view": p.last_view.as_ref().map(|(a, f)| serde_json::json!([a, f])),
        "columns": p.columns.map(|(s, l)| serde_json::json!([s, l])),
        "muted": p.muted,
    });
    let _ = std::fs::write(data_root.join("ui.json"), obj.to_string());
}

/// 特殊文件夹的本地化显示名（自定义文件夹显示服务器原名末段）
pub(crate) fn folder_display(l: &Lex, kind: smail_core::FolderKind, path: &str) -> String {
    match kind {
        smail_core::FolderKind::Inbox => l.folder_inbox.into(),
        smail_core::FolderKind::Sent => l.folder_sent.into(),
        smail_core::FolderKind::Junk => l.folder_junk.into(),
        smail_core::FolderKind::Archive => l.folder_archive.into(),
        smail_core::FolderKind::Trash => l.folder_trash.into(),
        smail_core::FolderKind::Drafts => l.folder_drafts.into(),
        smail_core::FolderKind::Plain => path.rsplit('/').next().unwrap_or(path).to_string(),
    }
}

// ---------------------------------------------------------------------------
// 字符串表
// ---------------------------------------------------------------------------

pub(crate) struct Lex {
    // ---- 顶栏 ----
    pub(crate) compose_new: &'static str,
    pub(crate) view_flat: &'static str,
    pub(crate) view_threads: &'static str,
    pub(crate) search_ph: &'static str,
    pub(crate) add_account: &'static str,
    pub(crate) oauth_title: &'static str,
    pub(crate) oauth_body: &'static str, // {url}
    pub(crate) oauth_open: &'static str,
    pub(crate) err_close: &'static str,

    // ---- 侧栏 ----
    pub(crate) unified: &'static str,
    pub(crate) unified_unread: &'static str, // {n}
    pub(crate) no_account: &'static str,
    pub(crate) add_account_big: &'static str,
    pub(crate) drafts: &'static str,
    pub(crate) outbox_label: &'static str,
    pub(crate) conn_lost: &'static str,
    pub(crate) conn_retrying: &'static str,
    pub(crate) outbox_retry: &'static str,
    pub(crate) outbox_hint: &'static str,
    pub(crate) empty_trash: &'static str,
    pub(crate) empty_trash_q: &'static str, // {n}
    pub(crate) empty_junk: &'static str,
    pub(crate) empty_junk_q: &'static str, // {n}
    pub(crate) mark_all_read: &'static str,
    pub(crate) expand_all: &'static str,
    pub(crate) collapse_all: &'static str,
    pub(crate) drafts_n: &'static str,      // {n}
    pub(crate) folder_inbox: &'static str,
    pub(crate) folder_sent: &'static str,
    pub(crate) folder_junk: &'static str,
    pub(crate) folder_archive: &'static str,
    pub(crate) folder_trash: &'static str,
    pub(crate) folder_drafts: &'static str,
    pub(crate) new_folder: &'static str,
    pub(crate) new_folder_ph: &'static str,
    pub(crate) do_create: &'static str,
    pub(crate) do_rename: &'static str,
    pub(crate) cancel: &'static str,

    // ---- 列表头/过滤/空态/分页 ----
    pub(crate) search_hdr: &'static str,  // {q} {n}
    pub(crate) drafts_hdr: &'static str,  // {acct} {n}
    pub(crate) unified_hdr: &'static str, // {n}
    pub(crate) filter_all: &'static str,
    pub(crate) filter_unread: &'static str,
    pub(crate) filter_starred: &'static str,
    pub(crate) sort_asc: &'static str,
    pub(crate) sort_desc: &'static str,
    pub(crate) empty_add_account: &'static str,
    pub(crate) empty_no_mail: &'static str,
    pub(crate) empty_no_match: &'static str, // {f}
    pub(crate) empty_syncing: &'static str,
    pub(crate) empty_stale: &'static str, // {n}
    pub(crate) resync: &'static str,
    pub(crate) loaded_pages: &'static str,   // {n}
    pub(crate) at_bottom: &'static str,      // {n}
    pub(crate) unified_cap: &'static str,    // {n}

    // ---- 批量栏 ----
    pub(crate) selected_n: &'static str, // {n}
    pub(crate) select_all: &'static str,
    pub(crate) mark_read: &'static str,
    pub(crate) mark_unread: &'static str,
    pub(crate) star: &'static str,
    pub(crate) batch_move: &'static str,
    pub(crate) batch_archive: &'static str,
    pub(crate) batch_spam: &'static str,
    pub(crate) batch_delete: &'static str,
    pub(crate) clear: &'static str,
    pub(crate) no_dest: &'static str,
    pub(crate) perm_delete: &'static str,
    pub(crate) folder_del_q: &'static str, // {f}

    // ---- 草稿行 ----
    pub(crate) no_subject: &'static str,

    // ---- 阅读区 ----
    pub(crate) pick_mail: &'static str,
    pub(crate) pick_mail_hint: &'static str,
    pub(crate) draft_hint: &'static str,
    pub(crate) mark_unread_t: &'static str,
    pub(crate) mark_read_t: &'static str,
    pub(crate) unstar: &'static str,
    pub(crate) star_t: &'static str,
    pub(crate) move_to: &'static str,
    pub(crate) cancel_move: &'static str,
    pub(crate) archive: &'static str,
    pub(crate) spam: &'static str,
    pub(crate) reply: &'static str,
    pub(crate) reply_all: &'static str,
    pub(crate) forward: &'static str,
    pub(crate) delete: &'static str,
    pub(crate) del_q_perm: &'static str,
    pub(crate) del_q_move: &'static str,
    pub(crate) batch_del_q_perm: &'static str, // {n}
    pub(crate) batch_del_q_move: &'static str, // {n}
    pub(crate) unsub: &'static str,
    pub(crate) unsub_note: &'static str,
    pub(crate) save_eml: &'static str,
    pub(crate) attach_n: &'static str, // {n}
    pub(crate) save: &'static str,
    pub(crate) not_junk: &'static str,
    pub(crate) img_loading: &'static str,
    pub(crate) show_remote: &'static str, // {n}

    // ---- 写信 ----
    pub(crate) title_new: &'static str,
    pub(crate) title_edit: &'static str,
    pub(crate) account_sw: &'static str, // {name}
    pub(crate) sign_toggle: &'static str,
    pub(crate) enc_toggle: &'static str,
    pub(crate) send: &'static str,
    pub(crate) save_draft: &'static str,
    pub(crate) close: &'static str,
    pub(crate) add_attach: &'static str,
    pub(crate) discard_q: &'static str,
    pub(crate) discard: &'static str,
    pub(crate) keep_editing: &'static str,
    pub(crate) to_label: &'static str,
    pub(crate) cc_label: &'static str,
    pub(crate) bcc_label: &'static str,
    pub(crate) subject_label: &'static str,
    pub(crate) to_ph: &'static str,
    pub(crate) cc_ph: &'static str,
    pub(crate) bcc_ph: &'static str,
    pub(crate) subject_ph: &'static str,
    pub(crate) body_ph: &'static str,
    pub(crate) compose_hints: &'static str,

    // ---- 设置屏 ----
    pub(crate) welcome: &'static str,
    pub(crate) add_hdr: &'static str,
    pub(crate) preset_custom: &'static str,
    pub(crate) email_label: &'static str,
    pub(crate) name_label: &'static str,
    pub(crate) name_ph: &'static str,
    pub(crate) oauth_note: &'static str,
    pub(crate) secret_ph: &'static str,
    pub(crate) pwd_label: &'static str,
    pub(crate) pwd_ph: &'static str,
    pub(crate) imap_label: &'static str,
    pub(crate) imap_port_label: &'static str,
    pub(crate) smtp_label: &'static str,
    pub(crate) smtp_port_label: &'static str,
    pub(crate) test_ok: &'static str, // {n}
    pub(crate) verifying: &'static str,
    pub(crate) force_save: &'static str,
    pub(crate) save_use: &'static str,
    pub(crate) testing: &'static str,
    pub(crate) test_conn: &'static str,
    pub(crate) look_around: &'static str,
    pub(crate) back: &'static str,
    pub(crate) esc_again: &'static str,
    pub(crate) have_accounts: &'static str,
    pub(crate) delete_short: &'static str,
    pub(crate) keychain_on: &'static str,
    pub(crate) keychain_off: &'static str,
    pub(crate) sync_interval_btn: &'static str,
    pub(crate) sync_interval_label: &'static str,
    pub(crate) sync_interval_ph: &'static str,
    pub(crate) sync_interval_hint: &'static str,
    pub(crate) err_sync_interval: &'static str,
    pub(crate) export_btn: &'static str,
    pub(crate) rules_btn: &'static str,
    pub(crate) rules_hdr: &'static str,
    pub(crate) rules_add: &'static str,
    pub(crate) rules_none: &'static str,
    pub(crate) rules_advanced: &'static str,
    pub(crate) rule_move_ph: &'static str,
    pub(crate) rules_hint: &'static str,
    pub(crate) contacts_hdr: &'static str,
    pub(crate) no_contacts: &'static str,
    pub(crate) acct_n: &'static str,        // {n}
    pub(crate) contacts_n: &'static str,    // {name} {n}
    pub(crate) contacts_more: &'static str, // {n}
    pub(crate) cfg_note: &'static str,
    pub(crate) pgp_note: &'static str,
    pub(crate) key_label: &'static str,
    pub(crate) key_ph: &'static str,
    pub(crate) passphrase_label: &'static str,
    pub(crate) passphrase_ph: &'static str,
    pub(crate) pubring_label: &'static str,
    pub(crate) pubring_ph: &'static str,
    pub(crate) wkd_label: &'static str,
    pub(crate) pick_file: &'static str,

    // ---- AI 助手 ----
    pub(crate) ai_section: &'static str,
    pub(crate) signature_btn: &'static str,
    pub(crate) signature_label: &'static str,
    pub(crate) ai_status_on: &'static str, // {model}
    pub(crate) ai_status_off: &'static str,
    pub(crate) ai_setup: &'static str,
    pub(crate) ai_enabled_label: &'static str,
    pub(crate) ai_base_url_label: &'static str,
    pub(crate) ai_model_label: &'static str,
    pub(crate) ai_api_key_label: &'static str,
    pub(crate) ai_keychain_btn: &'static str,
    pub(crate) ai_test_ok: &'static str, // {reply}
    pub(crate) ai_summary_btn: &'static str,
    pub(crate) ai_working: &'static str,
    pub(crate) ai_instr_ph: &'static str,
    pub(crate) ai_draft_btn: &'static str,
    pub(crate) ai_overwrite_q: &'static str,
    pub(crate) ai_overwrite_yes: &'static str,
    pub(crate) ai_smart_reply: &'static str,
    pub(crate) ai_reply_instruction: &'static str,
    pub(crate) ai_organize_btn: &'static str,
    pub(crate) ai_plan_loading: &'static str,
    pub(crate) ai_plan_apply: &'static str,
    pub(crate) ai_plan_move_to: &'static str, // {folder}
    pub(crate) ai_plan_group_n: &'static str, // {n}
    pub(crate) ai_err_disabled: &'static str,
    pub(crate) ai_err_too_many: &'static str, // {n}
    pub(crate) ai_err_no_instr: &'static str,
    pub(crate) ai_err_empty_body: &'static str,
    pub(crate) ai_summary_thread: &'static str,
    pub(crate) ai_summary_cached: &'static str,
    pub(crate) ai_summary_hide: &'static str,
    pub(crate) ai_polish: &'static str,
    pub(crate) ai_formal: &'static str,
    pub(crate) ai_casual: &'static str,
    pub(crate) ai_shorter: &'static str,
    pub(crate) ai_longer: &'static str,
    pub(crate) ai_to_zh: &'static str,
    pub(crate) ai_to_en: &'static str,
    pub(crate) ai_instr_polish: &'static str,
    pub(crate) ai_instr_formal: &'static str,
    pub(crate) ai_instr_casual: &'static str,
    pub(crate) ai_instr_shorter: &'static str,
    pub(crate) ai_instr_longer: &'static str,
    pub(crate) ai_instr_to_zh: &'static str,
    pub(crate) ai_instr_to_en: &'static str,
    pub(crate) ai_rewrite_btn: &'static str,
    pub(crate) ai_undo: &'static str,
    pub(crate) ai_applied_n: &'static str, // {n}
    pub(crate) ai_privacy_title: &'static str,
    pub(crate) ai_privacy_text: &'static str, // {base_url}
    pub(crate) ai_privacy_go: &'static str,

    // ---- 错误/状态（UI 侧生成）----
    pub(crate) err_need_account: &'static str,
    pub(crate) err_need_account_search: &'static str,
    pub(crate) err_empty_to: &'static str,
    pub(crate) err_folder_name: &'static str,
    pub(crate) err_folder_rename: &'static str,
    pub(crate) err_no_plain_pwd: &'static str,
    pub(crate) err_orig_missing: &'static str,
    pub(crate) err_save_cfg: &'static str, // {e}
    pub(crate) err_reload: &'static str,   // {e}
    pub(crate) err_port: &'static str,
    pub(crate) oauth_no_test: &'static str,
    pub(crate) err_dup: &'static str,  // {email}
    pub(crate) err_conn: &'static str, // {e}
}

pub(crate) const ZH: Lex = Lex {
    compose_new: "✎ 写邮件",
    view_flat: "平铺视图",
    view_threads: "会话视图",
    search_ph: "搜索全文（回车）",
    add_account: "＋ 账号",
    oauth_title: "需要授权",
    oauth_body: "浏览器打开 {} ，输入验证码",
    oauth_open: "用浏览器打开",
    err_close: "✕",

    unified: "所有收件箱",
    unified_unread: "所有收件箱（{} 未读）",
    no_account: "还没有账号",
    add_account_big: "＋ 添加账号",
    drafts: "草稿",
    outbox_label: "发件箱",
    conn_lost: "⟳ 未连接：",
    conn_retrying: "自动重试中（新邮件/发信将在恢复后处理）",
    outbox_retry: "重发",
    outbox_hint: "发件箱项目不支持阅读——重发 / 删除在列表行内",
    empty_trash: "清空废纸篓",
    empty_trash_q: "永久删除废纸篓中全部 {} 封邮件？不可恢复。",
    empty_junk: "清空垃圾邮件",
    empty_junk_q: "永久删除垃圾文件夹中全部 {} 封邮件？不可恢复。",
    mark_all_read: "全部已读",
    expand_all: "展开全部",
    collapse_all: "折叠全部",
    drafts_n: "草稿（{}）",
    folder_inbox: "收件箱",
    folder_sent: "已发送",
    folder_junk: "垃圾",
    folder_archive: "归档",
    folder_trash: "废纸篓",
    folder_drafts: "草稿",
    new_folder: "  ＋ 文件夹",
    new_folder_ph: "新文件夹名",
    do_create: "建",
    do_rename: "改",
    cancel: "取消",

    search_hdr: "搜索 “{}”（{}）",
    drafts_hdr: "{} 草稿（{}）",
    unified_hdr: "所有收件箱（{} 封）",
    filter_all: "全部",
    filter_unread: "未读",
    filter_starred: "★",
    sort_asc: "↑ 旧→新",
    sort_desc: "↓ 新→旧",
    empty_add_account: "先添加一个账号，收件箱在这里出现",
    empty_no_mail: "没有邮件",
    empty_no_match: "没有{}邮件（点上方切回“全部”）",
    empty_syncing: "正在同步当前文件夹…",
    empty_stale: "服务器报告有 {} 封未读，但列表是空的——试试重新同步",
    resync: "重新同步",
    loaded_pages: "已加载 {} 封，滚动到底自动加载更多",
    at_bottom: "已到底部（共 {} 封）",
    unified_cap: "已聚合各账号最近 {} 封",

    selected_n: "已选 {} 封",
    select_all: "全选",
    mark_read: "标已读",
    mark_unread: "标未读",
    star: "★ 星标",
    batch_move: "移动",
    batch_archive: "归档",
    batch_spam: "标为垃圾",
    batch_delete: "删除",
    clear: "✕ 清除",
    no_dest: "没有其他文件夹（可在侧栏新建）",
    perm_delete: "永久删除",
    folder_del_q: "删除文件夹 {}？服务器上及其内全部邮件将被永久删除，不可恢复。",

    no_subject: "(无主题)",

    pick_mail: "选择一封邮件",
    pick_mail_hint: "↑↓ 选信 · r 回复 · a 全部回复 · f 转发 · e 归档 · s 星标 · Del 删除 · F5 刷新",
    draft_hint: "点击草稿进行编辑",
    mark_unread_t: "标为未读",
    mark_read_t: "标为已读",
    unstar: "★ 取消星标",
    star_t: "☆ 星标",
    move_to: "移动到",
    cancel_move: "取消移动",
    archive: "归档",
    spam: "标为垃圾",
    reply: "↩ 回复",
    reply_all: "↩↩ 回复全部",
    forward: "➦ 转发",
    delete: "删除",
    del_q_perm: "确认永久删除？废纸篓里的邮件删除后不可恢复",
    del_q_move: "移到废纸篓？（可从废纸篓恢复）",
    batch_del_q_perm: "在服务器上永久删除这 {} 封邮件？不可恢复。",
    batch_del_q_move: "将这 {} 封邮件移到废纸篓？（可恢复）",
    unsub: "退订",
    unsub_note: "（来自邮件的 List-Unsubscribe 头）",
    save_eml: "存为 .eml",
    attach_n: "附件（{}）",
    save: "保存",
    not_junk: "移回收件箱（非垃圾）",
    img_loading: "⏳ 图片加载中…",
    show_remote: "显示 {} 张远程图片（会向外部服务器暴露你的阅读行为）",

    title_new: "写邮件",
    title_edit: "编辑草稿",
    account_sw: "账号：{} ⇄",
    sign_toggle: "签名",
    enc_toggle: "加密",
    send: "发送",
    save_draft: "存草稿",
    close: "关闭",
    add_attach: "＋ 附件",
    discard_q: "丢弃这封未发送的邮件？",
    discard: "丢弃",
    keep_editing: "继续编辑",
    to_label: "收件人",
    cc_label: "抄送",
    bcc_label: "密送",
    subject_label: "主题",
    to_ph: "逗号分隔（输入时下方提示最近联系人）",
    cc_ph: "可留空",
    bcc_ph: "不写入邮件头，仅信封收件人",
    subject_ph: "邮件主题",
    body_ph: "在此输入正文…",
    compose_hints: "⌘↩ 发送 · ⌘S 存草稿 · Esc 关闭（有内容时先确认）",

    welcome: "欢迎使用 s-mail —— 先加一个账号",
    add_hdr: "添加账号",
    preset_custom: "自定义",
    email_label: "邮箱地址",
    name_label: "显示名",
    name_ph: "可留空（默认邮箱前缀）",
    oauth_note: "OAuth（XOAUTH2）—— 保存后自动弹授权链接",
    secret_ph: "Outlook 公共客户端可留空",
    pwd_label: "密码/授权码",
    pwd_ph: "QQ/163 用授权码（网页版设置里生成）",
    imap_label: "IMAP 服务器",
    imap_port_label: "IMAP 端口",
    smtp_label: "SMTP 服务器",
    smtp_port_label: "SMTP 端口",
    test_ok: "✓ 连接成功（{} 个文件夹）",
    verifying: "验证中…",
    force_save: "仍要保存（连接失败）",
    save_use: "保存并使用",
    testing: "测试中…",
    test_conn: "测试连接",
    look_around: "先逛逛",
    back: "返回",
    esc_again: "再按一次 Esc 退出（表单内容会丢弃）",
    have_accounts: "已有账号",
    delete_short: "删除",
    keychain_on: "钥匙串✓",
    keychain_off: "入钥匙串",
    sync_interval_btn: "间隔",
    sync_interval_label: "自动同步间隔（秒）",
    sync_interval_ph: "如 60",
    sync_interval_hint: "切文件夹时超过该间隔才重拉服务器；新邮件推送（IDLE）不受影响，收信始终实时",
    err_sync_interval: "同步间隔无效（需正整数秒）",
    export_btn: "导出全部",
    rules_btn: "规则",
    rules_hdr: "过滤规则（新邮件自动执行）",
    rules_add: "＋ 添加规则",
    rules_none: "还没有规则——点「＋ 添加规则」建一条",
    rules_advanced: "高级规则（多条件/多动作，仅可删除；编辑请手改 config.toml）",
    rule_move_ph: "目标文件夹",
    rules_hint: "规则只作用于新收到的邮件，保存后立即生效",
    contacts_hdr: "通讯录（收信自动收集）",
    no_contacts: "暂无联系人（收到新邮件后自动收集）",
    acct_n: "账号 {}",
    contacts_n: "{}（{} 人）",
    contacts_more: "… 共 {} 人（仅示前 50）",
    cfg_note: "配置保存在 ~/.s-mail/config.toml（界面改动即时生效；手改文件需重启）",
    pgp_note: "PGP：收信解密/验签 + 发信签名/加密（三字段全空=移除）",
    key_label: "私钥路径",
    key_ph: "~/.s-mail/<账号>/pgp/secret.asc",
    passphrase_label: "私钥口令",
    passphrase_ph: "可留空（无口令证书）",
    pubring_label: "公钥环",
    pubring_ph: "~/.s-mail/<账号>/pgp/pub.asc（可留空=不验签）",
    wkd_label: "WKD 自动发现公钥（收件人缺公钥时向其域名查询；隐私默认关）",
    pick_file: "选择文件…",

    ai_section: "AI 助手",
    signature_btn: "签名",
    signature_label: "签名（写信时追加到正文末；留空=移除）",
    ai_status_on: "已开启 · {}",
    ai_status_off: "未开启",
    ai_setup: "设置",
    ai_enabled_label: "启用 AI（写邮件 / 摘要 / 整理建议）",
    ai_base_url_label: "API 地址",
    ai_model_label: "模型",
    ai_api_key_label: "API Key",
    ai_keychain_btn: "入钥匙串",
    ai_test_ok: "✓ 连接正常（{}）",
    ai_summary_btn: "✨ 摘要",
    ai_working: "⏳ AI 处理中…",
    ai_instr_ph: "AI 指令（例：婉拒周四的会议，提议改下周二）",
    ai_draft_btn: "✨ 起草",
    ai_overwrite_q: "AI 将覆盖主题与正文，继续？",
    ai_overwrite_yes: "覆盖",
    ai_smart_reply: "✨ 智能回复",
    ai_reply_instruction: "回复这封邮件，语气专业友好，把必要的问题问清楚",
    ai_organize_btn: "✨ 整理选中项",
    ai_plan_loading: "⏳ AI 正在分析选中邮件…",
    ai_plan_apply: "应用勾选项",
    ai_plan_move_to: "移到 {}",
    ai_plan_group_n: "（{} 封）",
    ai_err_disabled: "AI 未启用（右上角 ＋ 账号 → 设置里开启）",
    ai_err_too_many: "整理建议一次最多 {} 封（先缩小选择范围）",
    ai_err_no_instr: "先在 AI 指令框里写要起草什么",
    ai_err_empty_body: "正文还是空的（先写点什么，或用 ✨ 起草）",
    ai_summary_thread: "✨ 会话摘要",
    ai_summary_cached: "✨ 摘要 ✓",
    ai_summary_hide: "✨ 收起摘要",
    ai_polish: "润色",
    ai_formal: "更正式",
    ai_casual: "更轻松",
    ai_shorter: "缩短",
    ai_longer: "加长",
    ai_to_zh: "译中",
    ai_to_en: "译英",
    ai_instr_polish: "润色这封邮件：保持原意与事实，语言更通顺自然，修正错别字",
    ai_instr_formal: "把语气改得更正式、专业",
    ai_instr_casual: "把语气改得更轻松、口语化（仍保持礼貌）",
    ai_instr_shorter: "把正文显著缩短，只保留必要信息",
    ai_instr_longer: "把正文适当展开补充，更完整周到（不编造新事实）",
    ai_instr_to_zh: "把这封邮件翻译成中文（内容与格式保持不变）",
    ai_instr_to_en: "把这封邮件翻译成英文（内容与格式保持不变）",
    ai_rewrite_btn: "↻ 重写",
    ai_undo: "↩ 撤销 AI",
    ai_applied_n: "AI 整理已应用 {} 组",
    ai_privacy_title: "AI 隐私提示",
    ai_privacy_text: "邮件内容将发送给你配置的 AI 提供商（{}）。解密后的 PGP 邮件同样会外发。本地 Ollama 不出本机。",
    ai_privacy_go: "知道了，继续",

    err_need_account: "先添加账号再写邮件（右上角 ＋ 账号）",
    err_need_account_search: "先添加账号再搜索",
    err_empty_to: "收件人不能为空",
    err_folder_name: "文件夹名字不能为空",
    err_folder_rename: "文件夹新名字不能为空",
    err_no_plain_pwd: "该账号没有可迁移的明文密码（OAuth 账号或已入钥匙串）",
    err_orig_missing: "找不到原邮件",
    err_save_cfg: "保存配置失败：{}",
    err_reload: "重载会话失败：{}",
    err_port: "IMAP 端口无效",
    oauth_no_test: "OAuth 账号保存后自动发起授权（无需测试）",
    err_dup: "邮箱 {} 已配置过",
    err_conn: "连接失败：{}。可返回修改，或点「仍要保存」",
};

pub(crate) const EN: Lex = Lex {
    compose_new: "✎ Compose",
    view_flat: "Flat view",
    view_threads: "Threads view",
    search_ph: "Search (Enter)",
    add_account: "＋ Account",
    oauth_title: "Authorization needed",
    oauth_body: "Open {} in the browser, then enter the code",
    oauth_open: "Open in browser",
    err_close: "✕",

    unified: "All Inboxes",
    unified_unread: "All Inboxes ({} unread)",
    no_account: "No accounts yet",
    add_account_big: "＋ Add account",
    drafts: "Drafts",
    outbox_label: "Outbox",
    conn_lost: "⟳ Disconnected: ",
    conn_retrying: "retrying automatically (mail/send will resume)",
    outbox_retry: "Retry",
    outbox_hint: "Outbox items are not readable — retry / delete on each row",
    empty_trash: "Empty Trash",
    empty_trash_q: "Permanently delete all {} messages in Trash? Irreversible.",
    empty_junk: "Empty Junk",
    empty_junk_q: "Permanently delete all {} messages in Junk? Irreversible.",
    mark_all_read: "Mark all read",
    expand_all: "Expand all",
    collapse_all: "Collapse all",
    drafts_n: "Drafts ({})",
    folder_inbox: "Inbox",
    folder_sent: "Sent",
    folder_junk: "Junk",
    folder_archive: "Archive",
    folder_trash: "Trash",
    folder_drafts: "Drafts",
    new_folder: "  ＋ Folder",
    new_folder_ph: "New folder name",
    do_create: "OK",
    do_rename: "Rename",
    cancel: "Cancel",

    search_hdr: "Search “{}” ({})",
    drafts_hdr: "{} Drafts ({})",
    unified_hdr: "All Inboxes ({})",
    filter_all: "All",
    filter_unread: "Unread",
    filter_starred: "★",
    sort_asc: "↑ Oldest",
    sort_desc: "↓ Newest",
    empty_add_account: "Add an account first — your inbox lands here",
    empty_no_mail: "No messages",
    empty_no_match: "No {} messages (switch back to All above)",
    empty_syncing: "Syncing this folder…",
    empty_stale: "Server reports {} unread but the list is empty — try resyncing",
    resync: "Resync",
    loaded_pages: "{} loaded — scroll to bottom for more",
    at_bottom: "End of list ({})",
    unified_cap: "Showing latest {} across accounts",

    selected_n: "{} selected",
    select_all: "Select all",
    mark_read: "Mark read",
    mark_unread: "Mark unread",
    star: "★ Star",
    batch_move: "Move",
    batch_archive: "Archive",
    batch_spam: "Junk",
    batch_delete: "Delete",
    clear: "✕ Clear",
    no_dest: "No other folders (create one in the sidebar)",
    perm_delete: "Delete forever",
    folder_del_q: "Delete folder {}? All messages in it will be permanently deleted. Irreversible.",

    no_subject: "(no subject)",

    pick_mail: "Select a message",
    pick_mail_hint: "↑↓ Navigate · r Reply · a Reply all · f Forward · e Archive · s Star · Del Delete · F5 Refresh",
    draft_hint: "Click a draft to edit",
    mark_unread_t: "Mark unread",
    mark_read_t: "Mark read",
    unstar: "★ Unstar",
    star_t: "☆ Star",
    move_to: "Move to",
    cancel_move: "Cancel move",
    archive: "Archive",
    spam: "Junk",
    reply: "↩ Reply",
    reply_all: "↩↩ Reply all",
    forward: "➦ Forward",
    delete: "Delete",
    del_q_perm: "Delete permanently? Messages in Trash cannot be recovered",
    del_q_move: "Move to Trash? (recoverable from Trash)",
    batch_del_q_perm: "Permanently delete these {} messages on the server? Irreversible.",
    batch_del_q_move: "Move these {} messages to Trash? (recoverable)",
    unsub: "Unsubscribe",
    unsub_note: "(from List-Unsubscribe header)",
    save_eml: "Save .eml",
    attach_n: "Attachments ({})",
    save: "Save",
    not_junk: "Move to Inbox (not junk)",
    img_loading: "⏳ Loading images…",
    show_remote: "Show {} remote images (will notify external servers)",

    title_new: "New message",
    title_edit: "Edit draft",
    account_sw: "Account: {} ⇄",
    sign_toggle: "Sign",
    enc_toggle: "Encrypt",
    send: "Send",
    save_draft: "Save draft",
    close: "Close",
    add_attach: "＋ Attach",
    discard_q: "Discard this unsent message?",
    discard: "Discard",
    keep_editing: "Keep editing",
    to_label: "To",
    cc_label: "Cc",
    bcc_label: "Bcc",
    subject_label: "Subject",
    to_ph: "Comma separated (recent contacts suggested)",
    cc_ph: "Optional",
    bcc_ph: "Envelope recipients only, not in headers",
    subject_ph: "Subject",
    body_ph: "Write your message…",
    compose_hints: "⌘↩ Send · ⌘S Save draft · Esc Close (confirm first when not empty)",

    welcome: "Welcome to s-mail — add an account",
    add_hdr: "Add account",
    preset_custom: "Custom",
    email_label: "Email",
    name_label: "Display name",
    name_ph: "Optional (defaults to email prefix)",
    oauth_note: "OAuth (XOAUTH2) — authorization link opens after save",
    secret_ph: "Optional for Outlook public clients",
    pwd_label: "Password / app password",
    pwd_ph: "QQ/163 use auth codes (generate in web settings)",
    imap_label: "IMAP server",
    imap_port_label: "IMAP port",
    smtp_label: "SMTP server",
    smtp_port_label: "SMTP port",
    test_ok: "✓ Connected ({} folders)",
    verifying: "Verifying…",
    force_save: "Save anyway (connection failed)",
    save_use: "Save & use",
    testing: "Testing…",
    test_conn: "Test connection",
    look_around: "Look around",
    back: "Back",
    esc_again: "Press Esc again to close (form will be discarded)",
    have_accounts: "Accounts",
    delete_short: "Delete",
    keychain_on: "Keychain ✓",
    keychain_off: "To Keychain",
    sync_interval_btn: "Interval",
    sync_interval_label: "Resync interval (seconds)",
    sync_interval_ph: "e.g. 60",
    sync_interval_hint: "Folders older than this are re-fetched on open; IDLE push is unaffected — new mail still arrives instantly",
    err_sync_interval: "Invalid interval (positive integer seconds)",
    export_btn: "Export all",
    rules_btn: "Rules",
    rules_hdr: "Filter rules (run on new mail)",
    rules_add: "＋ Add rule",
    rules_none: "No rules yet — click \"＋ Add rule\"",
    rules_advanced: "Advanced rule (multi-condition/action, delete only; edit in config.toml)",
    rule_move_ph: "Destination folder",
    rules_hint: "Rules apply to newly received mail only and take effect on save",
    contacts_hdr: "Contacts (auto-collected)",
    no_contacts: "No contacts yet (collected from incoming mail)",
    acct_n: "Account {}",
    contacts_n: "{} ({})",
    contacts_more: "… {} total (first 50 shown)",
    cfg_note: "Config lives at ~/.s-mail/config.toml (UI edits apply instantly; manual edits need restart)",
    pgp_note: "PGP: decrypt/verify on read + sign/encrypt on send (all three empty = remove)",
    key_label: "Private key",
    key_ph: "~/.s-mail/<account>/pgp/secret.asc",
    passphrase_label: "Passphrase",
    passphrase_ph: "Optional (unprotected key)",
    pubring_label: "Pubring",
    pubring_ph: "~/.s-mail/<account>/pgp/pub.asc (optional = no verify)",
    wkd_label: "WKD auto-discovery (queries recipient's domain when key missing; off by default)",
    pick_file: "Pick file…",

    ai_section: "AI Assistant",
    signature_btn: "Signature",
    signature_label: "Signature (appended to new messages; empty = remove)",
    ai_status_on: "Enabled · {}",
    ai_status_off: "Disabled",
    ai_setup: "Settings",
    ai_enabled_label: "Enable AI (drafting / summaries / organizing)",
    ai_base_url_label: "API base URL",
    ai_model_label: "Model",
    ai_api_key_label: "API key",
    ai_keychain_btn: "Keychain",
    ai_test_ok: "✓ Connected ({})",
    ai_summary_btn: "✨ Summarize",
    ai_working: "⏳ AI working…",
    ai_instr_ph: "AI instruction (e.g. politely decline Thursday's meeting, propose Tuesday)",
    ai_draft_btn: "✨ Draft",
    ai_overwrite_q: "AI will overwrite subject and body. Continue?",
    ai_overwrite_yes: "Overwrite",
    ai_smart_reply: "✨ Smart reply",
    ai_reply_instruction: "Reply to this email, professional and friendly tone, ask any necessary questions",
    ai_organize_btn: "✨ Organize selected",
    ai_plan_loading: "⏳ AI is analyzing selected messages…",
    ai_plan_apply: "Apply checked",
    ai_plan_move_to: "Move to {}",
    ai_plan_group_n: " ({})",
    ai_err_disabled: "AI is disabled (enable in settings)",
    ai_err_too_many: "Organize works on at most {} messages at once (narrow the selection)",
    ai_err_no_instr: "Write an AI instruction first",
    ai_err_empty_body: "Body is empty (write something first, or use ✨ Draft)",
    ai_summary_thread: "✨ Summarize thread",
    ai_summary_cached: "✨ Summary ✓",
    ai_summary_hide: "✨ Hide summary",
    ai_polish: "Polish",
    ai_formal: "More formal",
    ai_casual: "More casual",
    ai_shorter: "Shorter",
    ai_longer: "Longer",
    ai_to_zh: "→中文",
    ai_to_en: "→English",
    ai_instr_polish: "Polish this email: keep meaning and facts, make it fluent and natural, fix typos",
    ai_instr_formal: "Make the tone more formal and professional",
    ai_instr_casual: "Make the tone more relaxed and conversational (still polite)",
    ai_instr_shorter: "Significantly shorten the body, keep only essentials",
    ai_instr_longer: "Expand the body moderately, more complete (do not invent facts)",
    ai_instr_to_zh: "Translate this email into Chinese (keep content and layout)",
    ai_instr_to_en: "Translate this email into English (keep content and layout)",
    ai_rewrite_btn: "↻ Rewrite",
    ai_undo: "↩ Undo AI",
    ai_applied_n: "Applied {} AI group(s)",
    ai_privacy_title: "AI privacy notice",
    ai_privacy_text: "Message content will be sent to your AI provider ({}). Decrypted PGP mail is included. Local Ollama stays on this machine.",
    ai_privacy_go: "Got it, continue",

    err_need_account: "Add an account first (top-right ＋)",
    err_need_account_search: "Add an account first",
    err_empty_to: "Recipient required",
    err_folder_name: "Folder name required",
    err_folder_rename: "New folder name required",
    err_no_plain_pwd: "No plaintext password to migrate (OAuth or already in keychain)",
    err_orig_missing: "Original message not found",
    err_save_cfg: "Failed to save config: {}",
    err_reload: "Failed to reload session: {}",
    err_port: "Invalid IMAP port",
    oauth_no_test: "OAuth accounts authorize after save (no test needed)",
    err_dup: "{} is already configured",
    err_conn: "Connection failed: {}. Edit again, or \"Save anyway\"",
};

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f_fills_positional_args_in_order() {
        assert_eq!(f("{} + {}", &[&"a", &"b"]), "a + b");
        assert_eq!(f(ZH.unified_hdr, &[&"7"]), "所有收件箱（7 封）");
        assert_eq!(f(EN.contacts_n, &[&"工作", &"3"]), "工作 (3)");
    }

    #[test]
    fn folder_display_localizes_special_folders() {
        assert_eq!(
            folder_display(&ZH, smail_core::FolderKind::Inbox, "INBOX"),
            "收件箱"
        );
        assert_eq!(
            folder_display(&EN, smail_core::FolderKind::Sent, "Sent Messages"),
            "Sent"
        );
        // 自定义文件夹显示末段原名
        assert_eq!(
            folder_display(&ZH, smail_core::FolderKind::Plain, "客户/甲"),
            "甲"
        );
    }

    #[test]
    fn prefs_round_trip() {
        let dir = std::env::temp_dir().join(format!("smail-ui-prefs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = UiPrefs {
            lang: Lang::En,
            dark: Some(true),
            ai_ack: false,
            win: None,
            last_view: None,
            columns: Some((240.0, 420.0)),
            muted: vec!["spam@x.com".into()],
        };
        save_prefs(&dir, &p);
        let p = load_prefs(&dir);
        assert_eq!(p.lang, Lang::En);
        assert_eq!(p.dark, Some(true));
        assert_eq!(p.columns, Some((240.0, 420.0)), "列宽记忆往返");
        assert_eq!(p.muted, vec!["spam@x.com".to_string()], "静音列表往返");
        // 默认（无偏好文件）：中文 + 跟随系统
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let p = load_prefs(&dir);
        assert_eq!(p.lang, Lang::Zh);
        assert_eq!(p.dark, None);
        assert_eq!(p.columns, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
