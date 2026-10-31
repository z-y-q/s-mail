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

#[allow(clippy::large_enum_variant)] // Setup 表单一次性传递（盒装无收益）
pub(crate) enum Screen {
    Mail,
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
    /// 静音的发件人地址（新邮件通知跳过；ui.json 持久化）
    pub(crate) muted: Vec<String>,
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
    /// 列表排序维度（文件夹视图；平铺模式生效）
    pub(crate) sort_key: SortKey,
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
    /// 读信区「代转」收件人输入中：（账号, hash）
    pub(crate) redirect_prompt: Option<(usize, i64)>,
    /// 代转收件人输入框内容（随 prompt 开关清空）
    pub(crate) redirect_to: String,
    /// 代转挂起：Reader 正在取正文（BodyLoaded 到达后以 RedirectNow 续发）
    pub(crate) redirect_pending: Option<(usize, i64, String)>,
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
    /// 写信独立窗口的写作状态（None=无写信窗；多窗口：主窗保持邮件列表可见）
    pub(crate) compose: Option<Compose>,
    /// 写信窗口 id（window::open 回填；关闭与标题判定用）
    pub(crate) compose_window: Option<iced::window::Id>,
    /// 主窗 id（boot 时 window::open 回填；托盘激活与关闭请求处理用）
    pub(crate) main_window: Option<iced::window::Id>,
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
    /// 规则编辑面板：（账号索引, 草稿行列表）。草稿直接持有 Rule 结构
    ///（all_of[1] + actions[1] 的常用形态可完整编辑；多条件/多动作的
    /// 高级规则只读摘要 + 删除，不提供破坏性编辑）
    pub(crate) rule_edit: Option<(usize, Vec<crate::setup::RuleDraft>)>,
    /// 三栏列宽（拖拽分割条调整，prefs 记忆）
    pub(crate) sidebar_w: f32,
    pub(crate) list_w: f32,
    /// 拖拽中的分割条 + 起始光标 x（首个 CursorMoved 填充）+ 起始宽度
    pub(crate) split_drag: Option<(SplitKind, Option<f32>, f32)>,
    /// 拖拽中的邮件（按下即候选，超阈值成真拖；松开按悬停目标投递）
    pub(crate) drag: Option<crate::drag::MailDrag>,
    /// 侧栏文件夹命中表（投放目标；每帧绘制回填）
    pub(crate) drag_targets: crate::drag::TargetRects,
    /// 右键菜单打开中：（账号, hash）；菜单行渲染在该邮件行后
    pub(crate) context_menu: Option<(usize, i64)>,
    /// 通讯录搜索过滤词（设置页，前端过滤）
    pub(crate) contact_search: String,
    /// 文件夹导出进行中：（账号, 文件夹, 目录）——picked 回填后后台写出
    pub(crate) export_folder: Option<(usize, String, std::path::PathBuf)>,
    /// 邮件提醒（到期发系统通知；内存态，重启丢失）
    pub(crate) reminders: Vec<MailReminder>,
    /// 本地索引只读连接（按账号懒打开）：切文件夹时 UI 直读缓存行，
    /// 不等服务线程——它可能正堵在长同步（重连全量拉取/IDLE 变更）上，
    /// 命令队列几十秒不被消费，等回包=「一同步就看不见邮件」
    pub(crate) local_stores: HashMap<usize, smail_store::Store>,
    /// 导出目标：保存对话框期间用户可能切换邮件，回调不能再用 selected
    /// （文件名是请求时的主题，导出内容也必须是请求时的邮件）
    pub(crate) export_target: Option<(usize, i64)>,
    /// AI 助手状态（配置/在途/摘要缓存/整理预览）
    pub(crate) ai: crate::ai::AiUiState,
    /// HTML 原样渲染（阅读窗正文区换挂系统网页视图；细节见 webview.rs）
    pub(crate) html: HtmlView,
    /// 网页视图本体（!Send，update 主线程持有；None=未创建/已关闭）
    #[cfg(target_os = "macos")]
    pub(crate) webview: Option<wry::WebView>,
}

/// 原样渲染会话状态（`on`/`source`/`pane` 跨平台读；其余字段只在 macOS
/// 网页视图路径读写，一并门禁——非 macOS 构建里它们无人引用，CI 会拦）
pub(crate) struct HtmlView {
    /// 开关（阅读 HTML 邮件时显示；切换非 HTML 邮件自动失效回 markdown）
    pub(crate) on: bool,
    /// 当前信的消毒 HTML（Extract 任务回填；None=未就绪/非 HTML）
    pub(crate) source: Option<String>,
    /// 阅读窗正文区几何（探针每帧回填，取最后一个登记）
    pub(crate) pane: crate::drag::PaneRects,
    /// 远程图片许可（默认禁——与 markdown 视图的「显示图片」同一隐私语义）
    #[cfg(target_os = "macos")]
    pub(crate) load_remote: bool,
    /// 已同步给网页视图的几何（避免每帧重复 set_bounds）
    #[cfg(target_os = "macos")]
    pub(crate) synced: Option<[f32; 4]>,
    /// 原生窗口句柄已捕获（window::run 异步回填，创建子视图的前置）
    #[cfg(target_os = "macos")]
    pub(crate) handles_ready: bool,
    /// 句柄未就绪时的挂起打开（HtmlHandlesReady 到场后补创建）
    #[cfg(target_os = "macos")]
    pub(crate) pending_open: bool,
    /// 主窗内容视图指针（window::run 回调期提取；0/None=未捕获）
    #[cfg(target_os = "macos")]
    pub(crate) raw_ns_view: Option<u64>,
    /// 子视图当前显隐（缓存上次设定值，重复 setHidden 免掉）
    #[cfg(target_os = "macos")]
    pub(crate) shown: Option<bool>,
}

impl HtmlView {
    pub(crate) fn new() -> Self {
        Self {
            on: false,
            source: None,
            pane: crate::drag::new_pane_rects(),
            #[cfg(target_os = "macos")]
            load_remote: false,
            #[cfg(target_os = "macos")]
            synced: None,
            #[cfg(target_os = "macos")]
            handles_ready: false,
            #[cfg(target_os = "macos")]
            pending_open: false,
            #[cfg(target_os = "macos")]
            raw_ns_view: None,
            #[cfg(target_os = "macos")]
            shown: None,
        }
    }
}

/// 列表快速过滤（会话视图按会话级谓词：任一成员命中则显示整会话）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListFilter {
    All,
    Unread,
    Flagged,
}

/// 平铺列表排序维度（Mail.app「排序方式」对齐）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortKey {
    Date,
    Sender,
    Subject,
}

impl SortKey {
    pub(crate) fn next(&self) -> Self {
        match self {
            SortKey::Date => SortKey::Sender,
            SortKey::Sender => SortKey::Subject,
            SortKey::Subject => SortKey::Date,
        }
    }
    pub(crate) fn label(&self, zh: bool) -> &'static str {
        match (self, zh) {
            (SortKey::Date, true) => "按日期",
            (SortKey::Sender, true) => "按发件人",
            (SortKey::Subject, true) => "按主题",
            (SortKey::Date, false) => "By date",
            (SortKey::Sender, false) => "By sender",
            (SortKey::Subject, false) => "By subject",
        }
    }
}

/// 可拖拽的分割条：侧栏↔列表、列表↔阅读
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitKind {
    Sidebar,
    List,
}

/// 邮件提醒（右键「提醒我」设置；到期发系统通知，点击跳转该邮件）
#[derive(Debug, Clone)]
pub(crate) struct MailReminder {
    pub(crate) due: std::time::Instant,
    pub(crate) click: crate::notify::NotifyClick,
    pub(crate) subject: String,
}

impl SplitKind {
    pub(crate) fn clamp_width(&self, w: f32) -> f32 {
        match self {
            SplitKind::Sidebar => w.clamp(150.0, 320.0),
            SplitKind::List => w.clamp(300.0, 560.0),
        }
    }
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
        let (boot_theme, boot_follow, boot_lang, boot_ai_ack, boot_win, boot_muted) =
            boot_prefs(&data_root);
        let boot_last_view = crate::lang::load_prefs(&data_root).last_view;
        let boot_cols = crate::lang::load_prefs(&data_root).columns;
        // 主窗几何（daemon 模式：boot 任务里显式 open 主窗）
        let (win_size, win_pos) = match crate::lang::load_prefs(&data_root).win {
            Some([w, h, x, y]) if w >= 980.0 && h >= 560.0 => (
                iced::Size::new(w, h),
                iced::window::Position::Specific(iced::Point::new(x, y)),
            ),
            _ => (
                iced::Size::new(1280.0, 760.0),
                iced::window::Position::Default,
            ),
        };
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
                        muted: boot_muted,
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
                        sort_key: crate::SortKey::Date,
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
                    compose: None,
                    compose_window: None,
                    main_window: None,
                    statuses: HashMap::new(),
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
                    outbox_counts: HashMap::new(),
                    conn_ok: HashMap::new(),
                    pending_empty_trash: false,
                    contacts_full: vec![],
                    pgp_edit: None,
                    signature_edit: None,
                    sync_edit: None,
                    rule_edit: None,
                    sidebar_w: boot_cols.as_ref().map(|c| c.0).unwrap_or(200.0),
                    list_w: boot_cols.as_ref().map(|c| c.1).unwrap_or(380.0),
                    split_drag: None,
                    drag: None,
                    drag_targets: crate::drag::new_target_rects(),
                    context_menu: None,
                    contact_search: String::new(),
                    export_folder: None,
                    reminders: vec![],
                    local_stores: HashMap::new(),
                    export_target: None,
                    ai: crate::ai::AiUiState::new(
                        smail_ai::load_config(&cfg_path.with_file_name("ai.toml")),
                        &data_root,
                    ),
                    html: HtmlView::new(),
                    #[cfg(target_os = "macos")]
                    webview: None,
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
                            crate::update(&mut state, Message::FolderSelected(0, "INBOX".into()))
                        }
                    }
                };
                // 启动即对齐系统深浅色 + 打开主窗（daemon 多窗口模式）
                let (_main_id, open_task) = iced::window::open(iced::window::Settings {
                    size: win_size,
                    position: win_pos,
                    min_size: Some(iced::Size::new(980.0, 560.0)),
                    // 关闭事件交还应用层：红叉清理（webview/句柄）必须经我们
                    exit_on_close_request: false,
                    ..Default::default()
                });
                let open_main = open_task.map(crate::Message::MainWindowOpened);
                (
                    state,
                    restore
                        .chain(open_main)
                        .chain(iced::system::theme().map(crate::Message::SystemTheme)),
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
                        muted: vec![],
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
                        sort_key: crate::SortKey::Date,
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
                    compose: None,
                    compose_window: None,
                    main_window: None,
                    statuses: HashMap::new(),
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
                    outbox_counts: HashMap::new(),
                    conn_ok: HashMap::new(),
                    pending_empty_trash: false,
                    contacts_full: vec![],
                    pgp_edit: None,
                    signature_edit: None,
                    sync_edit: None,
                    rule_edit: None,
                    sidebar_w: 200.0,
                    list_w: 380.0,
                    split_drag: None,
                    drag: None,
                    drag_targets: crate::drag::new_target_rects(),
                    context_menu: None,
                    contact_search: String::new(),
                    export_folder: None,
                    reminders: vec![],
                    local_stores: HashMap::new(),
                    export_target: None,
                    ai: crate::ai::AiUiState::new(
                        smail_ai::load_config(&cfg_path.with_file_name("ai.toml")),
                        &data_root,
                    ),
                    html: HtmlView::new(),
                    #[cfg(target_os = "macos")]
                    webview: None,
                };
                let mut state = state;
                state.prefs.win_geom = boot_win;
                state.ai.ack = boot_ai_ack;
                // 无配置引导屏也要有主窗（daemon 模式窗口由 boot 显式打开）
                let (_main_id, open_task) = iced::window::open(iced::window::Settings {
                    size: win_size,
                    position: win_pos,
                    min_size: Some(iced::Size::new(980.0, 560.0)),
                    // 关闭事件交还应用层：红叉清理（webview/句柄）必须经我们
                    exit_on_close_request: false,
                    ..Default::default()
                });
                let open_main = open_task.map(crate::Message::MainWindowOpened);
                (state, open_main)
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
            columns: Some((self.sidebar_w, self.list_w)),
            muted: self.prefs.muted.clone(),
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

    /// 智能邮箱聚合：跨账号跨文件夹按谓词收集行（本地索引直读，离线可用）。
    /// key：`__smart_unread__`（全部未读）/ `__smart_flagged__`（全部星标）；
    /// 结果按时间降序
    pub(crate) fn smart_fill(&mut self, key: &str) {
        let folders: Vec<(usize, String)> = self
            .nav
            .folders
            .iter()
            .map(|(a, f)| (*a, f.path.clone()))
            .collect();
        let mut all: Vec<RowEntry> = vec![];
        for (a, fp) in &folders {
            for (acct, r) in self.local_folder_rows(*a, fp) {
                let hit = match key {
                    "__smart_unread__" => !r.seen,
                    "__smart_flagged__" => r.flagged,
                    _ => false,
                };
                if hit {
                    all.push((acct, r));
                }
            }
        }
        all.sort_by_key(|x| std::cmp::Reverse(x.1.date_unix));
        self.rows = all;
    }

    /// 当前是否处于智能邮箱虚拟视图
    pub(crate) fn cur_is_smart(&self) -> bool {
        self.nav
            .cur
            .as_ref()
            .map(|(_, f)| f.starts_with("__smart_"))
            .unwrap_or(false)
    }

    /// 当前文件夹是否废纸篓（删除确认文案/语义提示用）
    pub(crate) fn cur_is_trash_folder(&self) -> bool {
        self.cur_folder_kind() == Some(smail_core::FolderKind::Trash)
    }

    pub(crate) fn cur_is_junk_folder(&self) -> bool {
        self.cur_folder_kind() == Some(smail_core::FolderKind::Junk)
    }

    /// 当前文件夹的 kind（本地直读路径无 FolderInfo 时按合成名判定）
    pub(crate) fn cur_folder_kind(&self) -> Option<smail_core::FolderKind> {
        let (acct, folder) = self.nav.cur.as_ref()?;
        if let Some((_, fi)) = self
            .nav
            .folders
            .iter()
            .find(|(fa, fi)| fa == acct && &fi.path == folder)
        {
            return Some(fi.kind);
        }
        // 本地直读/合成行：按名称识别（与 list_view 合成规则一致）
        Some(match folder.as_str() {
            "INBOX" => smail_core::FolderKind::Inbox,
            "Sent Messages" => smail_core::FolderKind::Sent,
            "Junk" => smail_core::FolderKind::Junk,
            "Archive" => smail_core::FolderKind::Archive,
            "Trash" => smail_core::FolderKind::Trash,
            _ => smail_core::FolderKind::Plain,
        })
    }

    /// 邮箱循环序列（上一个/下一个邮箱快捷键用）：与侧栏同序——
    /// 草稿（恒显）→ 收件箱 → 已发送 → 垃圾 → 归档 → 废纸篓 → 其他
    pub(crate) fn mailbox_cycle(&self, acct: usize) -> Vec<String> {
        let mut seq = vec!["__drafts__".to_string()];
        let mut server: Vec<smail_core::FolderInfo> = self
            .nav
            .folders
            .iter()
            .filter(|(a, _)| *a == acct)
            .map(|(_, f)| f.clone())
            .collect();
        let defaults: [(smail_core::FolderKind, &str); 5] = [
            (smail_core::FolderKind::Inbox, "INBOX"),
            (smail_core::FolderKind::Sent, "Sent Messages"),
            (smail_core::FolderKind::Junk, "Junk"),
            (smail_core::FolderKind::Archive, "Archive"),
            (smail_core::FolderKind::Trash, "Trash"),
        ];
        for (kind, path) in defaults {
            if !server.iter().any(|f| f.kind == kind) {
                server.push(smail_core::FolderInfo {
                    path: path.to_string(),
                    kind,
                    unseen: 0,
                    total: 0,
                });
            }
        }
        server.sort_by_key(|f| match f.kind {
            smail_core::FolderKind::Inbox => 0,
            smail_core::FolderKind::Drafts => 1,
            smail_core::FolderKind::Sent => 2,
            smail_core::FolderKind::Junk => 3,
            smail_core::FolderKind::Archive => 4,
            smail_core::FolderKind::Trash => 5,
            smail_core::FolderKind::Plain => 6,
        });
        for f in server {
            if f.kind != smail_core::FolderKind::Drafts {
                seq.push(f.path);
            }
        }
        seq
    }

    /// 会话视图全局展开：当前文件夹全部会话根置为展开
    pub(crate) fn expand_all_threads(&mut self) {
        let roots: Vec<i64> = self
            .mail_display
            .iter()
            .filter_map(|d| match &d.kind {
                crate::DispKind::ThreadRoot { .. } => Some(d.row.envelope_hash),
                _ => None,
            })
            .collect();
        for h in roots {
            self.expanded.insert(h, true);
        }
        self.rebuild_display();
    }

    /// 会话视图全局折叠
    pub(crate) fn collapse_all_threads(&mut self) {
        self.expanded.clear();
        self.rebuild_display();
    }

    pub(crate) fn send(&self, account: usize, cmd: Cmd) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(SessionCmd::Route { account, cmd });
        }
    }

    /// 多窗口标题：写信窗与主窗区分（daemon .title 每窗口调用）
    pub(crate) fn window_title(&self, window: iced::window::Id) -> String {
        if self.compose_window == Some(window) {
            match &self.compose {
                Some(c) if !c.subject.trim().is_empty() => {
                    format!("{} — 写邮件", c.subject.trim())
                }
                _ => "写邮件".to_string(),
            }
        } else {
            "s-mail".to_string()
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
    state.rule_edit = None;
    state.context_menu = None;
    state.contact_search.clear();
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
) -> (
    Theme,
    bool,
    crate::lang::Lang,
    bool,
    Option<[f32; 4]>,
    Vec<String>,
) {
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
        prefs.muted,
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
