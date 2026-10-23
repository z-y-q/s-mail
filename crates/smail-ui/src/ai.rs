//! AI 助手 UI 层：状态、请求派发、结果落地。
//!
//! 安全要点（对应设计方案）：
//! - 执行边界：本模块只产生数据（摘要文本 / 草稿内容 / 整理计划预览），
//!   `AiPlanApply` 构造的是与手工批量操作完全相同的 Cmd——用户点击即执行，
//!   复用既有乐观更新与同步链路；
//! - 在途守卫：`in_flight` id 匹配才落地（同 body_req 模式），切邮件/关屏后
//!   迟到响应丢弃；
//! - 隐私门：首次使用前 `privacy_pending` 挡一道确认（ai_ack 记入 ui.json）；
//! - 整理计划：白名单解析 + hash 归属校验 + 文件夹白名单（Junk/Drafts/
//!   当前文件夹按构造排除，Delete 不在词汇表）。

use std::collections::HashMap;

use crate::{Message, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusyKind {
    Summarize,
    Draft,
    Organize,
}

pub(crate) struct AiForm {
    pub(crate) preset: smail_ai::Preset,
    pub(crate) enabled: bool,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) api_key: String,
    pub(crate) testing: bool,
    pub(crate) test_result: Option<Result<String, String>>,
    pub(crate) err: Option<String>,
}

impl AiForm {
    fn from_cfg(cfg: &smail_ai::AiConfig) -> Self {
        let preset = smail_ai::Preset::ALL
            .into_iter()
            .find(|p| !p.base_url().is_empty() && p.base_url() == cfg.base_url)
            .unwrap_or(smail_ai::Preset::Custom);
        Self {
            preset,
            enabled: cfg.enabled,
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            // 钥匙串标记不在表单里回显明文；留空=保存时保留原值
            api_key: if smail_core::keychain::is_marker(&cfg.api_key) {
                String::new()
            } else {
                cfg.api_key.clone()
            },
            testing: false,
            test_result: None,
            err: None,
        }
    }
    pub(crate) fn apply_preset(&mut self) {
        if self.preset == smail_ai::Preset::Custom {
            return;
        }
        self.base_url = self.preset.base_url().into();
        if self.model.is_empty() {
            self.model = self.preset.default_model().into();
        }
        self.test_result = None;
    }
}

/// 预览里的一组（checked=应用勾选；expanded=展开看成员清单）
pub(crate) struct PlanUi {
    pub(crate) group: smail_ai::PlanGroup,
    pub(crate) checked: bool,
    pub(crate) expanded: bool,
}

pub(crate) struct AiUiState {
    pub(crate) cfg: smail_ai::AiConfig,
    /// 隐私确认（持久化 ui.json）
    pub(crate) ack: bool,
    /// 在途请求 id（0=空闲）
    pub(crate) in_flight: u64,
    pub(crate) next_id: u64,
    pub(crate) busy: Option<BusyKind>,
    /// 隐私确认后要重发的动作
    pub(crate) privacy_pending: Option<Message>,
    /// 摘要缓存（(账号, hash) → 文本；内存 + ai.db 双层，跨启动保留）
    pub(crate) summary_cache: HashMap<(usize, i64), String>,
    /// 持久层（ai.db；打开失败降级纯内存）
    pub(crate) cache: Option<smail_store::AiCache>,
    pub(crate) summary_visible: bool,
    /// 整理计划预览（组 + 勾选态 + 展开态）
    pub(crate) plan: Option<Vec<PlanUi>>,
    pub(crate) plan_loading: bool,
    /// 整理请求时的编号→(账号,hash) 快照（回包时按当前选择校验）
    pub(crate) plan_ctx: HashMap<usize, (usize, i64)>,
    pub(crate) form: Option<AiForm>,
}

impl AiUiState {
    pub(crate) fn new(cfg: smail_ai::AiConfig, data_root: &std::path::Path) -> Self {
        Self {
            cfg,
            ack: false,
            in_flight: 0,
            next_id: 1,
            busy: None,
            privacy_pending: None,
            cache: smail_store::AiCache::open(&data_root.join("ai.db")).ok(),
            summary_cache: HashMap::new(),
            summary_visible: false,
            plan: None,
            plan_loading: false,
            plan_ctx: HashMap::new(),
            form: None,
        }
    }
}

pub(crate) fn ai_enabled(state: &State) -> bool {
    state.ai.cfg.enabled && !state.ai.cfg.base_url.is_empty() && !state.ai.cfg.model.is_empty()
}

/// 统一入口门：未启用报错；未确认隐私则挂起待确认后重发。
/// 返回 true = 已放行（隐私门通过）
fn gate(state: &mut State, pending: Message) -> bool {
    if !ai_enabled(state) {
        state.error = Some(state.l().ai_err_disabled.into());
        return false;
    }
    if !state.ai.ack {
        state.ai.privacy_pending = Some(pending);
        return false;
    }
    true
}

/// 解析 API key（keychain 标记 → 明文）。失败即报错并返回 None。
fn resolve_key(state: &State) -> Option<String> {
    let raw = state.ai.cfg.api_key.clone();
    if smail_core::keychain::is_marker(&raw) {
        match smail_core::keychain::resolve(&raw) {
            Ok(k) => Some(k),
            Err(e) => {
                // 借用限制：这里只返回 None，错误由调用方写（需要 &mut）
                let _ = e;
                None
            }
        }
    } else {
        Some(raw)
    }
}

fn make_chat(state: &State) -> Result<smail_ai::Chat, String> {
    let key = resolve_key(state).ok_or_else(|| "AI API Key 钥匙串读取失败".to_string())?;
    Ok(smail_ai::Chat {
        base_url: state.ai.cfg.base_url.clone(),
        api_key: key,
        model: state.ai.cfg.model.clone(),
    })
}

/// 本地正文（服务线程打开邮件时落盘的 .eml；不出网）。
/// 在 spawn_blocking 里调用（磁盘 IO + MIME 解析）。
fn eml_text(data_root: &std::path::Path, slug: &str, hash: i64) -> Option<String> {
    let p = data_root
        .join(slug)
        .join("bodies")
        .join(format!("{hash}.eml"));
    let bytes = std::fs::read(p).ok()?;
    let (text, _) = smail_core::extract_body(&bytes);
    Some(text)
}

fn clip_str(s: &str, limit: usize) -> String {
    if s.chars().count() > limit {
        s.chars().take(limit).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

fn zh(state: &State) -> bool {
    matches!(state.prefs.lang, crate::lang::Lang::Zh)
}

fn new_req(state: &mut State, kind: BusyKind) -> u64 {
    let id = state.ai.next_id;
    state.ai.next_id += 1;
    state.ai.in_flight = id;
    state.ai.busy = Some(kind);
    id
}

// ---------------------------------------------------------------------------
// 摘要
// ---------------------------------------------------------------------------

pub(crate) fn start_summarize(state: &mut State, acct: usize, hash: i64) -> iced::Task<Message> {
    if state.ai.busy.is_some() {
        return iced::Task::none();
    }
    if !gate(state, Message::AiSummarize(acct, hash)) {
        return iced::Task::none();
    }
    let key = (acct, hash);
    // 三级缓存：内存 → ai.db → 网络（点已缓存的邮件 = 收起/展开卡片）
    if state.ai.summary_cache.contains_key(&key)
        || state
            .ai
            .cache
            .as_ref()
            .map(|c| c.get_summary(acct, hash).is_some())
            .unwrap_or(false)
    {
        if let Some(text) = state.ai.summary_cache.get(&key).cloned() {
            state.ai.summary_visible = !state.ai.summary_visible;
            let _ = text;
            return iced::Task::none();
        }
        if let Some(c) = &state.ai.cache {
            if let Some(text) = c.get_summary(acct, hash) {
                state.ai.summary_cache.insert(key, text);
                state.ai.summary_visible = true;
                return iced::Task::none();
            }
        }
    }
    let Some((ra, reading)) = &state.reading else {
        return iced::Task::none();
    };
    if *ra != acct || reading.hash != hash {
        return iced::Task::none();
    }
    let (body_now, subject_now) = (reading.body.clone(), reading.subject.clone());
    // 会话聚合：文件夹会话视图下，把同会话成员的本地正文拼起来做整场讨论摘要
    // （当前邮件正文在内存；其余成员读 .eml 本地缓存）
    let mut members: Vec<(i64, String, String)> = Vec::new(); // (hash, from, snippet?)
    let mut thread_subject = subject_now.clone();
    let in_folder_thread = state.nav.threaded && !state.nav.in_search && !state.nav.viewing_unified;
    if in_folder_thread {
        let flat: Vec<smail_store::MessageRow> = state
            .rows
            .iter()
            .filter(|(a, _)| *a == acct)
            .map(|(_, r)| r.clone())
            .collect();
        for t in smail_core::threads::group_threads(&flat) {
            if t.members.iter().any(|m| m.envelope_hash == hash) {
                if t.members.len() > 1 {
                    thread_subject = t.root().subject.clone();
                    for m in &t.members {
                        let snip = if m.envelope_hash == hash {
                            Some(body_now.clone())
                        } else {
                            None // 阻塞任务里读 .eml
                        };
                        members.push((
                            m.envelope_hash,
                            m.from_addr.clone(),
                            snip.unwrap_or_default(),
                        ));
                    }
                }
                break;
            }
        }
    }
    let from = state
        .rows
        .iter()
        .find(|(a, r)| *a == acct && r.envelope_hash == hash)
        .map(|(_, r)| r.from_addr.clone())
        .unwrap_or_default();
    let Some(chat) = make_chat(state).map_err(|e| state.error = Some(e)).ok() else {
        return iced::Task::none();
    };
    let id = new_req(state, BusyKind::Summarize);
    let is_zh = zh(state);
    let data_root = state.data_root.clone();
    let slug = state
        .accounts
        .get(acct)
        .map(|a| a.slug.clone())
        .unwrap_or_default();
    let single = smail_ai::MailCtx {
        subject: subject_now,
        from,
        body: body_now,
    };
    iced::Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                if members.is_empty() {
                    smail_ai::summarize(&chat, is_zh, &single)
                } else {
                    let items: Vec<smail_ai::ThreadItem> = members
                        .iter()
                        .map(|(h, f, snip)| {
                            let snippet = if snip.is_empty() {
                                eml_text(&data_root, &slug, *h)
                                    .map(|t| clip_str(&t, 2500))
                                    .unwrap_or_else(|| "[正文未缓存]".into())
                            } else {
                                snip.clone()
                            };
                            smail_ai::ThreadItem {
                                from: f.clone(),
                                snippet,
                            }
                        })
                        .collect();
                    smail_ai::summarize_thread(&chat, is_zh, &thread_subject, &items)
                }
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        },
        move |result| Message::AiSummaryReady {
            acct,
            hash,
            id,
            result,
        },
    )
}

pub(crate) fn summary_ready(
    state: &mut State,
    acct: usize,
    hash: i64,
    id: u64,
    result: Result<String, String>,
) {
    if state.ai.in_flight != id {
        return; // 迟到/已切换：丢弃
    }
    state.ai.in_flight = 0;
    state.ai.busy = None;
    match result {
        Ok(text) => {
            // 阅读器还在这封上才显示（同 MarkdownParsed 守卫语义）
            let still = state
                .reading
                .as_ref()
                .map(|(a, r)| *a == acct && r.hash == hash)
                .unwrap_or(false);
            // 写穿持久层（跨启动保留；顺手淘汰旧条目防膨胀）
            if let Some(c) = &state.ai.cache {
                c.put_summary(acct, hash, &text);
                c.prune(200);
            }
            state.ai.summary_cache.insert((acct, hash), text);
            state.ai.summary_visible = still;
        }
        Err(e) => state.error = Some(format!("AI 摘要失败：{e}")),
    }
}

// ---------------------------------------------------------------------------
// 起草
// ---------------------------------------------------------------------------

pub(crate) fn start_draft(
    state: &mut State,
    instruction: String,
    orig: Option<smail_ai::MailCtx>,
) -> iced::Task<Message> {
    if state.ai.busy.is_some() {
        return iced::Task::none();
    }
    if !gate(state, Message::AiDraftGo) {
        return iced::Task::none();
    }
    if instruction.trim().is_empty() {
        state.error = Some(state.l().ai_err_no_instr.into());
        return iced::Task::none();
    }
    let Some(chat) = make_chat(state).map_err(|e| state.error = Some(e)).ok() else {
        return iced::Task::none();
    };
    if let crate::Screen::Compose(c) = &mut state.screen {
        c.ai_busy = true;
    }
    let id = new_req(state, BusyKind::Draft);
    let is_zh = zh(state);
    iced::Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                smail_ai::draft(&chat, is_zh, &instruction, orig.as_ref())
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        },
        move |result| Message::AiDraftReady { id, result },
    )
}

/// 改写现有正文（润色/语气/长度/翻译）：指令是预设文案，正文取自编辑器
pub(crate) fn start_rewrite(state: &mut State, instruction: String) -> iced::Task<Message> {
    if state.ai.busy.is_some() {
        return iced::Task::none();
    }
    if !gate(state, Message::AiRewrite(instruction.clone())) {
        return iced::Task::none();
    }
    let (subject, body) = match &state.screen {
        crate::Screen::Compose(c) => (c.subject.clone(), c.body.text().to_string()),
        _ => return iced::Task::none(),
    };
    if body.trim().is_empty() && subject.trim().is_empty() {
        state.error = Some(state.l().ai_err_empty_body.into());
        return iced::Task::none();
    }
    let Some(chat) = make_chat(state).map_err(|e| state.error = Some(e)).ok() else {
        return iced::Task::none();
    };
    if let crate::Screen::Compose(c) = &mut state.screen {
        c.ai_busy = true;
        c.ai_rewrite_mode = true;
    }
    let id = new_req(state, BusyKind::Draft);
    let is_zh = zh(state);
    iced::Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                smail_ai::rewrite(&chat, is_zh, &instruction, &subject, &body)
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        },
        move |result| Message::AiDraftReady { id, result },
    )
}

pub(crate) fn draft_ready(
    state: &mut State,
    id: u64,
    result: Result<smail_ai::DraftContent, String>,
) {
    if state.ai.in_flight != id {
        return;
    }
    state.ai.in_flight = 0;
    state.ai.busy = None;
    let crate::Screen::Compose(c) = &mut state.screen else {
        return; // 用户已关写作屏：丢弃
    };
    c.ai_busy = false;
    let rewrite = c.ai_rewrite_mode;
    match result {
        Ok(d) => {
            let has_content = !c.subject.is_empty() || !c.body.text().trim().is_empty();
            // 改写（润色/翻译等）：有「撤销 AI」兜底，跳过确认条直接应用——
            // 快速迭代不该每步都点确认；从零起草仍保留覆盖确认
            if rewrite {
                stash_prev(state);
                fill_draft(state, d);
                return;
            }
            if has_content && !c.ai_overwrite_ask {
                c.ai_overwrite_ask = true;
                c.ai_pending = Some(d);
                return;
            }
            stash_prev(state);
            fill_draft(state, d);
        }
        Err(e) => state.error = Some(format!("AI 起草失败：{e}")),
    }
}

/// AI 改写前的快照（单步撤销）
fn stash_prev(state: &mut State) {
    if let crate::Screen::Compose(c) = &mut state.screen {
        let snap = (c.subject.clone(), c.body.text().to_string());
        // 连续多次 AI 操作只保留最早快照（撤销回到 AI 介入前）
        if c.ai_prev.is_none() {
            c.ai_prev = Some(snap);
        }
    }
}

/// 撤销上一次 AI 修改
pub(crate) fn draft_undo(state: &mut State) {
    if let crate::Screen::Compose(c) = &mut state.screen {
        if let Some((subject, body)) = c.ai_prev.take() {
            c.subject = subject;
            let mut content = iced::widget::text_editor::Content::new();
            content.perform(crate::compose::text_editor_edit_paste(&body));
            c.body = content;
            c.ai_overwrite_ask = false;
            c.ai_pending = None;
        }
    }
}

fn fill_draft(state: &mut State, d: smail_ai::DraftContent) {
    if let crate::Screen::Compose(c) = &mut state.screen {
        c.subject = d.subject;
        let mut content = iced::widget::text_editor::Content::new();
        content.perform(crate::compose::text_editor_edit_paste(&d.body));
        c.body = content;
        c.ai_overwrite_ask = false;
        c.ai_pending = None;
        c.ai_rewrite_mode = false;
        // AI 生成的内容也需要自动保存保护（旧版漏标——用户 AI 写完直接关
        // 会走 discard confirm 而非已保存）
        c.autosave_at = Some(std::time::Instant::now());
    }
}

pub(crate) fn draft_apply_overwrite(state: &mut State) {
    let pending = match &mut state.screen {
        crate::Screen::Compose(c) => {
            c.ai_overwrite_ask = false;
            c.ai_pending.take()
        }
        _ => None,
    };
    if let Some(d) = pending {
        fill_draft(state, d);
    }
}

pub(crate) fn draft_discard_overwrite(state: &mut State) {
    if let crate::Screen::Compose(c) = &mut state.screen {
        c.ai_overwrite_ask = false;
        c.ai_pending = None;
    }
}

/// 智能回复：带原信上下文起草（先按普通回复开屏，再自动起草）
pub(crate) fn smart_reply(state: &mut State, acct: usize, hash: i64) -> iced::Task<Message> {
    if state.ai.busy.is_some() {
        return iced::Task::none();
    }
    if !gate(state, Message::AiSmartReply(acct, hash)) {
        return iced::Task::none();
    }
    let Some((ra, reading)) = &state.reading else {
        return iced::Task::none();
    };
    if *ra != acct || reading.hash != hash {
        return iced::Task::none();
    }
    let from = state
        .rows
        .iter()
        .find(|(a, r)| *a == acct && r.envelope_hash == hash)
        .map(|(_, r)| r.from_addr.clone())
        .unwrap_or_default();
    let orig = smail_ai::MailCtx {
        subject: reading.subject.clone(),
        from,
        body: reading.body.clone(),
    };
    let instruction = state.l().ai_reply_instruction.to_string();
    // 开写作屏（普通回复的收件人/主题逻辑），再走起草
    crate::compose::open_compose_with(state, acct, hash, crate::compose::ReplyKind::Reply);
    if let crate::Screen::Compose(c) = &mut state.screen {
        c.ai_instruction = instruction.clone();
    }
    start_draft(state, instruction, Some(orig))
}

// ---------------------------------------------------------------------------
// 整理建议
// ---------------------------------------------------------------------------

pub(crate) fn organize_go(state: &mut State) -> iced::Task<Message> {
    if state.ai.busy.is_some() {
        return iced::Task::none();
    }
    let max = state.ai.cfg.max_mails_per_organize;
    if state.selection.is_empty() || state.selection.len() > max {
        state.error = Some(crate::lang::f(state.l().ai_err_too_many, &[&max]));
        return iced::Task::none();
    }
    if !gate(state, Message::AiOrganizeGo) {
        return iced::Task::none();
    }
    // 快照（编号 → 账号/hash + 元数据）；正文片段在阻塞任务里读本地 .eml
    let mut sel: Vec<(usize, i64)> = state.selection.iter().copied().collect();
    sel.sort_unstable();
    let mut by_idx = HashMap::new();
    /// (编号, 账号, hash, 主题, 发件人, 日期, 已读, 星标)
    type MetaBase = (usize, usize, i64, String, String, String, bool, bool);
    let mut metas_base: Vec<MetaBase> = Vec::new();
    for (i, (a, h)) in sel.iter().enumerate() {
        let Some(row) = state
            .rows
            .iter()
            .find(|(ra, r)| *ra == *a && r.envelope_hash == *h)
            .map(|(_, r)| r.clone())
        else {
            continue; // 不在当前列表：跳过
        };
        by_idx.insert(i, (*a, *h));
        metas_base.push((
            i,
            *a,
            *h,
            row.subject,
            row.from_addr,
            crate::unix_to_local(row.date_unix),
            row.seen,
            row.flagged,
        ));
    }
    let metas_len = metas_base.len();
    if metas_len == 0 {
        return iced::Task::none();
    }
    // 文件夹白名单（排除 Junk/Drafts/当前文件夹——按构造排除，Junk 永不建议）
    let mut folders_by_acct: HashMap<usize, Vec<String>> = HashMap::new();
    for a in sel
        .iter()
        .map(|(a, _)| *a)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let cur = state
            .nav
            .cur
            .as_ref()
            .filter(|(ca, _)| *ca == a)
            .map(|(_, c)| c.clone());
        let list = state
            .nav
            .folders
            .iter()
            .filter(|(fa, _)| *fa == a)
            .filter(|(_, f)| f.kind != smail_core::FolderKind::Junk)
            .filter(|(_, f)| f.kind != smail_core::FolderKind::Drafts)
            .filter(|(_, f)| f.kind != smail_core::FolderKind::Trash)
            .filter(|(_, f)| cur.as_deref() != Some(f.path.as_str()))
            .map(|(_, f)| f.path.clone())
            .collect::<Vec<String>>();
        folders_by_acct.insert(a, list);
    }
    // prompt 里给去重的文件夹名（跨账号合并展示，回包按名匹配各账号校验）
    let mut names: Vec<String> = Vec::new();
    for fs in folders_by_acct.values() {
        for f in fs {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(f)) {
                names.push(f.clone());
            }
        }
    }
    let Some(chat) = make_chat(state).map_err(|e| state.error = Some(e)).ok() else {
        return iced::Task::none();
    };
    state.ai.plan = None;
    state.ai.plan_loading = true;
    state.ai.plan_ctx = by_idx;
    let id = new_req(state, BusyKind::Organize);
    let is_zh = zh(state);
    let data_root = state.data_root.clone();
    let slugs: HashMap<usize, String> = state
        .accounts
        .iter()
        .enumerate()
        .map(|(i, a)| (i, a.slug.clone()))
        .collect();
    iced::Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                // 本地片段（400 字）：未打开过的邮件没有 .eml，片段留空照样可用
                let metas: Vec<smail_ai::MailMeta> = metas_base
                    .iter()
                    .map(|(idx, a, h, subj, from, date, seen, flagged)| {
                        let snippet = slugs
                            .get(a)
                            .and_then(|slug| eml_text(&data_root, slug, *h))
                            .map(|t| clip_str(&t, 400))
                            .unwrap_or_default();
                        smail_ai::MailMeta {
                            idx: *idx,
                            subject: subj.clone(),
                            from: from.clone(),
                            date: date.clone(),
                            seen: *seen,
                            flagged: *flagged,
                            snippet,
                        }
                    })
                    .collect();
                smail_ai::organize(&chat, is_zh, &metas, &names)
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        },
        move |result| Message::AiPlanReady { id, result },
    )
}

pub(crate) fn plan_ready(
    state: &mut State,
    id: u64,
    result: Result<Vec<smail_ai::PlanGroup>, String>,
) {
    if state.ai.in_flight != id {
        return;
    }
    state.ai.in_flight = 0;
    state.ai.busy = None;
    state.ai.plan_loading = false;
    match result {
        Ok(raw) => {
            // 落地前按「当前选择」复核归属（请求期间用户可能已改选），
            // 文件夹白名单用当前列表重建
            let cur: std::collections::HashSet<(usize, i64)> = state.selection.clone();
            let mut folders = HashMap::new();
            for a in cur
                .iter()
                .map(|(a, _)| *a)
                .collect::<std::collections::BTreeSet<_>>()
            {
                let list = state
                    .nav
                    .folders
                    .iter()
                    .filter(|(fa, _)| *fa == a)
                    .filter(|(_, f)| f.kind != smail_core::FolderKind::Junk)
                    .filter(|(_, f)| f.kind != smail_core::FolderKind::Drafts)
                    .map(|(_, f)| f.path.clone())
                    .collect::<Vec<String>>();
                folders.insert(a, list);
            }
            // 不在当前选择里的编号整条移除（请求期间用户可能已改选）
            let by_idx: HashMap<usize, (usize, i64)> = state
                .ai
                .plan_ctx
                .iter()
                .filter(|(_, v)| cur.contains(v))
                .map(|(k, v)| (*k, *v))
                .collect();
            let groups = smail_ai::validate_plan(raw, &by_idx, &folders);
            if groups.is_empty() {
                state.error = Some("AI 没有给出可用的整理建议".into());
            } else {
                state.ai.plan = Some(
                    groups
                        .into_iter()
                        .map(|g| PlanUi {
                            group: g,
                            checked: true,
                            expanded: false,
                        })
                        .collect(),
                );
            }
        }
        Err(e) => state.error = Some(format!("AI 整理失败：{e}")),
    }
}

pub(crate) fn plan_apply(state: &mut State) {
    let Some(groups) = state.ai.plan.take() else {
        return;
    };
    let mut applied: Vec<(usize, i64)> = Vec::new();
    let mut applied_groups = 0usize;
    for PlanUi {
        group: g,
        checked,
        expanded: _,
    } in groups
    {
        if !checked {
            continue;
        }
        applied_groups += 1;
        // 按账号分组路由（与手工批量同构）
        let mut by: std::collections::BTreeMap<usize, Vec<i64>> = Default::default();
        for (a, h) in &g.ids {
            by.entry(*a).or_default().push(*h);
            applied.push((*a, *h));
        }
        for (a, hs) in by {
            match &g.op {
                smail_ai::PlanOp::MarkSeen => {
                    state.send(
                        a,
                        smail_core::Cmd::BatchFlags {
                            hashes: hs,
                            seen: Some(true),
                            flagged: None,
                        },
                    );
                }
                smail_ai::PlanOp::MarkStar => {
                    state.send(
                        a,
                        smail_core::Cmd::BatchFlags {
                            hashes: hs,
                            seen: None,
                            flagged: Some(true),
                        },
                    );
                }
                smail_ai::PlanOp::MoveTo(dest) => {
                    // 与 BatchMoveDest 相同的乐观移除 + 后台搬运
                    state
                        .rows
                        .retain(|(ra, r)| !(*ra == a && hs.contains(&r.envelope_hash)));
                    state.send(
                        a,
                        smail_core::Cmd::BatchMove {
                            hashes: hs,
                            dest: dest.clone(),
                        },
                    );
                }
            }
        }
    }
    if !applied.is_empty() {
        for (a, h) in &applied {
            state.selection.remove(&(*a, *h));
        }
        state.rebuild_display();
        crate::close_reading_if_gone(state);
        // 结果反馈（顶栏状态；按涉及账号逐个报）
        let accts: std::collections::BTreeSet<usize> = applied.iter().map(|(a, _)| *a).collect();
        for a in accts {
            state.statuses.insert(
                a,
                crate::lang::f(state.l().ai_applied_n, &[&applied_groups]),
            );
        }
    }
    state.ai.plan_ctx.clear();
}

// ---------------------------------------------------------------------------
// 设置表单
// ---------------------------------------------------------------------------

pub(crate) fn form_save(state: &mut State) {
    let Some(f) = state.ai.form.take() else {
        return;
    };
    if f.base_url.trim().is_empty() || f.model.trim().is_empty() {
        state.error = Some("AI 需要填 API 地址与模型".into());
        state.ai.form = Some(f);
        return;
    }
    // api_key 留空 = 保留原值（钥匙串标记场景）
    let api_key = if f.api_key.trim().is_empty() {
        state.ai.cfg.api_key.clone()
    } else {
        f.api_key.trim().to_string()
    };
    let cfg = smail_ai::AiConfig {
        enabled: f.enabled,
        base_url: f.base_url.trim().to_string(),
        api_key,
        model: f.model.trim().to_string(),
        max_mails_per_organize: state.ai.cfg.max_mails_per_organize,
    };
    let path = state.cfg_path.with_file_name("ai.toml");
    if let Err(e) = smail_ai::save_config(&path, &cfg) {
        state.error = Some(format!("AI 配置保存失败：{e}"));
        state.ai.form = Some(f);
        return;
    }
    state.ai.cfg = cfg;
}

pub(crate) fn form_keychain_move(state: &mut State) {
    let Some(f) = &mut state.ai.form else {
        return;
    };
    if f.api_key.trim().is_empty() {
        f.err = Some("先在输入框里填 API Key".into());
        return;
    }
    match smail_core::keychain::store("ai/apikey", f.api_key.trim()) {
        Ok(()) => {
            f.api_key = String::new();
            f.test_result = None;
        }
        Err(e) => f.err = Some(e),
    }
}

pub(crate) fn form_from_cfg(state: &State) -> AiForm {
    AiForm::from_cfg(&state.ai.cfg)
}
