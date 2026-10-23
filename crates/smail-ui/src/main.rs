//! s-mail M3-b 多账号 UI（iced 0.14）
//!
//! 配置：SMAIL_CONFIG 或 ~/.s-mail/config.toml（支持 [[accounts]] 多账号与旧版 [account]）
//! 数据：SMAIL_HOME 或 ~/.s-mail/<账号slug>/
//!
//! 多账号模型：每账号一个服务线程（Session 多路复用），事件带账号标签；
//! 侧栏按账号分组；搜索全账号 fan-out 合并；发件账号在 compose 头部切换。
//!
//! 模块划分：数据类型与 update/事件循环在本文件；视图与表单在
//! list / reader / compose / setup 模块，样式在 theme。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use iced::futures::SinkExt;
use iced::widget::{button, column, container, row, text, text_editor, Space};
use iced::{Element, Font, Length, Subscription, Task, Theme};

mod ai;
mod compose;
mod events;
mod keys;
mod lang;
mod list;
mod notify;
mod reader;
mod setup;
mod state;
mod theme;
mod tray;

use compose::{
    compose_view, open_compose_with, split_last_segment, text_editor_edit_paste, Compose, ReplyKind,
};
use list::{main_view, ROW_H};
use setup::{build_account, setup_view, Preset, SetupField, SetupForm};

use smail_core::{Cmd, Event, SessionCmd, TaggedEvent};

// 拆分模块的符号回灌（调用点零改动）
pub(crate) use events::handle_event;
pub(crate) use keys::{handle_key, scroll_list_to_top};
pub(crate) use state::*;

fn main() -> iced::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("auth") {
        // CLI：s-mail auth <账号名> —— 交互式设备流授权（终端显示 URL + 代码）
        return match cli_auth(&args) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("s-mail auth 失败：{e}");
                std::process::exit(1);
            }
        };
    }
    tray::spawn();
    // 窗口几何记忆（ui.json；无记录用默认）
    let (win_size, win_pos) = match crate::lang::load_prefs(&crate::paths().1).win {
        Some([w, h, x, y]) if w >= 980.0 && h >= 560.0 => (
            iced::Size::new(w, h),
            iced::window::Position::Specific(iced::Point::new(x, y)),
        ),
        _ => (
            iced::Size::new(1280.0, 760.0),
            iced::window::Position::Default,
        ),
    };
    iced::application(State::boot, update, view)
        .title("s-mail")
        .window(iced::window::Settings {
            size: win_size,
            position: win_pos,
            min_size: Some(iced::Size::new(980.0, 560.0)),
            ..Default::default()
        })
        .theme(|state: &State| state.ui_theme.clone())
        // macOS 系统字体（SF Pro Text；缺失时 iced 回退默认字体）
        .default_font(Font::with_name("SF Pro Text"))
        .subscription(subscription)
        .run()
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // 事件/解析结果大负载一次性传递（盒装无收益）
pub(crate) enum Message {
    Ev(TaggedEvent),
    FolderSelected(usize, String),
    RowSelected(usize, i64),
    /// 列表滚动：offset = 滚动位置；view_h = 可视区实际高度（虚拟化窗口校准用）
    ListScrolled(f32, f32),
    /// 统一收件箱：所有账号 INBOX 聚合（侧栏「所有收件箱」入口）
    UnifiedInboxSelected,
    /// 托盘动作（打开/退出）
    Tray(crate::tray::TrayAction),
    /// 快速过滤切换（全部/未读/星标；文件夹视图）
    SetListFilter(crate::ListFilter),
    /// 排序方向切换（新→旧 ⇄ 旧→新）
    ToggleListSort,
    ComposeNew,
    ComposeFromDraft(usize, i64),
    ComposeClose,
    ComposeAccountNext,
    /// 密送（进信封不进邮件头）
    ComposeField(Field, String),
    ComposeBodyEdit(iced::widget::text_editor::Action),
    /// 自动保存节拍（仅写作屏订阅；静默 20s 后落草稿，防崩溃/断电丢信）
    AutosaveTick,
    /// 通知点击跳转：(账号, 文件夹, hash)
    NotificationClicked(notify::NotifyClick),
    SendClicked,
    SaveDraftClicked,
    DraftRowClicked(usize, i64),
    /// 发件箱：重发 / 删除
    OutboxRetry(i64),
    /// 清空废纸篓（Trash 视图）：请求 / 确认 / 取消
    EmptyTrashRequest,
    EmptyTrashConfirm,
    EmptyTrashCancel,
    OutboxRemove(i64),
    MarkUnread(usize, i64),
    ToggleStar(usize, i64, bool),
    ToggleThreading,
    ToggleUiTheme,
    /// 中/英文切换（默认中文；存 ui.json）
    ToggleLang,
    /// 键盘导航（↑↓ 选信、Esc 层级退出）
    KeyPressed(iced::keyboard::Key),
    /// 系统深浅色（跟随模式）
    SystemTheme(iced::theme::Mode),
    /// 窗口几何（记忆尺寸/位置；节流落盘）
    WindowGeometry(iced::window::Event),
    /// 点会话根行：切换展开 + 打开最新一封（acct, root_hash, latest_hash）
    ThreadRootClicked(usize, i64, i64),
    AskDelete(usize, i64),
    CancelDelete,
    /// 回复（发件人 + 引用正文）
    Reply(usize, i64),
    /// 转发附件就绪（从 .eml 提取后回填 compose；staging 路径）
    ForwardAttachmentsReady(Vec<PathBuf>),
    /// 回复全部（发件人 + 所有收件人 + 引用）
    ReplyAll(usize, i64),
    /// 转发（引用正文，收件人留空）
    Forward(usize, i64),
    DeleteMail(usize, i64),
    SearchInput(String),
    SearchSubmit,
    /// 手动刷新当前文件夹（顶栏 ↻ / F5；绕过同步 TTL）
    ManualRefresh,
    SaveAttachment(usize, i64, String),
    AddAttachment,
    AttachmentsPicked(Option<Vec<PathBuf>>),
    RemoveAttachment(usize),
    LinkClicked(String),
    TogglePgpSign,
    TogglePgpEncrypt,
    // ---- M6 账号配置 ----
    SetupOpen,
    SetupClose,
    ComposeDiscardConfirm,
    ComposeDiscardCancel,
    SetupPreset(Preset),
    SetupField(SetupField, String),
    SetupTest,
    SetupTestResult(std::result::Result<usize, String>),
    /// 保存前自动连接验证的结果
    SetupSaveVerify(std::result::Result<usize, String>),
    SetupSave,
    SetupDelete(usize),
    OAuthOpenUrl,
    // ---- M7：移动 / 批量 / 文件夹管理 / 联系人补全 ----
    /// 打开/关闭读信区「移动到」面板
    MoveTo(usize, i64),
    MoveToDest(usize, i64, String),
    ToggleSelect(usize, i64),
    SelectAll,
    ClearSelection,
    BatchSeen(bool),
    BatchStar,
    BatchMoveOpen,
    BatchMoveDest(usize, String),
    AskBatchDelete,
    BatchDeleteConfirm,
    BatchDeleteCancel,
    NewFolderOpen(usize),
    NewFolderInput(String),
    NewFolderConfirm,
    NewFolderCancel,
    /// 侧栏文件夹改名（输入行两态，同新建模式）：RenameFolderOpen(账号, 旧路径)
    RenameFolderOpen(usize, String),
    RenameFolderInput(String),
    RenameFolderConfirm,
    RenameFolderCancel,
    AskFolderDelete(usize, String),
    FolderDeleteConfirm,
    FolderDeleteCancel,
    ContactPicked(Field, String),
    // ---- AI 助手（方案：AI 只出建议，执行全走人工点击路径） ----
    /// 读信区摘要
    AiSummarize(usize, i64),
    AiSummaryReady {
        acct: usize,
        hash: i64,
        id: u64,
        result: std::result::Result<String, String>,
    },
    /// 写信屏起草（指令在 Compose.ai_instruction）
    AiDraftGo,
    /// 改写现有正文（润色/语气/长度/翻译；指令为预设文案）
    AiRewrite(String),
    /// 撤销上一次 AI 修改（回快照）
    AiDraftUndo,
    AiDraftReady {
        id: u64,
        result: std::result::Result<smail_ai::DraftContent, String>,
    },
    /// 覆盖确认条：应用 / 放弃 AI 草稿
    AiDraftApply,
    AiDraftDiscard,
    AiInstructionInput(String),
    /// 读信区智能回复（带原信上下文起草）
    AiSmartReply(usize, i64),
    /// 批量整理建议（作用当前选择）
    AiOrganizeGo,
    AiPlanReady {
        id: u64,
        result: std::result::Result<Vec<smail_ai::PlanGroup>, String>,
    },
    AiPlanToggle(usize),
    /// 展开/收起计划组看成员清单
    AiPlanExpand(usize),
    AiPlanApply,
    AiPlanCancel,
    /// 隐私门
    AiPrivacyAck,
    AiPrivacyCancel,
    /// 摘要卡片关闭
    AiSummaryClose,
    // ---- AI 设置（设置屏内联表单） ----
    AiFormOpen,
    AiFormClose,
    AiFormPreset(smail_ai::Preset),
    AiFormToggleEnabled(bool),
    AiFormField(AiField, String),
    AiFormKeychain,
    AiFormTest,
    AiFormTestReady(std::result::Result<String, String>),
    AiFormSave,
    /// 关闭错误横幅（错误也随下一条 Status 事件自动清除）
    ErrorDismiss,
    // ---- B4：账号 PGP 内联编辑 ----
    PgpEditOpen(usize),
    PgpEditField(setup::PgpField, String),
    /// 打开文件选择器（私钥/公钥环）
    PgpEditPick(setup::PgpField),
    PgpEditPicked(setup::PgpField, Option<PathBuf>),
    PgpEditSave,
    PgpEditCancel,
    /// WKD 自动发现开关
    PgpEditWkd(bool),
    // ---- 签名编辑（设置屏内联；compose 预填用） ----
    SignatureEditOpen(usize),
    SignatureEditAction(iced::widget::text_editor::Action),
    SignatureEditSave,
    SignatureEditCancel,
    // ---- B6：账号密码入系统钥匙串 ----
    /// 把该账号的明文 IMAP/SMTP 密码迁入钥匙串，config 改存标记
    KeychainMove(usize),
    // ---- 第 1 批标配：退订 / 归档 / 导出 ----
    /// List-Unsubscribe 退订（http URL 开浏览器；mailto 预填写信）
    Unsubscribe(String),
    /// 归档当前阅读的邮件（乐观移除，服务器搬运后台执行）
    ArchiveCurrent(usize, i64),
    /// 批量归档选中项
    BatchArchive,
    /// 标为垃圾（移到 Junk；当前阅读的邮件）
    SpamCurrent(usize, i64),
    /// 批量标为垃圾
    BatchSpam,
    /// 通讯录删除条目（账号路由）
    ContactDelete(usize, i64),
    /// 请求导出 .eml（弹保存对话框，路径回填后发 Cmd）
    ExportEmlRequest(usize, i64),
    /// 保存对话框返回的路径
    ExportEmlPicked(Option<PathBuf>),
    /// 点击「显示远程图片」（隐私默认禁；下载经服务线程）
    LoadRemoteImages(usize, i64),
    /// HTML 邮件后台 markdown 解析完成。ParsedMd 是一次性传递盒
    ///（Content 无 Clone 而 iced 要求 Message: Clone，只能共享句柄取走即空；
    /// 过期结果——解析期间用户已切信——由 update 丢弃）
    MarkdownParsed {
        acct: usize,
        hash: i64,
        md: ParsedMd,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Field {
    To,
    Cc,
    Bcc,
    Subject,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AiField {
    BaseUrl,
    Model,
    ApiKey,
}

// ---------------------------------------------------------------------------
// 事件接收端包装（Hash 按 id）
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct EventTap {
    id: u64,
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<TaggedEvent>>>,
}

/// 托盘订阅标识（Receiver 无 Hash；用引用地址，静态存活期内稳定）
#[derive(Clone)]
struct TrayTap(usize);

impl PartialEq for TrayTap {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for TrayTap {}
impl std::hash::Hash for TrayTap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl EventTap {
    fn new(rx: tokio::sync::mpsc::Receiver<TaggedEvent>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }
}

impl PartialEq for EventTap {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for EventTap {}
impl std::hash::Hash for EventTap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

fn cli_auth(args: &[String]) -> Result<(), String> {
    let name = args
        .get(2)
        .ok_or_else(|| "用法: s-mail auth <账号名>（名字须与配置里 name 一致）".to_string())?;
    let (cfg_path, home) = paths();
    let accounts = smail_core::load_accounts(&cfg_path).map_err(|e| format!("读配置失败：{e}"))?;
    let acct = accounts
        .into_iter()
        .find(|a| &a.name == name || &a.slug == name)
        .ok_or_else(|| format!("配置里没有账号 {name:?}"))?;
    let oauth = acct
        .oauth
        .as_ref()
        .ok_or_else(|| format!("账号 {name:?} 没有配 [oauth] 段，无需授权"))?;
    let dir = home.join(&acct.slug);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建数据目录失败: {e}"))?;
    smail_core::oauth::authorize(oauth, &dir, true)?;
    println!("账号 {name:?} 授权完成，现在可以启动 s-mail 了");
    Ok(())
}

/// M6：写回配置 + 热重载会话（Drop 旧会话保证数据目录独占，再起新会话）。
/// EventTap 换新实例 → subscription 重新订阅，无需重启应用。
fn update(state: &mut State, message: Message) -> Task<Message> {
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
            state.last_view = Some(("__unified__".into(), "__all_inbox__".into()));
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            state.in_search = false;
            state.viewing_unified = true;
            state.search_text.clear();
            state.cur = Some((0, "__all_inbox__".into()));
            state.rows.clear();
            state.unified_contrib.clear();
            state.selection.clear();
            state.list_offset = 0.0;
            state.reading = None;
            state.selected = None; // 残留 selected 会让快捷键作用于不可见邮件
            state.body_req = None;
            state.move_picker = None;
            state.pending_delete = None;
            state.batch_move_open = false;
            state.pending_batch_delete = false;
            state.rebuild_display();
            state.load_more_pending = false;
            state.load_more_done = false;
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
            state.list_filter = f;
            state.rebuild_display();
            Task::none()
        }
        Message::ToggleListSort => {
            state.list_asc = !state.list_asc;
            state.rebuild_display();
            Task::none()
        }
        Message::FolderSelected(acct, folder) => {
            // 记忆上次视图（按账号名存，账号增删/改名后索引漂移不影响）
            if let Some(name) = state.account_names.get(acct).cloned() {
                state.last_view = Some((name, folder.clone()));
                crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            }
            state.in_search = false;
            state.viewing_unified = false;
            state.search_text.clear();
            state.list_offset = 0.0; // 虚拟化窗口回顶；部件本身靠 scroll_to（见下）
            state.selection.clear(); // 选择不跨文件夹（批量操作按当前列表语义）
            state.batch_move_open = false;
            state.pending_batch_delete = false;
            state.move_picker = None;
            state.pending_delete = None; // 确认条不跨文件夹（确认会删不可见邮件）
            state.body_req = None; // 在途正文不再属于新列表
            if folder == "__drafts__" {
                state.cur = Some((acct, folder));
                state.rows.clear();
                state.selected = None;
                state.reading = None;
                state.send(acct, Cmd::ListDrafts);
            } else if folder == "__outbox__" {
                state.cur = Some((acct, folder));
                state.rows.clear();
                state.selected = None;
                state.reading = None;
                state.send(acct, Cmd::ListOutbox);
            } else {
                state.cur = Some((acct, folder.clone()));
                state.rows.clear();
                state.list_offset = 0.0;
                state.reading = None;
                state.selected = None; // 快捷键/阅读器不再指向旧文件夹邮件
                state.expanded.clear(); // 展开状态不跨文件夹（hash 可能同名冲突）
                state.send(acct, Cmd::SelectFolder(folder));
            }
            state.rebuild_display();
            state.load_more_pending = false;
            state.load_more_done = false;
            scroll_list_to_top()
        }
        Message::RowSelected(acct, hash) => {
            state.body_req = Some((acct, hash));
            state.send(acct, Cmd::OpenMessage(hash));
            Task::none()
        }
        Message::ToggleThreading => {
            state.threaded = !state.threaded;
            state.rebuild_display();
            Task::none()
        }
        Message::ToggleUiTheme => {
            // 手动切换后不再跟随系统（Agency：用户显式选择优先）
            state.theme_follow = false;
            state.ui_theme = match state.ui_theme {
                Theme::Dark => Theme::Light,
                _ => Theme::Dark,
            };
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            Task::none()
        }
        Message::ToggleLang => {
            state.lang = state.lang.toggle();
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            // 状态/错误文案跟随：广播到全部服务线程（立即生效，无需重启）
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::SetLang(matches!(
                    state.lang,
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
                if persist_and_reload(state, accounts) {
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
                        if persist_and_reload(state, accounts) {
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
            // B6：先清理钥匙串条目（删完配置后 marker 无处可查，条目会孤儿化）
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
                persist_and_reload(state, accounts);
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
                iced::window::Event::Resized(size) => state.win_geom.map(|g| {
                    let mut n = g;
                    n[0] = size.width;
                    n[1] = size.height;
                    n
                }),
                iced::window::Event::Moved(pos) => state.win_geom.map(|g| {
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
                state.win_geom = Some(g);
                let due = state
                    .win_save_at
                    .map(|t| t.elapsed().as_secs() >= 2)
                    .unwrap_or(true);
                if due {
                    state.win_save_at = Some(std::time::Instant::now());
                    crate::lang::save_prefs(&state.data_root, &state.current_prefs());
                }
            }
            Task::none()
        }
        Message::SystemTheme(mode) => {
            if state.theme_follow {
                state.ui_theme = match mode {
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
            state.list_offset = y;
            // 实测校正（初始估计值只在首个滚动事件前生效）
            if (view_h - state.list_view_h).abs() > 1.0 {
                state.list_view_h = view_h;
            }
            // 触底翻页：行数恰为页边界（500 倍数）才可能是被截断的满页；
            // 穷尽判定靠 FolderLoaded 后行数不再增长。
            // 内容高度必须用显示模型（会话聚合/过滤后远小于 rows），
            // 否则触底条件永远达不到——会话视图翻页从不触发。
            // 过滤态不开自动翻页：显示行数≪数据行数，任何滚动都会立刻触底，
            // 连环 LoadMore 把整个文件夹抱下来（深度筛选用搜索，它是服务端执行）
            if !state.in_search
                && state.list_filter == ListFilter::All
                && !state.load_more_pending
                && !state.load_more_done
            {
                if let Some((a, f)) = state.cur.clone() {
                    if f != "__drafts__" && f != "__outbox__" && f != "__all_inbox__" {
                        let n = state.rows.len();
                        let content_h = state.mail_display.len() as f32 * ROW_H;
                        if n > 0
                            && n.is_multiple_of(smail_core::UI_LIST_LIMIT as usize)
                            && !state.mail_display.is_empty()
                            && y + view_h >= content_h - 2.0 * ROW_H
                        {
                            state.load_more_pending = true;
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
            let account = state.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
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
            if let Some((a, f)) = state.cur.clone() {
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
            let acct = state.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
            state.send(acct, Cmd::OutboxRetry(id));
            Task::none()
        }
        Message::OutboxRemove(id) => {
            let acct = state.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
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
            state.move_picker = None;
            state
                .rows
                .retain(|(a, r)| !(*a == acct && r.envelope_hash == hash));
            state.rebuild_display();
            state.selection.remove(&(acct, hash)); // 后续批量不再携带死 hash
                                                   // 精确关闭：仅当正在阅读的正是被删邮件（同账号另封信不受牵连），
                                                   // 同时清 body_req 防迟到正文回弹
            close_reading_if_gone(state);
            state.send(acct, Cmd::DeleteMessage(hash));
            Task::none()
        }
        Message::SearchInput(s) => {
            state.search_text = s;
            Task::none()
        }
        Message::SearchSubmit if state.account_names.is_empty() => {
            state.error = Some(state.l().err_need_account_search.into());
            Task::none()
        }
        Message::SearchSubmit => {
            let q = state.search_text.trim().to_string();
            if q.is_empty() {
                return Task::none();
            }
            state.in_search = true;
            state.viewing_unified = false;
            state.search_query = q.clone();
            state.search_contrib.clear();
            state.search_snippets.clear(); // 换词清旧片段
            state.rows.clear();
            state.rebuild_display();
            state.reading = None;
            state.selected = None; // 搜索是独立视图，残留 selected 误导快捷键
            state.body_req = None;
            // 批量状态同样不跨视图：搜索前选中的邮件不属于搜索结果，
            // 残留会让「批量移动/删除」误作用于旧列表的邮件
            state.selection.clear();
            state.batch_move_open = false;
            state.pending_batch_delete = false;
            state.move_picker = None;
            state.pending_delete = None;
            state.list_offset = 0.0;
            if let Some(tx) = &state.cmd_tx {
                let _ = tx.send(SessionCmd::SearchAll(q));
            }
            scroll_list_to_top()
        }
        Message::ManualRefresh => {
            // 只对“当前账号+当前文件夹”有意义；草稿/搜索态忽略；
            // 统一收件箱是伪文件夹——刷新动作改为重拉聚合，否则向服务线程发伪名报错
            if state.viewing_unified {
                if let Some(tx) = &state.cmd_tx {
                    let _ = tx.send(SessionCmd::UnifiedInbox);
                }
                return Task::none();
            }
            if let Some((a, f)) = state.cur.clone() {
                if !state.in_search && f != "__drafts__" {
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
                    let account = state.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
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
            state.move_picker = if state.move_picker == Some((acct, hash)) {
                None
            } else {
                Some((acct, hash))
            };
            Task::none()
        }
        Message::MoveToDest(acct, hash, dest) => {
            state.move_picker = None;
            // 乐观更新：立即从列表移除；服务器结果仅状态提示（失败下次同步回真）
            state
                .rows
                .retain(|(a, r)| !(*a == acct && r.envelope_hash == hash));
            state.rebuild_display();
            state.selection.remove(&(acct, hash));
            if state.selected == Some((acct, hash)) {
                state.reading = None;
                state.selected = None;
                state.body_req = None; // 迟到正文不弹开已移走的邮件
            }
            state.send(acct, Cmd::MoveMessage { hash, dest });
            Task::none()
        }
        Message::ToggleSelect(acct, hash) => {
            if !state.selection.remove(&(acct, hash)) {
                state.selection.insert((acct, hash));
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
                state.selection.insert((d.acct, hash));
            }
            Task::none()
        }
        Message::ClearSelection => {
            state.selection.clear();
            state.batch_move_open = false;
            state.pending_batch_delete = false;
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
            state.batch_move_open = !state.batch_move_open;
            state.pending_batch_delete = false;
            Task::none()
        }
        Message::BatchMoveDest(acct, dest) => {
            state.batch_move_open = false;
            let hashes: Vec<i64> = state
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
            state.selection.retain(|(a, _)| *a != acct);
            state.send(acct, Cmd::BatchMove { hashes, dest });
            Task::none()
        }
        Message::AskBatchDelete => {
            state.pending_batch_delete = true;
            state.batch_move_open = false;
            Task::none()
        }
        Message::BatchDeleteConfirm => {
            state.pending_batch_delete = false;
            for (acct, hashes) in selection_by_acct(state) {
                state
                    .rows
                    .retain(|(a, r)| !(*a == acct && hashes.contains(&r.envelope_hash)));
                state.selection.retain(|(a, _)| *a != acct);
                state.send(acct, Cmd::BatchDelete { hashes });
            }
            state.rebuild_display();
            // 多账号/搜索态：只关掉真正破删的阅读（另一账号正在读的邮件不受牵连）
            close_reading_if_gone(state);
            Task::none()
        }
        Message::BatchDeleteCancel => {
            state.pending_batch_delete = false;
            Task::none()
        }
        Message::NewFolderOpen(acct) => {
            state.new_folder = Some((acct, String::new()));
            Task::none()
        }
        Message::NewFolderInput(name) => {
            if let Some((_, n)) = &mut state.new_folder {
                *n = name;
            }
            Task::none()
        }
        Message::NewFolderConfirm => {
            let Some((acct, name)) = state.new_folder.take() else {
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
            state.new_folder = None;
            Task::none()
        }
        Message::RenameFolderOpen(acct, path) => {
            // 预填当前名（微调场景多；全选替换也方便）
            state.rename_folder = Some((acct, path.clone(), path));
            Task::none()
        }
        Message::RenameFolderInput(name) => {
            if let Some((_, _, n)) = &mut state.rename_folder {
                *n = name;
            }
            Task::none()
        }
        Message::RenameFolderConfirm => {
            let Some((acct, old, new)) = state.rename_folder.take() else {
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
                .folders
                .iter_mut()
                .filter(|(a, f)| *a == acct && f.path == old)
            {
                f.path = new.clone();
            }
            let mut viewing_renamed = false;
            if let Some((a, c)) = &mut state.cur {
                if *a == acct && *c == old {
                    *c = new.clone();
                    viewing_renamed = true;
                }
            }
            state.send(acct, Cmd::RenameFolder(old, new.clone()));
            if viewing_renamed {
                // 服务线程按序处理：先改名（含本地索引换名）再重选；
                // 本地先行立即用新名字刷新行，消除行内存的旧 folder 字段
                state.send(acct, Cmd::SelectFolder(new));
            }
            Task::none()
        }
        Message::RenameFolderCancel => {
            state.rename_folder = None;
            Task::none()
        }
        Message::AskFolderDelete(acct, path) => {
            state.pending_folder_delete = Some((acct, path));
            Task::none()
        }
        Message::FolderDeleteConfirm => {
            let Some((acct, path)) = state.pending_folder_delete.take() else {
                return Task::none();
            };
            // 正在浏览该文件夹：清视图（本地记录由服务线程清理）
            if state.cur.as_ref().map(|(a, c)| *a == acct && c == &path) == Some(true) {
                state.cur = None;
                state.rows.clear();
                state.rebuild_display();
                state.reading = None;
                state.selected = None;
            }
            state
                .folders
                .retain(|(a, f)| !(*a == acct && f.path == path));
            state.send(acct, Cmd::DeleteFolder(path));
            Task::none()
        }
        Message::FolderDeleteCancel => {
            state.pending_folder_delete = None;
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
            state.selection.remove(&(acct, hash));
            state.move_picker = None;
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
                state.selection.retain(|(a, _)| *a != acct);
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
                let account = state.cur.as_ref().map(|(a, _)| *a).unwrap_or(0);
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
            state.selection.remove(&(acct, hash));
            state.move_picker = None;
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
                state.selection.retain(|(a, _)| *a != acct);
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
                persist_and_reload(state, accounts);
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
                    persist_and_reload(state, accounts);
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
fn close_reading_if_gone(state: &mut State) {
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
fn selection_by_acct(state: &State) -> Vec<(usize, Vec<i64>)> {
    let mut by: std::collections::BTreeMap<usize, Vec<i64>> = Default::default();
    for (a, h) in &state.selection {
        by.entry(*a).or_default().push(*h);
    }
    by.into_iter().collect()
}

fn subscription(state: &State) -> Subscription<Message> {
    // 系统主题变化（跟随模式）+ 键盘（无修饰键才处理，避免劫持快捷键）
    let keyboard = iced::keyboard::listen().filter_map(|e| match e {
        iced::keyboard::Event::KeyPressed { key, modifiers, .. } if modifiers.is_empty() => {
            Some(Message::KeyPressed(key))
        }
        _ => None,
    });
    let theme_sub = iced::system::theme_changes().map(Message::SystemTheme);
    // 写信自动保存节拍（只在写作屏激活；每 5s 检查，静默 20s 落草稿）
    let autosave_sub = if matches!(state.screen, Screen::Compose(_)) {
        iced::time::every(std::time::Duration::from_secs(5)).map(|_| Message::AutosaveTick)
    } else {
        Subscription::none()
    };
    // 窗口几何记忆：Resized/Moved 更新内存，节流写盘
    let window_sub = iced::window::events().map(|(_id, ev)| Message::WindowGeometry(ev));
    // 托盘事件（std mpsc → 轮询转 Message；100ms 足够托盘交互）
    let tray_sub = match crate::tray::take_receiver() {
        Some(rx) => {
            Subscription::run_with(TrayTap(rx as *const _ as usize), |t: &TrayTap| {
                // 指针指向 OnceLock 里的静态 Mutex（与进程同寿，解引用安全）
                let rx: &'static std::sync::Mutex<
                    std::sync::mpsc::Receiver<crate::tray::TrayAction>,
                > = unsafe { &*(t.0 as *const _) };
                iced::stream::channel(
                    8,
                    move |mut sender: iced::futures::channel::mpsc::Sender<Message>| async move {
                        loop {
                            loop {
                                // guard 限在本块（跨 await 会把 future 变 !Send）
                                let next = { rx.lock().unwrap().try_recv() };
                                match next {
                                    Ok(a) => {
                                        if sender.send(Message::Tray(a)).await.is_err() {
                                            return;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    },
                )
            })
        }
        None => Subscription::none(),
    };
    let Some(events) = state.events.clone() else {
        return Subscription::batch([keyboard, theme_sub, window_sub, autosave_sub, tray_sub]);
    };
    let events_sub = Subscription::run_with(events, |events: &EventTap| {
        let events = events.rx.clone();
        iced::stream::channel(
            50,
            move |mut sender: iced::futures::channel::mpsc::Sender<Message>| async move {
                loop {
                    let ev = {
                        let mut rx = events.lock().await;
                        rx.recv().await
                    };
                    match ev {
                        Some(ev) => {
                            // send().await 满仓时背压等待。旧 try_send 失败即 break，
                            // 事件风暴（多账号首次同步连发）下会永久断流且无任何征兆
                            if sender.send(Message::Ev(ev)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            },
        )
    });
    Subscription::batch([
        keyboard,
        theme_sub,
        window_sub,
        autosave_sub,
        tray_sub,
        events_sub,
    ])
}

// ---------------------------------------------------------------------------
// view
// ---------------------------------------------------------------------------

fn view(state: &State) -> Element<'_, Message> {
    // 错误横幅（曾被写给一个永不渲染的字段——所有错误提示静默丢失，修复：
    // 顶层显示 + ✕ 关闭 + 新动作/新 Status 自动清除）
    if let Some(err) = &state.error {
        return container(
            column![
                text("✉️ s-mail")
                    .size(20)
                    .font(theme::ui_font(iced::font::Weight::Bold)),
                container(
                    row![
                        text(err.clone()).size(13),
                        Space::new().width(Length::Fill),
                        button(text(state.l().err_close).size(12))
                            .on_press(Message::ErrorDismiss)
                            .padding([2, 8]),
                    ]
                    .spacing(8),
                )
                .padding([8, 14])
                .width(520)
                .style(|t| theme::status_badage(t, theme::BadgeKind::Bad)),
            ]
            .spacing(12)
            .align_x(iced::alignment::Alignment::Center),
        )
        .center(Length::Fill)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
    }
    if let Some(hint) = &state.setup_hint {
        return container(
            column![
                text("✉️ s-mail")
                    .size(30)
                    .font(theme::ui_font(iced::font::Weight::Bold)),
                text("还没有配置账号").size(14).style(theme::secondary_text),
                container(
                    column![
                        text(state.error.clone().unwrap_or_default()).size(12),
                        text(hint.to_string()).size(12),
                    ]
                    .spacing(6)
                )
                .padding(14)
                .width(520)
                .style(theme::reader_meta),
            ]
            .spacing(14)
            .align_x(iced::alignment::Alignment::Center),
        )
        .center(Length::Fill)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
    }
    match &state.screen {
        Screen::Compose(c) => compose_view(state, c),
        Screen::Mail => main_view(state),
        Screen::Setup(f) => setup_view(state, f),
    }
}

/// 键盘导航：↑↓ 选信（带滚动跟随），Esc 层级退出（写作→取消删除→退出搜索→清词）。
/// 门闸：搜索框有内容/搜索结果态/写作屏时不劫持方向键（避免和输入框光标移动打架）。
fn extract_forward_attachments(eml: &PathBuf, staging: &PathBuf) -> Vec<PathBuf> {
    use mail_parser::{MessageParser, MimeHeaders};
    let Ok(bytes) = std::fs::read(eml) else {
        return vec![];
    };
    // 护栏：超大邮件不做内存提取
    if bytes.len() > 512 * 1024 * 1024 {
        return vec![];
    }
    let Some(m) = MessageParser::default().parse(&bytes) else {
        return vec![];
    };
    let _ = std::fs::create_dir_all(staging);
    let stem = eml.file_stem().and_then(|s| s.to_str()).unwrap_or("fwd");
    let mut out = Vec::new();
    for (i, a) in m.attachments().enumerate() {
        let Some(name) = a.attachment_name() else {
            continue;
        };
        let safe: String = name
            .chars()
            .map(|c| {
                if c == '/' || c == '\\' || c == '\0' {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        if safe.is_empty() || safe == "." || safe == ".." {
            continue;
        }
        let path = staging.join(format!("{stem}-{i}-{safe}"));
        if std::fs::write(&path, a.contents()).is_ok() {
            out.push(path);
        }
    }
    out
}

/// 系统默认浏览器打开 URL（macOS open / Linux xdg-open）
/// 从 .eml 提取全部附件到 staging（转发用）；失败/无附件返回空
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    #[cfg(unix)]
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// 图片格式魔数判定（PNG/JPEG/GIF/WebP）——inline 附件不一定都是图片
fn is_image_bytes(b: &[u8]) -> bool {
    b.starts_with(&[0x89, b'P', b'N', b'G'])
        || (b.len() > 2 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF)
        || (b.len() > 3 && b[0] == b'G' && b[1] == b'I' && b[2] == b'F')
        || (b.len() > 12
            && b[0] == b'R'
            && b[1] == b'I'
            && b[2] == b'F'
            && b[3] == b'F'
            && b[8..12] == *b"WEBP")
}

/// unix 秒 → 本地时间字符串（无时区库依赖的手写民用历；
/// 与渲染都有关，list/reader/compose 模块共用）
pub(crate) fn unix_to_local(ts: i64) -> String {
    let secs = ts.max(0) as u64;
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod update_tests;
