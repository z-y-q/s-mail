//! s-mail 多账号邮件客户端
//!
//! ## 职责
//!
//! - **iced UI**：基于 iced 0.14 的跨平台桌面应用
//! - **多账号聚合**：侧栏按账号分组、搜索全账号合并、发件账号切换
//! - **状态管理**：会话路由、视图切换、表单生命周期
//! - **系统集成**：新邮件通知、文件保存对话框、OAuth 授权跳转
//!
//! ## 配置与数据
//!
//! - 配置：`SMAIL_CONFIG` 环境变量或 `~/.s-mail/config.toml`
//!   - 支持多账号 `[[accounts]]` 和单账号 `[account]`（兼容旧版）
//! - 数据：`SMAIL_HOME` 环境变量或 `~/.s-mail/<账号slug>/`
//!
//! ## 架构
//!
//! ```text
//! UI 主线程（iced event loop）
//!   ├─ update() 处理消息 ────> Session::cmd_tx (多路复用器)
//!   │                               ├──> 账号0服务线程
//!   │                               ├──> 账号1服务线程
//!   │                               └──> 账号2服务线程
//!   │
//!   └─ subscription() <────────── Session::event_rx (带账号标签)
//!       └─ view() 渲染
//! ```
//!
//! ## 模块划分
//!
//! - `main.rs`：数据类型、update/事件循环、应用入口
//! - `state.rs`：State 结构体（会话状态、导航、批量操作）
//! - `list.rs`：邮件列表视图（分栏、虚拟滚动）
//! - `reader.rs`：邮件阅读视图（正文、附件、内联图片）
//! - `compose.rs`：写信表单（编辑器、附件、发件账号切换）
//! - `setup.rs`：首次设置引导（账号配置、OAuth 授权）
//! - `theme.rs`：颜色主题、字体、样式常量
//! - `keys.rs`：键盘快捷键（j/k 导航、r 回复、/ 搜索）
//! - `lang.rs`：国际化（中英文切换）
//! - `notify.rs`：系统通知（新邮件桌面提醒）
//! - `ai.rs`：AI 功能集成（邮件总结、智能回复）

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use iced::futures::SinkExt;
use iced::widget::{button, column, container, row, text, Space};
use iced::{Element, Font, Length, Subscription, Theme};

mod ai;
mod compose;
mod events;
mod keys;
mod lang;
mod list;
mod list_view;
mod notify;
mod reader;
mod setup;
mod state;
mod theme;
mod tray;
mod update;

use compose::{compose_view, Compose};
pub(crate) use list::ROW_H;
use list_view::main_view;
use setup::{build_account, setup_view, Preset, SetupField, SetupForm};

use smail_core::TaggedEvent;

// 拆分模块的符号回灌（调用点零改动）
pub(crate) use events::handle_event;
pub(crate) use keys::{handle_key, scroll_list_to_top};
pub(crate) use state::*;
pub(crate) use update::{close_reading_if_gone, update};

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
        .theme(|state: &State| state.prefs.ui_theme.clone())
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
    /// 写信屏快捷键（⌘↩ 发送 / ⌘S 存草稿；订阅层从修饰键组合里分拣）
    ComposeShortcut(ComposeKey),
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
    // ---- 同步间隔（账号级 sync_config；内联编辑两态模式） ----
    SyncEditOpen(usize),
    SyncEditInput(String),
    SyncEditSave,
    SyncEditCancel,
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

/// 写信屏快捷键动作（⌘↩ / ⌘S）
#[derive(Debug, Clone, Copy)]
pub(crate) enum ComposeKey {
    Send,
    SaveDraft,
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

fn subscription(state: &State) -> Subscription<Message> {
    // 系统主题变化（跟随模式）+ 键盘（无修饰键才处理，避免劫持快捷键；
    // ⌘/Ctrl 组合单独分拣出写信屏的发送/存草稿）
    let keyboard = iced::keyboard::listen().filter_map(|e| match e {
        iced::keyboard::Event::KeyPressed { key, modifiers, .. } => {
            if modifiers.is_empty() {
                Some(Message::KeyPressed(key))
            } else if modifiers.command() {
                match &key {
                    iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter) => {
                        Some(Message::ComposeShortcut(crate::ComposeKey::Send))
                    }
                    iced::keyboard::Key::Character(c) if c.eq_ignore_ascii_case("s") => {
                        Some(Message::ComposeShortcut(crate::ComposeKey::SaveDraft))
                    }
                    _ => None,
                }
            } else {
                None
            }
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

/// 列表行短日期：同年显示「MM-DD HH:MM」，往年只显示「YYYY-MM-DD」。
/// 列宽有限，完整秒级时间在阅读页头部（两处都走同一本地时钟，年份可直接比）
pub(crate) fn unix_to_local_short(ts: i64) -> String {
    let full = unix_to_local(ts);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now_full = unix_to_local(now);
    if now_full.as_bytes().get(..4) == full.as_bytes().get(..4) {
        full.get(5..16).unwrap_or(&full).to_string()
    } else {
        full.get(..10).unwrap_or(&full).to_string()
    }
}

#[cfg(test)]
mod update_tests;
