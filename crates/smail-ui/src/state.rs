//! 应用状态管理
//!
//! ## 职责
//!
//! - **State 结构体**：应用全局状态（会话、导航、视图、批量操作）
//! - **Screen 枚举**：视图切换（邮件列表/写信/设置）
//! - **Reading 结构体**：阅读器状态（正文、附件、图片）
//! - **启动装载**：配置加载 + 会话重建
//! - **状态持久化**：账号配置保存、草稿自动存储
//!
//! ## State 拆分（M7 重构）
//!
//! 原 State 单体拆分为子结构体：
//! - `Nav`：导航状态（当前账号/文件夹、搜索模式、选中项）
//! - `BatchOps`：批量操作（勾选列表、操作类型）
//! - `FolderEdit`：文件夹管理（创建/删除/重命名表单）
//!
//! ## 关键类型
//!
//! - `Screen`：主视图（Mail/Compose/Setup）
//! - `Reading`：阅读器精确身份（hash、主题、正文、附件）
//! - `MailDisp`：邮件列表显示单元（会话聚合后的展示行）
#![allow(clippy::module_inception)]
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use iced::Task;
use iced::Theme;

use iced::widget::text_editor;

use crate::{list, list::build_mail_display, setup, Compose, EventTap, Message, SetupForm};
use smail_core::{Cmd, FolderInfo, Session, SessionCmd};
use smail_store::{DraftRow, MessageRow};
use std::sync::Arc;

pub(crate) enum Screen {
    Mail,
    Compose(Compose),
    /// M6：账号配置（首次运行引导 / 顶栏 ＋ 进入）
    Setup(SetupForm),
}

pub(crate) struct Reading {
    /// 本封邮件 hash：阅读器自带身份，工具栏/远程图/解析回填不再从
    /// selected 推断（两者漂移时曾会向服务器发 hash=0 的命令）
    pub(crate) hash: i64,
    pub(crate) subject: String,
    pub(crate) body: String,
    /// 原信收件人（回复全部用）
    pub(crate) to_addrs: String,
    pub(crate) cc_addrs: String,
    /// List-Unsubscribe 退订入口（http/mailto）
    pub(crate) unsubscribe: Option<String>,
    /// 已可显示的图片（内嵌 cid 自动 + 远程按需加载后回填）
    pub(crate) images: Vec<iced::widget::image::Handle>,
    /// 未加载的远程图片 URL（显示「显示图片」按钮）
    pub(crate) remote_pending: Vec<String>,
    /// 图片附件（预览）：(文件名, 显示句柄)
    pub(crate) attach_images: Vec<(String, iced::widget::image::Handle)>,
    /// PDF 附件缩略图（QuickLook PNG 预览）
    pub(crate) attach_previews: Vec<(String, iced::widget::image::Handle)>,
    /// 远程图下载中（防重复请求；失败后可重试——pending 不在点击时清空）
    pub(crate) remote_loading: bool,
    pub(crate) md: Option<iced::widget::markdown::Content>,
    pub(crate) attachments: Vec<(String, usize)>,
    /// M5-c：PGP 状态徽章（None=非 PGP）
    pub(crate) pgp: Option<String>,
}

/// markdown 解析结果一次性传递盒：Content 无 Clone 而 iced 要求 Message: Clone，
/// 只能共享句柄（clone 只克隆 Arc），接收方 lock 后 take 取走即空
#[derive(Debug, Clone)]
pub(crate) struct ParsedMd(
    pub(crate) Arc<std::sync::Mutex<Option<iced::widget::markdown::Content>>>,
);

/// 一行 =（账号, 邮件）
pub(crate) type RowEntry = (usize, MessageRow);

/// 列表展示条目（M5 会话视图）
pub(crate) struct MailDisp {
    pub(crate) acct: usize,
    pub(crate) row: MessageRow,
    pub(crate) kind: DispKind,
}

pub(crate) enum DispKind {
    Flat,
    ThreadRoot {
        latest: i64,
        count: usize,
        unseen: bool,
        last_from: String,
        last_date: i64,
    },
    ThreadChild,
}

/// UI 偏好（主题/语言/窗口/上次视图）——State 拆分第一组
pub(crate) struct Prefs {
    pub(crate) ui_theme: Theme,
    /// 未手动切换时跟随系统深浅色
    pub(crate) theme_follow: bool,
    /// UI 语言（中文默认；顶栏可切英文，存 ui.json）
    pub(crate) lang: crate::lang::Lang,
    /// 窗口几何（w,h,x,y；拖动/缩放时更新，节流落盘 ui.json）
    pub(crate) win_geom: Option<[f32; 4]>,
    pub(crate) win_save_at: Option<std::time::Instant>,
    /// 上次视图（账号名, 文件夹/伪视图；启动恢复，离线直达缓存）
    pub(crate) last_view: Option<(String, String)>,
}

/// 导航与视图状态（文件夹/搜索/列表显示/分页）——State 拆分第二组
pub(crate) struct Nav {
    pub(crate) folders: Vec<(usize, FolderInfo)>,
    /// 当前视图：(账号, 文件夹 | "__drafts__" | "__outbox__" | "__all_inbox__")
    pub(crate) cur: Option<(usize, String)>,
    pub(crate) in_search: bool,
    /// 统一收件箱视图（多账号 INBOX 聚合；cur 存 "__all_inbox__" 占位）
    pub(crate) viewing_unified: bool,
    pub(crate) search_query: String,
    /// 搜索框文字（回车提交才触发搜索）
    pub(crate) search_text: String,
    /// M5 会话视图开关（文件夹视图生效；搜索结果恒平铺）
    pub(crate) threaded: bool,
    /// 触底加载：已请求下一页（防重复触发）
    pub(crate) load_more_pending: bool,
    /// 触底加载：已到最后一页（不再触发）
    pub(crate) load_more_done: bool,
    /// 列表快速过滤（文件夹视图；作用于显示模型）
    pub(crate) list_filter: ListFilter,
    /// 列表排序方向（true = 升序；文件夹视图）
    pub(crate) list_asc: bool,
    pub(crate) list_offset: f32,
    /// 列表可视高度（初始估计；首个滚动事件后用 viewport 实测校正）
    pub(crate) list_view_h: f32,
    /// 当前文件夹首次列表在途（选中后到 FolderLoaded 前）——空列表时区分
    /// 「正在同步」与「真的没有邮件」，不再让未读徽标和空列表互相打脸
    pub(crate) folder_loading: bool,
}

/// M7 批量操作（选择集与确认面板）
pub(crate) struct BatchOps {
    /// 批量选择（账号, hash）：全账号唯一键，搜索结果跨账号也可批量
    pub(crate) selection: std::collections::HashSet<(usize, i64)>,
    /// 批量移动目标选择面板展开中
    pub(crate) move_open: bool,
    pub(crate) pending_delete: bool,
    /// 读信区「移动到」面板展开中：（账号, hash）
    pub(crate) move_picker: Option<(usize, i64)>,
}

/// 侧栏文件夹内联编辑（新建/改名/删除确认）
pub(crate) struct FolderEdit {
    /// 新建输入中：（账号, 名字）
    pub(crate) new_folder: Option<(usize, String)>,
    /// 改名输入中：（账号, 旧路径, 新路径）
    pub(crate) rename_folder: Option<(usize, String, String)>,
    pub(crate) pending_delete: Option<(usize, String)>,
}

pub(crate) struct State {
    pub(crate) prefs: Prefs,
    pub(crate) nav: Nav,
    #[allow(dead_code)]
    pub(crate) session: Option<Session>,
    pub(crate) cmd_tx: Option<Sender<SessionCmd>>,
    /// 每账号正文直通通道（下标=账号索引）：打开邮件直接发给 Reader 线程，
    /// 不经主循环命令队列（主循环堵在长同步里时正文照样秒出）
    pub(crate) read_txs: Vec<Sender<i64>>,
    pub(crate) events: Option<EventTap>,
    pub(crate) account_names: Vec<String>,
    pub(crate) error: Option<String>,
    pub(crate) setup_hint: Option<String>,
    /// 统一收件箱按账号聚合
    pub(crate) unified_contrib: HashMap<usize, Vec<MessageRow>>,
    /// 搜索结果按账号聚合
    pub(crate) search_contrib: HashMap<usize, Vec<MessageRow>>,
    /// 搜索正文片段（hash → snippet；渲染搜索结果行时显示）
    pub(crate) search_snippets: HashMap<i64, String>,
    pub(crate) rows: Vec<RowEntry>,
    /// 列表显示模型（rows/threaded/expanded 变化时轻 rebuild_display 重建；
    /// view 每帧直接读，不再全量克隆 500 行 + 重跑会话聚合）
    pub(crate) mail_display: Vec<MailDisp>,
    pub(crate) drafts: Vec<(usize, DraftRow)>,
    pub(crate) selected: Option<(usize, i64)>,
    pub(crate) reading: Option<(usize, Reading)>,
    /// 在途正文请求：(账号, hash)。发送 OpenMessage 时记录，
    /// BodyLoaded 只有匹配时才应用——切走列表/删信后迟到的正文不覆写阅读器
    pub(crate) body_req: Option<(usize, i64)>,
    /// 删除确认中：(账号, hash)
    pub(crate) pending_delete: Option<(usize, i64)>,
    /// 展开的会话（key = 根邮件 hash）
    pub(crate) expanded: HashMap<i64, bool>,
    pub(crate) screen: Screen,
    /// 每账号最近状态（顶栏展示）
    pub(crate) statuses: HashMap<usize, String>,
    /// M6：完整账号配置（添加/删除后写回并热重载会话）
    pub(crate) accounts: Vec<smail_core::AccountConfig>,
    pub(crate) cfg_path: PathBuf,
    pub(crate) data_root: PathBuf,
    /// M6：OAuth 设备流横幅（url, code）；下一条 Status/Error 事件自动清除
    pub(crate) oauth_prompt: Option<(String, String)>,
    /// M7 批量操作（选择集与确认面板）
    pub(crate) batch: BatchOps,
    /// 侧栏文件夹内联编辑（新建/改名/删除确认）
    pub(crate) folder_edit: FolderEdit,
    /// 最近联系人（Cmd::ListContacts 回填，进入 compose 时拉取）
    pub(crate) contacts: Vec<String>,
    /// 发件箱行（Cmd::ListOutbox 回填；按账号聚合）
    pub(crate) outbox_rows: Vec<(usize, smail_store::OutboxRow)>,
    /// 发件箱计数（pending, failed)：侧栏入口显隐
    pub(crate) outbox_counts: HashMap<usize, (usize, usize)>,
    /// 每账号连接状态（断连时顶栏持续黄条提示，重连成功自动消除）
    pub(crate) conn_ok: HashMap<usize, bool>,
    /// 清空废纸篓确认条
    pub(crate) pending_empty_trash: bool,
    /// 通讯录全量（管理界面；按账号聚合）
    pub(crate) contacts_full: Vec<(usize, Vec<smail_store::ContactRow>)>,
    /// B4：账号 PGP 内联编辑中：（账号索引, 表单）
    pub(crate) pgp_edit: Option<(usize, setup::PgpForm)>,
    /// 签名内联编辑中：（账号索引, 编辑器内容）
    pub(crate) signature_edit: Option<(usize, text_editor::Content)>,
    /// 同步间隔内联编辑中：（账号索引, 输入框内容，秒）
    pub(crate) sync_edit: Option<(usize, String)>,
    /// 本地索引只读连接（按账号懒打开）：切文件夹时 UI 直读缓存行，
    /// 不等服务线程——它可能正堵在长同步（重连全量拉取/IDLE 变更）上，
    /// 命令队列几十秒不被消费，等回包=「一同步就看不见邮件」
    pub(crate) local_stores: HashMap<usize, smail_store::Store>,
    /// 导出目标：保存对话框期间用户可能切换邮件，回调不能再用 selected
    /// （文件名是请求时的主题，导出内容也必须是请求时的邮件）
    pub(crate) export_target: Option<(usize, i64)>,
    /// AI 助手状态（配置/在途/摘要缓存/整理预览）
    pub(crate) ai: crate::ai::AiUiState,
}

/// 列表快速过滤（会话视图按会话级谓词：任一成员命中则显示整会话）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListFilter {
    All,
    Unread,
    Flagged,
}

impl State {
    /// 当前语言字符串表
    pub(crate) fn l(&self) -> &'static crate::lang::Lex {
        match self.prefs.lang {
            crate::lang::Lang::Zh => &crate::lang::ZH,
            crate::lang::Lang::En => &crate::lang::EN,
        }
    }

    pub(crate) fn boot() -> (Self, Task<Message>) {
        let (cfg_path, data_root) = paths();
        let (boot_theme, boot_follow, boot_lang, boot_ai_ack, boot_win) = boot_prefs(&data_root);
        let boot_last_view = crate::lang::load_prefs(&data_root).last_view;
        match Session::start(&cfg_path, &data_root) {
            Ok((session, rx)) => {
                let accounts = smail_core::load_accounts(&cfg_path).unwrap_or_default();
                let names = accounts.iter().map(|x| x.name.clone()).collect::<Vec<_>>();
                let cfg_path2 = cfg_path.clone();
                let data_root2 = data_root.clone();
                let accounts2 = accounts.clone();
                let state = State {
                    prefs: Prefs {
                        ui_theme: boot_theme,
                        theme_follow: boot_follow,
                        lang: boot_lang,
                        win_geom: None,
                        win_save_at: None,
                        last_view: None,
                    },
                    accounts: accounts2,
                    cfg_path: cfg_path2,
                    data_root: data_root2,
                    oauth_prompt: None,
                    session: Some(session),
                    cmd_tx: None,
                    read_txs: vec![],
                    events: Some(EventTap::new(rx)),
                    account_names: names,
                    error: None,
                    setup_hint: None,
                    nav: Nav {
                        folders: vec![],
                        cur: None,
                        in_search: false,
                        viewing_unified: false,
                        search_query: String::new(),
                        search_text: String::new(),
                        threaded: true,
                        load_more_pending: false,
                        load_more_done: false,
                        list_filter: ListFilter::All,
                        list_asc: false,
                        list_offset: 0.0,
                        list_view_h: list::DEFAULT_VIEW_H,
                        folder_loading: false,
                    },
                    unified_contrib: HashMap::new(),
                    search_contrib: HashMap::new(),
                    search_snippets: HashMap::new(),
                    rows: vec![],
                    mail_display: vec![],
                    drafts: vec![],
                    selected: None,
                    reading: None,
                    body_req: None,
                    pending_delete: None,
                    expanded: HashMap::new(),
                    screen: Screen::Mail,
                    statuses: HashMap::new(),
                    batch: BatchOps {
                        selection: Default::default(),
                        move_open: false,
                        pending_delete: false,
                        move_picker: None,
                    },
                    folder_edit: FolderEdit {
                        new_folder: None,
                        rename_folder: None,
                        pending_delete: None,
                    },
                    contacts: vec![],
                    outbox_rows: vec![],
                    outbox_counts: HashMap::new(),
                    conn_ok: HashMap::new(),
                    pending_empty_trash: false,
                    contacts_full: vec![],
                    pgp_edit: None,
                    signature_edit: None,
                    sync_edit: None,
                    local_stores: HashMap::new(),
                    export_target: None,
                    ai: crate::ai::AiUiState::new(
                        smail_ai::load_config(&cfg_path.with_file_name("ai.toml")),
                        &data_root,
                    ),
                };
                let mut state = state;
                state.prefs.win_geom = boot_win;
                state.ai.ack = boot_ai_ack;
                state.cmd_tx = state.session.as_ref().map(|s| s.cmd_tx.clone());
                state.read_txs = state
                    .session
                    .as_ref()
                    .map(|s| s.read_txs.clone())
                    .unwrap_or_default();
                // 初始语言下推（服务线程默认 zh；英文界面重启后保持英文状态文案）
                if let Some(tx) = &state.cmd_tx {
                    let _ = tx.send(SessionCmd::SetLang(matches!(
                        state.prefs.lang,
                        crate::lang::Lang::Zh
                    )));
                }
                // 恢复上次视图（离线优先：本地文件夹广播先行，启动直达缓存邮件，
                // 不必等服务器连接成功）
                state.prefs.last_view = boot_last_view.clone();
                let restore = match boot_last_view {
                    Some((_, f)) if f == "__all_inbox__" && state.account_names.len() > 1 => {
                        crate::update(&mut state, Message::UnifiedInboxSelected)
                    }
                    Some((name, f)) => match state.account_names.iter().position(|n| n == &name) {
                        Some(i) => crate::update(&mut state, Message::FolderSelected(i, f)),
                        None => iced::Task::none(),
                    },
                    // 首次使用/无历史视图：直接打开收件箱（以前首屏无任何选中，
                    // 空列表像出了故障）
                    None => {
                        if state.account_names.is_empty() {
                            iced::Task::none()
                        } else {
                            crate::update(
                                &mut state,
                                Message::FolderSelected(0, "INBOX".into()),
                            )
                        }
                    }
                };
                // 启动即对齐系统深浅色
                (
                    state,
                    restore.chain(iced::system::theme().map(Message::SystemTheme)),
                )
            }
            Err(e) => {
                let mut setup = SetupForm::new();
                // 构造用户可见的错误信息
                let error_msg = if cfg_path.exists() {
                    format!(
                        "现有配置文件解析失败：{e}\n保存新账号会覆盖它（建议先备份 {}）",
                        cfg_path.display()
                    )
                } else {
                    format!("启动失败：{e}")
                };
                setup.err = Some(error_msg.clone());

                let state = State {
                    prefs: Prefs {
                        ui_theme: boot_theme,
                        theme_follow: boot_follow,
                        lang: boot_lang,
                        win_geom: None,
                        win_save_at: None,
                        last_view: None,
                    },
                    accounts: vec![],
                    cfg_path: cfg_path.clone(),
                    data_root: data_root.clone(),
                    oauth_prompt: None,
                    session: None,
                    cmd_tx: None,
                    read_txs: vec![],
                    events: None,
                    account_names: vec![],
                    error: Some(error_msg), // ← 关键修复：同步到全局错误横幅
                    setup_hint: None,
                    nav: Nav {
                        folders: vec![],
                        cur: None,
                        in_search: false,
                        viewing_unified: false,
                        search_query: String::new(),
                        search_text: String::new(),
                        threaded: true,
                        load_more_pending: false,
                        load_more_done: false,
                        list_filter: ListFilter::All,
                        list_asc: false,
                        list_offset: 0.0,
                        list_view_h: list::DEFAULT_VIEW_H,
                        folder_loading: false,
                    },
                    unified_contrib: HashMap::new(),
                    search_contrib: HashMap::new(),
                    search_snippets: HashMap::new(),
                    rows: vec![],
                    mail_display: vec![],
                    drafts: vec![],
                    selected: None,
                    reading: None,
                    body_req: None,
                    pending_delete: None,
                    expanded: HashMap::new(),
                    screen: Screen::Setup(setup),
                    statuses: HashMap::new(),
                    batch: BatchOps {
                        selection: Default::default(),
                        move_open: false,
                        pending_delete: false,
                        move_picker: None,
                    },
                    folder_edit: FolderEdit {
                        new_folder: None,
                        rename_folder: None,
                        pending_delete: None,
                    },
                    contacts: vec![],
                    outbox_rows: vec![],
                    outbox_counts: HashMap::new(),
                    conn_ok: HashMap::new(),
                    pending_empty_trash: false,
                    contacts_full: vec![],
                    pgp_edit: None,
                    signature_edit: None,
                    sync_edit: None,
                    local_stores: HashMap::new(),
                    export_target: None,
                    ai: crate::ai::AiUiState::new(
                        smail_ai::load_config(&cfg_path.with_file_name("ai.toml")),
                        &data_root,
                    ),
                };
                let mut state = state;
                state.prefs.win_geom = boot_win;
                state.ai.ack = boot_ai_ack;
                (state, Task::none())
            }
        }
    }

    /// 当前 UI 偏好快照（写 ui.json 用——单一来源，字段增加不再散落调用点）
    pub(crate) fn current_prefs(&self) -> crate::lang::UiPrefs {
        crate::lang::UiPrefs {
            lang: self.prefs.lang,
            dark: if self.prefs.theme_follow {
                None
            } else {
                Some(matches!(self.prefs.ui_theme, iced::Theme::Dark))
            },
            ai_ack: self.ai.ack,
            win: self.prefs.win_geom,
            last_view: self.prefs.last_view.clone(),
        }
    }

    /// 本地索引缓存行（只读连接直读；无缓存/库未建返回空）
    pub(crate) fn local_folder_rows(&mut self, acct: usize, folder: &str) -> Vec<RowEntry> {
        let Some(cfg) = self.accounts.get(acct) else {
            return vec![];
        };
        if !self.local_stores.contains_key(&acct) {
            let db = self.data_root.join(&cfg.slug).join("index.db");
            if let Ok(s) = smail_store::Store::open_readonly(&db) {
                self.local_stores.insert(acct, s);
            }
        }
        let limit = smail_core::UI_LIST_LIMIT;
        self.local_stores
            .get(&acct)
            .and_then(|s| s.list_folder(folder, limit).ok())
            .map(|rows| rows.into_iter().map(|r| (acct, r)).collect())
            .unwrap_or_default()
    }

    /// 当前文件夹是否废纸篓（删除确认文案/语义提示用）
    pub(crate) fn cur_is_trash_folder(&self) -> bool {
        self.nav
            .cur
            .as_ref()
            .and_then(|(a, f)| {
                self.nav
                    .folders
                    .iter()
                    .find(|(fa, fi)| fa == a && &fi.path == f)
            })
            .map(|(_, fi)| fi.kind == smail_core::FolderKind::Trash)
            .unwrap_or(false)
    }

    pub(crate) fn send(&self, account: usize, cmd: Cmd) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(SessionCmd::Route { account, cmd });
        }
    }

    /// 正文请求直通：绕过主循环命令队列（长同步期间队列几十秒不被消费，
    /// 正文却常已在本地缓存）。仍配合 Cmd::OpenMessage 走已读上推，
    /// 迟到的第二次正文回包由 body_req 守卫丢弃
    pub(crate) fn request_body(&self, account: usize, hash: i64) {
        if let Some(tx) = self.read_txs.get(account) {
            let _ = tx.send(hash);
        }
    }

    pub(crate) fn merged_search_rows(&self) -> Vec<RowEntry> {
        let mut all: Vec<RowEntry> = self
            .search_contrib
            .iter()
            .flat_map(|(acct, rows)| rows.iter().map(move |r| (*acct, r.clone())))
            .collect();
        all.sort_by_key(|a| std::cmp::Reverse(a.1.date_unix));
        all.into_iter().take(200).collect()
    }

    /// 重建列表显示模型（rows/threaded/expanded/search 变化处必调，view 只读缓存）
    pub(crate) fn rebuild_display(&mut self) {
        self.mail_display = build_mail_display(self);
    }
}

pub(crate) fn persist_and_reload(
    state: &mut State,
    accounts: Vec<smail_core::AccountConfig>,
    // 随旧 Session 一并清理的数据目录（删账号时传；在后台线程 join 完成后删除）
    cleanup_dir: Option<std::path::PathBuf>,
) -> bool {
    // 账号删到 1 个后统一收件箱入口消失——强制退出该视图防困死
    if accounts.len() < 2 && state.nav.viewing_unified {
        state.nav.viewing_unified = false;
        state.nav.cur = None;
    }
    // 账号增删后索引位移：所有按账号索引 key 的缓存与指向旧账号位的
    // 瞬态全部作发（含空账号集分支——它提前 return，不能只放在后面）
    state.batch.selection.clear();
    state.expanded.clear();
    state.batch.move_picker = None;
    state.batch.move_open = false;
    state.batch.pending_delete = false;
    state.pending_delete = None;
    state.export_target = None;
    state.body_req = None;
    state.sync_edit = None;
    // 账号集变化：只读连接指向旧 slug 的库，全部作废重开
    state.local_stores.clear();
    state.unified_contrib.clear(); // 死键残留会让已删账号的邮件混入聚合
    state.search_contrib.clear();
    state.contacts_full.clear();
    state.contacts.clear();
    state.drafts.clear();
    state.outbox_rows.clear();
    state.outbox_counts.clear();
    state.conn_ok.clear();
    state.nav.list_offset = 0.0;
    state.nav.load_more_pending = false;
    state.nav.load_more_done = false;
    // 当前视图指向的账号索引已位移（删账号后指向别人）：作废，
    // 用户重新选文件夹（否则列表头显示 "? / INBOX"、行指向错误账号）
    state.nav.cur = None;
    state.nav.viewing_unified = false;
    state.nav.in_search = false;
    state.nav.search_text.clear();
    state.search_snippets.clear();
    // 空账号集：Session 起不来是预期，落盘后直接进无会话空态（不报错）
    if accounts.is_empty() {
        return match smail_core::save_accounts(&state.cfg_path, &accounts) {
            Ok(()) => {
                state.session = None;
                state.cmd_tx = None;
                state.read_txs.clear();
                state.events = None;
                state.accounts = vec![];
                state.account_names = vec![];
                state.nav.folders.clear();
                state.rows.clear();
                state.rebuild_display();
                state.reading = None;
                state.selected = None;
                state.nav.in_search = false;
                state.statuses.clear();
                state.oauth_prompt = None;
                true
            }
            Err(e) => {
                state.error = Some(crate::lang::f(state.l().err_save_cfg, &[&e]));
                false
            }
        };
    }
    if let Err(e) = smail_core::save_accounts(&state.cfg_path, &accounts) {
        let msg = crate::lang::f(state.l().err_save_cfg, &[&e]);
        if let Screen::Setup(f) = &mut state.screen {
            f.err = Some(msg);
        } else {
            state.error = Some(msg);
        }
        return false;
    }
    // 确保数据根目录存在（首次保存）
    let _ = std::fs::create_dir_all(&state.data_root);
    // 后台释放旧 Session：Session::drop 会同步 join 全部服务线程（可能卡在
    // 慢网络操作上），绝不能在 UI 线程做。join 返回即数据目录无人持有，
    // cleanup_dir（删账号）在同一根线程里随后删除——顺序确定性保证，无定时赌运
    if let Some(old_session) = state.session.take() {
        std::thread::spawn(move || {
            drop(old_session);
            if let Some(dir) = cleanup_dir {
                if let Err(e) = std::fs::remove_dir_all(&dir) {
                    eprintln!(
                        "[s-mail] 删除账号数据目录失败（可手动删 {}: {}",
                        dir.display(),
                        e
                    );
                }
            }
        });
    }
    state.cmd_tx = None;
    state.events = None;
    match smail_core::Session::start(&state.cfg_path, &state.data_root) {
        Ok((session, rx)) => {
            state.session = Some(session);
            state.cmd_tx = state.session.as_ref().map(|s| s.cmd_tx.clone());
            state.read_txs = state
                .session
                .as_ref()
                .map(|s| s.read_txs.clone())
                .unwrap_or_default();
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::SetLang(matches!(
                    state.prefs.lang,
                    crate::lang::Lang::Zh
                )));
            }
            state.events = Some(EventTap::new(rx));
            state.accounts = accounts;
            state.account_names = state.accounts.iter().map(|a| a.name.clone()).collect();
            state.nav.folders.clear();
            state.rows.clear();
            state.rebuild_display();
            state.drafts.clear();
            state.outbox_rows.clear();
            state.outbox_counts.clear();
            state.reading = None;
            state.selected = None;
            state.nav.in_search = false;
            state.nav.search_text.clear();
            state.statuses.clear();
            state.oauth_prompt = None;
            state.error = None;
            // 账号增删后当前视图已作废（上面 cur=None）：自动打开第一个账号
            // 的收件箱，不让用户停在无选中的空屏
            if !state.account_names.is_empty() {
                state.nav.cur = Some((0, "INBOX".into()));
                state.nav.folder_loading = true;
                state.send(0, Cmd::SelectFolder("INBOX".into()));
            }
            true
        }
        Err(e) => {
            // 落盘已成功：账号列表以文件为准（否则界面与磁盘漂移）
            state.session = None;
            state.cmd_tx = None;
            state.read_txs.clear();
            state.events = None;
            state.account_names = accounts.iter().map(|a| a.name.clone()).collect();
            state.local_stores.clear();
            let msg = crate::lang::f(state.l().err_reload, &[&e]);
            if let Screen::Setup(f) = &mut state.screen {
                f.err = Some(msg);
            } else {
                state.error = Some(msg);
            }
            false
        }
    }
}

/// SMAIL_THEME=dark 可预设深色（也方便无头冒烟两种主题）
pub(crate) fn ui_theme_from_env() -> Theme {
    match std::env::var("SMAIL_THEME").as_deref() {
        Ok("dark") => Theme::Dark,
        _ => Theme::Light,
    }
}

/// 启动主题/语言：环境变量 > ui.json 手动选择 > 跟随系统。
/// （选过主题后不再跟随系统——手动选择优先，与顶栏切换语义一致）
pub(crate) fn boot_prefs(
    data_root: &std::path::Path,
) -> (Theme, bool, crate::lang::Lang, bool, Option<[f32; 4]>) {
    let prefs = crate::lang::load_prefs(data_root);
    let env_set = std::env::var_os("SMAIL_THEME").is_some();
    let theme = if env_set {
        ui_theme_from_env()
    } else {
        match prefs.dark {
            Some(true) => Theme::Dark,
            Some(false) => Theme::Light,
            None => ui_theme_from_env(),
        }
    };
    (
        theme,
        !env_set && prefs.dark.is_none(),
        prefs.lang,
        prefs.ai_ack,
        prefs.win,
    )
}

pub(crate) fn paths() -> (PathBuf, PathBuf) {
    let cfg = std::env::var_os("SMAIL_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("config.toml"));
    let data = std::env::var_os("SMAIL_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(home);
    (cfg, data)
}

pub(crate) fn home() -> PathBuf {
    let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    p.push(".s-mail");
    p
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------
