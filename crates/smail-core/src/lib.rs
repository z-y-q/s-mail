//! smail-core：账号服务线程（M3）。
//!
//! M1：收信同步 / 正文懒加载 / IDLE
//! M2：SMTP 发信 + outbox 队列 / flags 双向 / 断线重连 / 本地草稿
//! M3：FTS 全文搜索（bigram）+ 后台索引器 + 附件收发
//!
//! 同步策略：全量信封 diff（M4 候选：UID/QRESYNC 增量）。

pub mod html;
pub mod keychain;
pub mod oauth;
pub mod pgp;
pub mod threads;
pub mod wkd;

use pgp::PgpCfg;

use oauth::OAuthCfg;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use smail_backend::{
    connect, BackendError, EnvelopeSummary, FlagChange, MailBackend, OutgoingMail, ServerEvent,
};
pub use smail_backend::{test_connection, FolderKind, ImapConfig, SmtpConfig};
use smail_store::{DraftRow, MessageRow, Store};

// ---------------------------------------------------------------------------
// 通道协议
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Cmd {
    SelectFolder(String),
    /// UI 语言（服务线程状态/错误文案用；不改任何数据行为）
    SetLang(bool),
    OpenMessage(i64),
    Shutdown,
    SendMail {
        to: String,
        cc: String,
        bcc: String,
        subject: String,
        body: String,
        /// 附件文件路径（服务线程读文件）
        attachments: Vec<String>,
        /// M5-d：PGP 签名
        sign: bool,
        /// M5-d：PGP 加密（隐含签名）
        encrypt: bool,
    },
    SaveDraft {
        id: Option<i64>,
        to: String,
        cc: String,
        bcc: String,
        subject: String,
        body: String,
    },
    ListDrafts,
    /// 发件箱视图：全量列表（含 failed）
    ListOutbox,
    /// 发件箱：重发一条（回 pending，立即尝试）
    OutboxRetry(i64),
    /// 发件箱：删除一条
    OutboxRemove(i64),
    /// 清空废纸篓（服务器永久删除该文件夹全部邮件）
    EmptyTrash(String),
    DeleteDraft(i64),
    MarkSeen(i64, bool),
    ToggleFlagged(i64, bool),
    DeleteMessage(i64),
    // M3
    Search(String),
    /// 手动刷新：绕过 TTL 立即全量同步指定文件夹（顶栏 ↻ / F5；TTL 的安全出口）
    ForceRefresh(String),
    /// 触底加载下一页（本地索引扩大重发；服务端新邮件由 IDLE 推送）
    LoadMore(String),
    /// 一键归档：解析归档目标（无则新建 Archive）后批量移动
    Archive(Vec<i64>),
    /// 标为垃圾：解析垃圾目标（无则新建 Junk）后批量移动（v1 无自学习）
    Spam(Vec<i64>),
    /// 通讯录：全量列表（管理界面用；带名字与 ID）
    ListContactsFull,
    /// 通讯录删除条目
    DeleteContact(i64),
    /// 导出原始 .eml 到指定路径（UI 先弹保存对话框拿路径）
    ExportEml {
        hash: i64,
        path: String,
    },
    /// 按需加载远程图片（用户点击「显示图片」后；隐私默认禁远程图）
    LoadImages {
        hash: i64,
        urls: Vec<String>,
    },
    /// 只读本地索引发列表（统一收件箱聚合用；不触发服务器同步，
    /// 新邮件由 IDLE 推送）
    LocalList(String),
    SaveAttachment {
        hash: i64,
        name: String,
    },
    // ---- M7：手动移动/批量操作/文件夹管理/联系人 ----
    /// 单封移动（读信视图「移动到」）
    MoveMessage {
        hash: i64,
        dest: String,
    },
    /// 批量标记（seen/flagged 至少其一；可跨文件夹，服务按 folder 分组）
    BatchFlags {
        hashes: Vec<i64>,
        seen: Option<bool>,
        flagged: Option<bool>,
    },
    BatchMove {
        hashes: Vec<i64>,
        dest: String,
    },
    BatchDelete {
        hashes: Vec<i64>,
    },
    CreateFolder(String),
    DeleteFolder(String),
    RenameFolder(String, String),
    /// 最近联系人（compose 自动补全用）
    ListContacts,
}

#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub path: String,
    pub total: u32,
    pub unseen: u32,
    /// 特殊文件夹类型（UI 图标/Drafts 重定向/Junk 动作）
    pub kind: smail_backend::FolderKind,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // BodyLoaded 带图片字节——事件流一次性传递，盒装收益小于可读性代价
pub enum Event {
    Status(String),
    Error(String),
    Folders(Vec<FolderInfo>),
    FolderLoaded {
        folder: String,
        rows: Vec<MessageRow>,
    },
    BodyLoaded {
        hash: i64,
        /// 图片附件（name, bytes）：UI 内联预览用；魔数过滤 + 2MB/张上限
        attachment_images: Vec<(String, Vec<u8>)>,
        /// PDF 附件缩略图（name, png_bytes）：QuickLook 生成，UI 预览用
        attachment_previews: Vec<(String, Vec<u8>)>,
        subject: String,
        /// 原信收件人（"名字 <a@b>, …" 原样，回复全部用）
        to_addrs: String,
        cc_addrs: String,
        /// List-Unsubscribe 退订入口（http/mailto），无则 None
        unsubscribe: Option<String>,
        /// 内嵌图片（cid；随邮件自带，自动显示）：(cid, 字节)
        inline_images: Vec<(String, Vec<u8>)>,
        /// 远程图片 URL（隐私默认不加载；用户点击后经 LoadImages 按需下载）
        remote_images: Vec<String>,
        /// M5-c：PGP 状态（None=非 PGP 邮件）
        pgp: Option<String>,
        body: String,
        /// true = body 是 markdown（源自 HTML 邮件，UI 用 markdown widget 渲染）
        is_html: bool,
        /// (文件名, 字节数)
        attachments: Vec<(String, usize)>,
    },
    MailSent {
        to: String,
        subject: String,
    },
    DraftsListed(Vec<DraftRow>),
    DraftSaved {
        id: i64,
    },
    DraftDeleted(i64),
    MessageDeleted(i64),
    FlagsChanged {
        hash: i64,
        seen: Option<bool>,
        flagged: Option<bool>,
    },
    /// 发件箱计数变化（侧栏入口显隐/刷新信号）
    OutboxPending {
        pending: usize,
        failed: usize,
    },
    /// 发件箱全量（发件箱视图）
    OutboxListed(Vec<smail_store::OutboxRow>),
    /// 连接状态（true=已连接；断连时 UI 持续提示而非一闪而过的状态行）
    ConnState(bool),
    // M3
    SearchResults {
        query: String,
        rows: Vec<MessageRow>,
        /// 匹配正文片段（与 rows 同序；空=仅主题命中）
        snippets: Vec<String>,
    },
    /// 统一收件箱聚合行（LocalList 的回流；带账号标签）
    UnifiedRows {
        rows: Vec<MessageRow>,
    },
    /// 远程图片下载完成（按需加载；键回 hash）
    ImagesLoaded {
        hash: i64,
        images: Vec<(String, Vec<u8>)>,
    },
    /// M6：OAuth 设备流等待用户授权（GUI 显示链接+验证码横幅）
    OAuthPrompt {
        url: String,
        code: String,
    },
    AttachmentSaved {
        path: String,
    },
    // ---- M7 ----
    /// INBOX 增量新信（系统通知用；初次全量同步不发）
    NewMail {
        from: String,
        subject: String,
        /// 新邮件所在文件夹（通知跳转直达；空=未知）
        folder: String,
        /// 新邮件 hash（通知跳转直达）
        hash: i64,
    },
    /// 最近联系人（Cmd::ListContacts 的响应）
    /// 通讯录补全数据（compose 用，display 格式 "名字 <a@b>"）
    ContactsListed(Vec<String>),
    /// 通讯录全量（管理界面用）
    ContactsFull(Vec<smail_store::ContactRow>),
}

// ---------------------------------------------------------------------------
// M3-b：多账号配置与 Session 多路复用
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct AccountEntry {
    name: Option<String>,
    imap: ImapConfig,
    smtp: Option<SmtpConfig>,
    #[serde(default)]
    oauth: Option<OAuthCfg>,
    #[serde(default)]
    rules: Vec<Rule>,
    #[serde(default)]
    pgp: Option<PgpCfg>,
    #[serde(default)]
    signature: Option<String>,
}

/// M5-b：过滤规则（同步时对新见到的邮件应用，天然幂等）
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Rule {
    /// 发件人子串（不区分大小写）
    #[serde(default)]
    pub from: Option<String>,
    /// 主题子串（不区分大小写）
    #[serde(default)]
    pub subject: Option<String>,
    /// 只在该文件夹应用（缺省=任意文件夹）
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub mark_seen: bool,
    #[serde(default)]
    pub flag: bool,
    /// 移动到该服务器文件夹
    #[serde(default)]
    pub move_to: Option<String>,
}

impl Rule {
    fn matches(&self, m: &MessageRow) -> bool {
        if self.from.is_none() && self.subject.is_none() {
            return false;
        }
        if let Some(f) = &self.from {
            if !m.from_addr.to_lowercase().contains(&f.to_lowercase()) {
                return false;
            }
        }
        if let Some(sub) = &self.subject {
            if !m.subject.to_lowercase().contains(&sub.to_lowercase()) {
                return false;
            }
        }
        true
    }

    fn has_action(&self) -> bool {
        self.mark_seen || self.flag || self.move_to.is_some()
    }
}

#[derive(serde::Deserialize)]
struct FileCfgV2 {
    /// 旧版单账号（[account]/[smtp]），保留兼容
    account: Option<ImapConfig>,
    smtp: Option<SmtpConfig>,
    /// 旧版单账号的 [oauth]
    #[serde(default)]
    oauth: Option<OAuthCfg>,
    #[serde(default)]
    accounts: Vec<AccountEntry>,
}

/// 解析后的账号配置
#[derive(Clone)]
pub struct AccountConfig {
    pub name: String,
    /// 数据目录名（只留安全字符）
    pub slug: String,
    pub imap: ImapConfig,
    pub smtp: Option<SmtpConfig>,
    pub oauth: Option<OAuthCfg>,
    pub rules: Vec<Rule>,
    pub pgp: Option<PgpCfg>,
    pub identity: String,
    /// 发信签名（compose 预填到正文尾）
    pub signature: Option<String>,
}

/// 数据目录名（slug）派生：ASCII 安全字符，纯中文名回退 username，再不行回退 acct-N。
/// load_accounts 与 GUI 保存共用，保证手写配置与界面添加行为一致。
pub fn slug_for(name_or_user: &str, fallback: &str, idx: usize) -> String {
    let sanitize = |x: &str| -> String {
        x.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut slug = sanitize(name_or_user);
    if !slug.chars().any(|c| c.is_ascii_alphanumeric()) {
        slug = sanitize(fallback);
    }
    if !slug.chars().any(|c| c.is_ascii_alphanumeric()) {
        slug = format!("acct-{idx}");
    }
    slug
}

/// 同时接受 [[accounts]] 多账号与旧版 [account]/[smtp] 单账号格式
pub fn load_accounts(cfg_path: &Path) -> Result<Vec<AccountConfig>, StartError> {
    let raw = std::fs::read_to_string(cfg_path)
        .map_err(|_| StartError::ConfigMissing(cfg_path.to_path_buf()))?;
    let file: FileCfgV2 = toml::from_str(&raw).map_err(|e| {
        StartError::BadConfig(format!(
            "{e}（[[accounts]] 或旧版 [account] 格式，见 config.example.toml）"
        ))
    })?;

    let mut out = Vec::new();
    let slugify = |s: &str, fallback: &str, idx: usize| -> String { slug_for(s, fallback, idx) };

    if let Some(imap) = file.account {
        let smtp = file.smtp.clone();
        let identity = smtp
            .as_ref()
            .and_then(|s| s.from.clone())
            .unwrap_or_else(|| imap.username.clone());
        let idx = out.len() + 1;
        out.push(AccountConfig {
            slug: slugify(&imap.username, &imap.username, idx),
            name: "默认".into(),
            identity,
            imap,
            smtp,
            oauth: file.oauth,
            rules: Vec::new(),
            pgp: None,
            signature: None,
        });
    }
    for entry in file.accounts {
        let identity = entry
            .smtp
            .as_ref()
            .and_then(|s| s.from.clone())
            .unwrap_or_else(|| entry.imap.username.clone());
        let name = entry.name.unwrap_or_else(|| entry.imap.username.clone());
        let idx = out.len() + 1;
        out.push(AccountConfig {
            slug: slugify(&name, &entry.imap.username, idx),
            name,
            identity,
            imap: entry.imap,
            smtp: entry.smtp,
            oauth: entry.oauth,
            rules: entry.rules,
            pgp: entry.pgp,
            signature: entry.signature,
        });
    }
    if out.is_empty() {
        return Err(StartError::BadConfig("配置里没有任何账号".into()));
    }
    // 规则校验：无匹配条件或无动作的规则直接报配置错（静默失效更糟）
    for (i, a) in out.iter().enumerate() {
        for (j, r) in a.rules.iter().enumerate() {
            if r.from.is_none() && r.subject.is_none() {
                return Err(StartError::BadConfig(format!(
                    "账号#{} 规则#{} 缺少 from/subject 匹配条件",
                    i + 1,
                    j + 1
                )));
            }
            if !r.has_action() {
                return Err(StartError::BadConfig(format!(
                    "账号#{} 规则#{} 没有任何动作（mark_seen/flag/move_to）",
                    i + 1,
                    j + 1
                )));
            }
        }
    }
    // slug 去重（同名账号）
    let mut seen: Vec<String> = Vec::new();
    for a in out.iter_mut() {
        let base = a.slug.clone();
        let mut n = 1;
        while seen.contains(&a.slug) {
            n += 1;
            a.slug = format!("{base}-{n}");
        }
        seen.push(a.slug.clone());
    }
    Ok(out)
}

/// M6：写回配置（[[accounts]] 格式，原子替换）。GUI 里添加/删除账号后调用。
pub fn save_accounts(cfg_path: &Path, accounts: &[AccountConfig]) -> Result<(), StartError> {
    #[derive(serde::Serialize)]
    struct FileOut<'a> {
        name: &'a str,
        imap: &'a ImapConfig,
        #[serde(skip_serializing_if = "Option::is_none")]
        smtp: Option<&'a SmtpConfig>,
        #[serde(skip_serializing_if = "Option::is_none")]
        oauth: Option<&'a OAuthCfg>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pgp: Option<&'a PgpCfg>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<&'a str>,
        #[serde(skip_serializing_if = "<[Rule]>::is_empty")]
        rules: &'a [Rule],
    }
    #[derive(serde::Serialize)]
    struct Wrapper<'a> {
        accounts: Vec<FileOut<'a>>,
    }
    let out = Wrapper {
        accounts: accounts
            .iter()
            .map(|a| FileOut {
                name: &a.name,
                imap: &a.imap,
                smtp: a.smtp.as_ref(),
                oauth: a.oauth.as_ref(),
                pgp: a.pgp.as_ref(),
                signature: a.signature.as_deref(),
                rules: &a.rules,
            })
            .collect(),
    };
    let body = toml::to_string_pretty(&out)
        .map_err(|e| StartError::BadConfig(format!("序列化配置失败: {e}")))?;
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| StartError::BadConfig(format!("建配置目录失败: {e}")))?;
    }
    let tmp = cfg_path.with_extension("toml.tmp");
    // 密码/授权码明文落盘：创建即 0600（umask 补不上，必须显式 mode；
    // 与 oauth.json 的 0600 策略一致）
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| StartError::BadConfig(format!("写配置失败: {e}")))?
    };
    #[cfg(not(unix))]
    let mut f = std::fs::File::create(&tmp)
        .map_err(|e| StartError::BadConfig(format!("写配置失败: {e}")))?;
    use std::io::Write as _;
    f.write_all(body.as_bytes())
        .map_err(|e| StartError::BadConfig(format!("写配置失败: {e}")))?;
    drop(f);
    std::fs::rename(&tmp, cfg_path)
        .map_err(|e| StartError::BadConfig(format!("替换配置失败: {e}")))?;
    Ok(())
}

/// 多账号命令
#[derive(Debug)]
pub enum SessionCmd {
    /// 路由到指定账号
    Route {
        account: usize,
        cmd: Cmd,
    },
    /// 全账号搜索（结果逐账号回流）
    SearchAll(String),
    /// 统一收件箱：各账号本地 INBOX 列表回流聚合
    UnifiedInbox,
    /// 通讯录全量列表（各账号回流，管理界面聚合显示）
    ContactsFull,
    /// UI 语言切换：广播到全部服务线程（状态/错误文案跟随）
    SetLang(bool),
    Shutdown,
}

/// 带账号标签的事件
#[derive(Debug, Clone)]
pub struct TaggedEvent {
    pub account: usize,
    pub event: Event,
}

/// 多账号会话：每账号一个服务线程，事件统一打标签回流
pub struct Session {
    pub cmd_tx: std::sync::mpsc::Sender<SessionCmd>,
    mux: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(SessionCmd::Shutdown);
        if let Some(h) = self.mux.take() {
            let _ = h.join();
        }
    }
}

impl Session {
    pub fn start(
        cfg_path: &Path,
        data_root: &Path,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<TaggedEvent>), StartError> {
        let accounts = load_accounts(cfg_path)?;
        std::fs::create_dir_all(data_root)
            .map_err(|e| StartError::Backend(format!("建数据目录失败: {e}")))?;

        let mut services = Vec::new();
        for acfg in &accounts {
            let dir = data_root.join(&acfg.slug);
            let (svc, rx) = AccountService::start_for(acfg, &dir)?;
            services.push((svc, rx));
        }

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SessionCmd>();
        // 有界（UI 卡住时背压到 mux 线程，而不是无界堆内存；此前 unbounded
        // 在 UI 冻结期间事件无限累积）
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<TaggedEvent>(1000);
        let zh_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        // 终报必须在 mux 线程内、循环退出后发——此前误放主线程（每次启动
        // 立即误报“会话已结束”，m5 e2e 苔出）；loop 内用 break 退出。
        let mux = std::thread::spawn(move || {
            'mux: loop {
                match cmd_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(SessionCmd::SetLang(zh)) => {
                        zh_flag.store(zh, std::sync::atomic::Ordering::Relaxed);
                        for (svc, _) in &services {
                            let _ = svc.cmd_tx.send(Cmd::SetLang(zh));
                        }
                    }
                    Ok(SessionCmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                        for (svc, _) in &services {
                            let _ = svc.cmd_tx.send(Cmd::Shutdown);
                        }
                        for (svc, _) in &mut services {
                            svc.stop(); // join 服务线程：会话 Drop 返回时数据目录独占可保证
                        }
                        break 'mux;
                    }
                    Ok(SessionCmd::Route { account, cmd }) => {
                        if let Some((svc, _)) = services.get(account) {
                            let _ = svc.cmd_tx.send(cmd);
                        }
                    }
                    Ok(SessionCmd::SearchAll(q)) => {
                        for (svc, _) in &services {
                            let _ = svc.cmd_tx.send(Cmd::Search(q.clone()));
                        }
                    }
                    Ok(SessionCmd::UnifiedInbox) => {
                        for (svc, _) in &services {
                            let _ = svc.cmd_tx.send(Cmd::LocalList("INBOX".into()));
                        }
                    }
                    Ok(SessionCmd::ContactsFull) => {
                        for (svc, _) in &services {
                            let _ = svc.cmd_tx.send(Cmd::ListContactsFull);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
                for (i, (_, rx)) in services.iter_mut().enumerate() {
                    while let Ok(ev) = rx.try_recv() {
                        // blocking_send：UI 冻结时 mux 等待（背压），通道关闭则结束
                        if event_tx
                            .blocking_send(TaggedEvent {
                                account: i,
                                event: ev,
                            })
                            .is_err()
                        {
                            break 'mux;
                        }
                    }
                }
            }
            // mux 退出（Shutdown / 全部服务结束）：给 UI 一个显式终报，
            // 而不是事件流悄悄断掉（用户表现为“再无任何同步”）
            let zh = zh_flag.load(std::sync::atomic::Ordering::Relaxed);
            let _ = event_tx.blocking_send(TaggedEvent {
                account: 0,
                event: Event::Error(tr_str(
                    zh,
                    "会话已结束（如非主动退出，请重启应用）",
                    "Session ended (if unexpected, please restart the app)",
                )),
            });
        });

        Ok((
            Self {
                cmd_tx,
                mux: Some(mux),
            },
            event_rx,
        ))
    }
}

// ---------------------------------------------------------------------------
// 服务
// ---------------------------------------------------------------------------

pub struct AccountService {
    pub cmd_tx: Sender<Cmd>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// 服务线程的中断旗：shutdown/重建会话时置位（设备流轮询、长阻塞操作按粒度退出）
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for AccountService {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
    }
}

#[derive(Debug)]
pub enum StartError {
    ConfigMissing(PathBuf),
    BadConfig(String),
    Backend(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::ConfigMissing(p) => write!(f, "配置文件不存在: {}", p.display()),
            StartError::BadConfig(e) => write!(f, "配置解析失败: {e}"),
            StartError::Backend(e) => write!(f, "后端初始化失败: {e}"),
        }
    }
}

#[derive(serde::Deserialize)]
struct FileCfg {
    account: ImapConfig,
    smtp: Option<SmtpConfig>,
    #[serde(default)]
    oauth: Option<OAuthCfg>,
}

/// 应用配置（UI 显示用）
#[derive(Clone)]
pub struct AppConfig {
    pub imap: ImapConfig,
    pub smtp: Option<SmtpConfig>,
    pub oauth: Option<OAuthCfg>,
    pub rules: Vec<Rule>,
    pub pgp: Option<PgpCfg>,
    pub identity: String,
}

pub fn load_config(cfg_path: &Path) -> Result<AppConfig, StartError> {
    let raw = std::fs::read_to_string(cfg_path)
        .map_err(|_| StartError::ConfigMissing(cfg_path.to_path_buf()))?;
    let file: FileCfg = toml::from_str(&raw).map_err(|e| {
        StartError::BadConfig(format!(
            "{e}（参考 spikes/spike-melib/config.example.toml）"
        ))
    })?;
    let identity = file
        .smtp
        .as_ref()
        .and_then(|s| s.from.clone())
        .unwrap_or_else(|| file.account.username.clone());
    Ok(AppConfig {
        imap: file.account,
        smtp: file.smtp,
        oauth: file.oauth,
        rules: Vec::new(),
        pgp: None,
        identity,
    })
}

impl AccountService {
    /// 旧入口（examples 用）：单账号配置文件
    pub fn start(
        cfg_path: &Path,
        data_dir: &Path,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<Event>), StartError> {
        let mut accounts = load_accounts(cfg_path)?;
        let first = accounts.swap_remove(0);
        Self::start_for(&first, data_dir)
    }

    /// 多账号入口：接收已解析配置
    pub fn start_for(
        cfg: &AccountConfig,
        data_dir: &Path,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<Event>), StartError> {
        // B6：keychain 标记 → 真实密码（只在此处解析进内存；save_accounts 写回标记）
        let imap_password = keychain::resolve_password(cfg.imap.password.clone(), "IMAP 密码")
            .map_err(StartError::Backend)?;
        let smtp_password = keychain::resolve_password(
            cfg.smtp.as_ref().and_then(|s| s.password.clone()),
            "SMTP 密码",
        )
        .map_err(StartError::Backend)?;
        let cfg = AppConfig {
            imap: ImapConfig {
                password: imap_password,
                ..cfg.imap.clone()
            },
            smtp: cfg.smtp.as_ref().map(|s| SmtpConfig {
                password: smtp_password,
                ..s.clone()
            }),
            oauth: cfg.oauth.clone(),
            rules: cfg.rules.clone(),
            pgp: cfg.pgp.clone(),
            identity: cfg.identity.clone(),
        };
        std::fs::create_dir_all(data_dir)
            .map_err(|e| StartError::Backend(format!("建数据目录失败: {e}")))?;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let data_dir = data_dir.to_path_buf();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = std::thread::spawn(move || {
            ServiceInner {
                cfg,
                data_dir,
                zh: std::cell::Cell::new(true),
                events: event_tx,
                stop: stop_flag,
                folders_ready: std::cell::Cell::new(false),
                pgp_cache: std::sync::Mutex::new(None),
                last_sync: std::cell::RefCell::new(std::collections::HashMap::new()),
                page_limit: std::cell::RefCell::new(std::collections::HashMap::new()),
            }
            .run(cmd_rx);
        });

        Ok((
            Self {
                cmd_tx,
                handle: Some(handle),
                stop,
            },
            event_rx,
        ))
    }

    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 列表单次返回上限（无分页；UI 在满额时提示）
pub const UI_LIST_LIMIT: u32 = 500;
/// 后台索引器每批条数（约 3 秒一批）
const INDEX_BATCH: u32 = 10;
/// 切换文件夹时的重同步阈值：TTL 内信任本地索引，不再全量拉取
///（变更由 IDLE 推送补齐；避免来回切换反复 FETCH 整个文件夹）
const FOLDER_RESYNC_TTL: Duration = Duration::from_secs(60);

struct ServiceInner {
    cfg: AppConfig,
    data_dir: PathBuf,
    /// UI 语言（zh=true；状态/错误文案跟随，SetLang 运行时切换）
    zh: std::cell::Cell<bool>,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 文件夹映射已就绪（apply_rules 首次需要；之后避免冗余 LIST 往返）。
    /// 配置重载会重建 ServiceInner，无需手动失效
    folders_ready: std::cell::Cell<bool>,
    /// PGP 运行时缓存（密钥环解析贵：盘读+证书包解析；配置重载重建本结构即失效）
    pgp_cache: std::sync::Mutex<Option<std::sync::Arc<pgp::PgpRuntime>>>,
    /// 文件夹最近一次成功同步时刻（切换文件夹的 TTL 判据；RefCell：服务线程单线程独占）
    last_sync: std::cell::RefCell<std::collections::HashMap<String, Instant>>,
    /// 每文件夹当前列表上限（触底翻页逐步扩大；同步/本地先行都用它，避免同步把已翻页列表缩回 500）
    page_limit: std::cell::RefCell<std::collections::HashMap<String, u32>>,
}

enum Conn {
    Up {
        backend: Box<dyn MailBackend>,
        watch_rx: std::sync::mpsc::Receiver<ServerEvent>,
    },
    Down {
        attempts: u32,
        retry_at: Instant,
    },
}

impl ServiceInner {
    fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
    }

    /// 文件夹当前列表上限（默认 UI_LIST_LIMIT；LoadMore 逐步扩大）
    fn folder_limit(&self, folder: &str) -> u32 {
        self.page_limit
            .borrow()
            .get(folder)
            .copied()
            .unwrap_or(UI_LIST_LIMIT)
    }

    /// 状态文案双语：tr 常量串 / trf 位置参数（{} 依序填充）
    fn tr(&self, zh: &'static str, en: &'static str) -> String {
        tr_str(self.zh.get(), zh, en)
    }
    fn trf(&self, zh: &'static str, en: &'static str, args: &[&dyn std::fmt::Display]) -> String {
        trf_str(self.zh.get(), zh, en, args)
    }

    /// PGP 运行时（带缓存）：密钥环盘读+证书解析贵，每封重载浪费；
    /// 未配置返回 Ok(None)。加载失败不缓存（下次重试）
    fn pgp_runtime(&self) -> Result<Option<std::sync::Arc<pgp::PgpRuntime>>, String> {
        let Some(cfg) = &self.cfg.pgp else {
            return Ok(None);
        };
        // unwrap_or_else(into_inner)：锁毒化（持锁 panic）后仍能取回缓存继续服务
        let mut slot = self.pgp_cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(rt) = slot.as_ref() {
            return Ok(Some(rt.clone()));
        }
        let rt = std::sync::Arc::new(pgp::PgpRuntime::load(cfg)?);
        *slot = Some(rt.clone());
        Ok(Some(rt))
    }

    fn run(self, cmd_rx: Receiver<Cmd>) {
        let store = match Store::open(&self.data_dir.join("index.db")) {
            Ok(s) => s,
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "打开索引失败：{}",
                    "Failed to open index: {}",
                    &[&e],
                )));
                return;
            }
        };
        if let Err(e) = std::fs::create_dir_all(self.data_dir.join("bodies")) {
            self.emit(Event::Status(self.trf(
                "建正文目录失败：{}",
                "Failed to create bodies dir: {}",
                &[&e],
            )));
            return;
        }

        // 离线优先：先广播本地文件夹（连接失败/无网时侧栏与缓存邮件照常可用）
        self.emit_folders_local(&store);
        // 空间回收：空闲页 > 64MB 才 VACUUM（首屏已出，代价一次性且罕见）
        if store.maybe_vacuum(64 * 1024 * 1024) {
            self.emit(Event::Status(self.tr(
                "本地索引已压缩（空间回收）",
                "Local index compacted (space reclaimed)",
            )));
        }

        let mut current_folder: Option<String> = None;
        // 延迟同步目标（SelectFolder 标记；主循环 drain 完命令后执行，
        // 避免每个文件夹切换的 SELECT+FETCH 阻塞后续命令处理）
        let mut pending_sync: Option<String> = None;
        let mut conn = self.connect_initial();

        // 初次连接成功。顺序影响体感：LIST（最快）先行让侧栏立即可交互，
        // 再回放离线标记 → 同步 INBOX → 最后草稿对账（可延后不阻塞主流程）
        if let Conn::Up { backend, .. } = &mut conn {
            self.emit_folders(backend.as_mut(), &store);
            self.push_pending_flags(backend.as_mut(), &store);
            self.sync_folder(backend.as_mut(), &store, "INBOX");
            self.sync_drafts(backend.as_mut(), &store);
        }

        let mut tick: u32 = 0;
        loop {
            let timeout = match &conn {
                Conn::Down { retry_at, .. } => {
                    let now = Instant::now();
                    if *retry_at > now {
                        (*retry_at - now).min(Duration::from_millis(200))
                    } else {
                        Duration::from_millis(0)
                    }
                }
                Conn::Up { .. } => Duration::from_millis(200),
            };
            match cmd_rx.recv_timeout(timeout) {
                Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(cmd) => {
                    if self.handle_cmd(
                        cmd,
                        &mut conn,
                        &store,
                        &mut current_folder,
                        &mut pending_sync,
                    ) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }

            // index_pass 让路交还的命令（索引批进行中到达的用户命令）在这里处理
            let mut deferred: Option<Cmd> = None;
            match &mut conn {
                Conn::Up { backend, watch_rx } => {
                    // 集合去重逐个消费：旧写法 changed=Some(folder) 只留最后一个，
                    // 多文件夹同时变化时前面的同步波静默丢弃且 IDLE 不会再补发
                    let mut changed: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    let mut lost: Option<String> = None;
                    while let Ok(ev) = watch_rx.try_recv() {
                        match ev {
                            ServerEvent::FolderChanged { folder } => {
                                changed.insert(folder);
                            }
                            ServerEvent::ConnectionLost { reason } => lost = Some(reason),
                        }
                    }
                    if let Some(reason) = lost {
                        self.emit(Event::Status(self.trf(
                            "连接断开：{}，开始重连…",
                            "Connection lost: {}, reconnecting…",
                            &[&reason],
                        )));
                        self.emit(Event::ConnState(false));
                        conn = Conn::Down {
                            attempts: 0,
                            retry_at: Instant::now(),
                        };
                        continue;
                    }
                    // IDLE 多文件夹变更：一次同步一个（旧版串行同步全部，
                    // 3 个文件夹变更 = 3×SELECT+FETCH 全程阻塞命令循环）
                    if let Some(folder) = changed.iter().next() {
                        let f = folder.clone();
                        changed.remove(&f);
                        self.sync_folder(backend.as_mut(), &store, &f);
                        // Drafts 变更 → 草稿对账
                        if let Some(df) = backend.drafts_folder_name() {
                            if f == df {
                                self.sync_drafts(backend.as_mut(), &store);
                            }
                        }
                        // 本地计数刷新（无 LIST 往返；LIST 只留结构性变化）
                        self.emit_folder_counts(&store);
                    }
                    tick = tick.wrapping_add(1);
                    // oauth：token 临期前主动走重连路径刷新（tick 200ms → 约每 60s 查一次）
                    if self.cfg.oauth.is_some()
                        && tick.is_multiple_of(300)
                        && oauth::load_tokens(&self.data_dir)
                            .map(|ts| ts.expires_at - oauth::unix_now() < 180)
                            .unwrap_or(false)
                    {
                        self.emit(Event::Status(self.tr(
                            "token 临期，重建连接刷新…",
                            "token near expiry, rebuilding connection…",
                        )));
                        self.emit(Event::ConnState(false));
                        conn = Conn::Down {
                            attempts: 0,
                            retry_at: Instant::now(),
                        };
                        continue;
                    }
                    if tick.is_multiple_of(30) {
                        self.flush_outbox(backend.as_mut(), &store);
                    }
                    if tick.is_multiple_of(15) {
                        deferred = self.index_pass(backend.as_mut(), &store, &cmd_rx);
                    }
                }
                Conn::Down { attempts, retry_at } => {
                    if Instant::now() < *retry_at {
                        continue;
                    }
                    self.emit(Event::Status(self.trf(
                        "重连中（第 {} 次）…",
                        "Reconnecting (attempt {})…",
                        &[&(*attempts + 1)],
                    )));
                    match self.connect_backend() {
                        Ok(mut backend) => {
                            let watch_rx = match backend.watch() {
                                Ok(rx) => rx,
                                Err(e) => {
                                    self.emit(Event::Status(self.trf(
                                        "恢复 IDLE 失败：{}",
                                        "Failed to resume IDLE: {}",
                                        &[&e],
                                    )));
                                    *attempts += 1;
                                    *retry_at = Instant::now() + backoff(*attempts);
                                    continue;
                                }
                            };
                            self.emit(Event::Status(self.tr("已重连", "Reconnected")));
                            // 先回放离线标记再同步：本地意图先落地，同步 diff 才不会反向覆盖
                            self.emit(Event::ConnState(true));
                            self.push_pending_flags(backend.as_mut(), &store);
                            // 断网期间其他设备可能改过草稿：重连即对账
                            self.sync_drafts(backend.as_mut(), &store);
                            if let Some(folder) = &current_folder {
                                let f = folder.clone();
                                self.sync_folder(backend.as_mut(), &store, &f);
                            }
                            self.emit_folders(backend.as_mut(), &store);
                            self.flush_outbox(backend.as_mut(), &store);
                            conn = Conn::Up { backend, watch_rx };
                        }
                        Err(_) => {
                            *attempts += 1;
                            *retry_at = Instant::now() + backoff(*attempts);
                        }
                    }
                }
            }
            if let Some(cmd) = deferred {
                if self.handle_cmd(
                    cmd,
                    &mut conn,
                    &store,
                    &mut current_folder,
                    &mut pending_sync,
                ) {
                    return;
                }
            }
            // 延迟同步：命令队列已 drain，现在执行最后一次请求的文件夹同步
            //（用户快速切 A→B→C 时只同步 C，不排队 A/B 的 SELECT+FETCH）
            // 断连时保留 pending_sync（重连后同步当前文件夹路径会覆盖；
            // 旧版 take() 消费后 Conn::Down 跳过 = 标记静默丢失）
            if pending_sync.is_some() {
                if let Conn::Up { backend, .. } = &mut conn {
                    let folder = pending_sync.take().unwrap();
                    self.sync_folder(backend.as_mut(), &store, &folder);
                    self.emit_folder_counts(&store);
                }
            }
        }
    }

    /// oauth 账号：解析/刷新 token 后建后端（重连与初连共用，token 每次重建都最新）
    fn connect_backend(&self) -> Result<Box<dyn MailBackend>, String> {
        let mut imap = self.cfg.imap.clone();
        if let Some(o) = &self.cfg.oauth {
            if let Err(e) = oauth::ensure_token(o, &self.data_dir) {
                if !e.contains("尚未授权") {
                    return Err(e);
                }
                // M6：首次使用在服务线程现场走设备流（阻塞本账号线程，事件回 GUI）
                let tx_events = self.events.clone();
                let data_dir = self.data_dir.clone();
                let oc = o.clone();
                let shown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let shown2 = shown.clone();
                let stop_flag3 = self.stop.clone();
                let res = oauth::authorize_check(
                    &oc,
                    &data_dir,
                    false,
                    &mut move |da| {
                        if !shown2.swap(true, std::sync::atomic::Ordering::SeqCst) {
                            let _ = tx_events.send(Event::OAuthPrompt {
                                url: da.verification_url.clone(),
                                code: da.user_code.clone(),
                            });
                        }
                    },
                    &move || stop_flag3.load(std::sync::atomic::Ordering::SeqCst),
                );
                res.map_err(|e| format!("设备流授权失败：{e}"))?;
            }
            let tok = oauth::ensure_token(o, &self.data_dir)?;
            imap.password = Some(oauth::sasl_b64(&imap.username, &tok));
            imap.use_oauth2 = true;
        }
        connect(&imap)
            .map(|(b, _)| Box::new(b) as Box<dyn MailBackend>)
            .map_err(|e| e.to_string())
    }

    fn connect_initial(&self) -> Conn {
        self.emit(Event::Status(
            self.tr("连接服务器…", "Connecting to server…"),
        ));
        match self.connect_backend() {
            Ok(mut backend) => {
                let watch_rx = backend.watch().ok();
                match watch_rx {
                    Some(rx) => {
                        self.emit(Event::ConnState(true));
                        Conn::Up {
                            backend,
                            watch_rx: rx,
                        }
                    }
                    None => {
                        self.emit(Event::Status(
                            self.tr("启动 IDLE 失败", "Failed to start IDLE"),
                        ));
                        Conn::Down {
                            attempts: 0,
                            retry_at: Instant::now(),
                        }
                    }
                }
            }
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "连接失败：{}",
                    "Connection failed: {}",
                    &[&e],
                )));
                self.emit(Event::ConnState(false));
                Conn::Down {
                    attempts: 0,
                    retry_at: Instant::now() + Duration::from_secs(2),
                }
            }
        }
    }

    fn handle_cmd(
        &self,
        cmd: Cmd,
        conn: &mut Conn,
        store: &Store,
        current_folder: &mut Option<String>,
        pending_sync: &mut Option<String>,
    ) -> bool {
        match cmd {
            Cmd::Shutdown => return true,
            Cmd::SetLang(zh) => self.zh.set(zh),
            Cmd::SelectFolder(folder) => {
                *current_folder = Some(folder.clone());
                // 本地先行（离线优先）：索引即出列表，服务端真相随后经 FolderLoaded 修正。
                // 旧路径每次切换都等全量 FETCH 完成 → 大文件夹下体感明显卡
                let rows = store
                    .list_folder(&folder, self.folder_limit(&folder))
                    .unwrap_or_default();
                if !rows.is_empty() {
                    self.emit(Event::FolderLoaded {
                        folder: folder.clone(),
                        rows,
                    });
                }
                // TTL 内不重复全量拉（IDLE 会推送变更，切回就重拉是冗余网络往返）
                let fresh = match self.last_sync.borrow().get(&folder) {
                    Some(t) => t.elapsed() > FOLDER_RESYNC_TTL,
                    None => true,
                };
                if fresh {
                    // 延迟同步：不阻塞命令循环（用户快速切文件夹时每个
                    // SELECT+FETCH 2-5s 串行排队 → 主循环 drain 完后只同步最终选中的）
                    *pending_sync = Some(folder);
                }
            }
            Cmd::ForceRefresh(folder) => {
                self.emit(Event::Status(self.trf(
                    "正在刷新 {}…",
                    "Refreshing {}…",
                    &[&folder],
                )));
                // 清 TTL 再走常规同步（sync_folder 会在成功后重新计时）
                self.last_sync.borrow_mut().remove(&folder);
                *pending_sync = Some(folder);
            }
            Cmd::LoadMore(folder) => {
                // 触底翻页：上限 +500 后本地索引整体重发（FolderLoaded 全量替换语义）。
                // 不碰服务器：新邮件由 IDLE 推送，这里只并已有索引
                let limit = {
                    let mut pl = self.page_limit.borrow_mut();
                    let e = pl.entry(folder.clone()).or_insert(UI_LIST_LIMIT);
                    *e = e.saturating_add(UI_LIST_LIMIT);
                    *e
                };
                let rows = store.list_folder(&folder, limit).unwrap_or_default();
                self.emit(Event::FolderLoaded { folder, rows });
            }
            Cmd::OpenMessage(hash) => {
                if let Conn::Up { backend, .. } = &mut *conn {
                    self.open_message(hash, backend.as_mut(), store);
                }
            }
            Cmd::SendMail {
                to,
                cc,
                bcc,
                subject,
                body,
                attachments,
                sign,
                encrypt,
            } => {
                let id = store
                    .enqueue_outbox(&to, &cc, &bcc, &subject, &body, &attachments)
                    .unwrap_or(-1);
                // 入队即广播计数：离线发送也要立刻看到「发件箱」入口
                //（此前只在连接正常时的 flush 轮询里报——断网发送=石沉大海）
                if let Ok((p, f)) = store.outbox_counts() {
                    self.emit(Event::OutboxPending {
                        pending: p,
                        failed: f,
                    });
                }
                // 凭据组装失败（缺 smtp 段/oauth 失败）：留 pending 由 flush 轮重试
                match self.smtp_sender() {
                    Ok(sender) => {
                        if let Conn::Up { backend, .. } = &mut *conn {
                            self.try_send(
                                backend.as_mut(),
                                store,
                                id,
                                &to,
                                &cc,
                                &bcc,
                                &subject,
                                &body,
                                &attachments,
                                match (sign, encrypt) {
                                    (_, true) => Some(pgp::PgpSendMode::EncryptSign),
                                    (true, false) => Some(pgp::PgpSendMode::Sign),
                                    _ => None,
                                },
                                &sender,
                            );
                        }
                    }
                    Err(e) => {
                        store.outbox_mark(id, "pending", &e).ok();
                        self.emit(Event::Error(e));
                    }
                }
            }
            Cmd::SaveDraft {
                id,
                to,
                cc,
                bcc,
                subject,
                body,
            } => {
                let row = DraftRow {
                    id: id.unwrap_or(0),
                    to_addr: to,
                    cc,
                    bcc,
                    subject,
                    body,
                    updated_at: 0,
                };
                match store.save_draft(&row) {
                    Ok(new_id) => {
                        self.emit(Event::DraftSaved { id: new_id });
                        let drafts = store.list_drafts().unwrap_or_default();
                        self.emit(Event::DraftsListed(drafts));
                        self.upload_draft(conn, store, new_id, &row);
                    }
                    Err(e) => self.emit(Event::Status(self.trf(
                        "草稿保存失败：{}",
                        "Failed to save draft: {}",
                        &[&e],
                    ))),
                }
            }
            Cmd::ListOutbox => {
                let rows = store.list_outbox_all().unwrap_or_default();
                self.emit(Event::OutboxListed(rows));
            }
            Cmd::OutboxRetry(id) => {
                store.outbox_reset(id).ok();
                self.emit(Event::Status(
                    self.tr("已重新排队发送…", "Requeued for sending…"),
                ));
                if let Conn::Up { backend, .. } = &mut *conn {
                    self.flush_outbox(backend.as_mut(), store);
                }
            }
            Cmd::EmptyTrash(folder) => {
                // 复用批量永久删除管线（已在废纸篓内，语义=真删）
                let hashes = store.hashes_in_folder(&folder).unwrap_or_default();
                if hashes.is_empty() {
                    self.emit(Event::Status(
                        self.tr("废纸篓已是空的", "Trash is already empty"),
                    ));
                } else {
                    self.emit(Event::Status(self.trf(
                        "正在清空废纸篓（{} 封）…",
                        "Emptying Trash ({} message(s))…",
                        &[&hashes.len()],
                    )));
                    self.batch_delete(conn, store, &hashes);
                }
            }
            Cmd::OutboxRemove(id) => {
                store.outbox_remove(id).ok();
                self.emit(Event::Status(
                    self.tr("已从发件箱删除", "Removed from outbox"),
                ));
                let rows = store.list_outbox_all().unwrap_or_default();
                self.emit(Event::OutboxListed(rows));
            }
            Cmd::ListDrafts => {
                // 双向同步先对账再列（打开草稿箱即拉最新；失败不阻塞本地列表）
                if let Conn::Up { backend, .. } = &mut *conn {
                    self.sync_drafts(backend.as_mut(), store);
                }
                let drafts = store.list_drafts().unwrap_or_default();
                self.emit(Event::DraftsListed(drafts));
            }
            Cmd::DeleteDraft(id) => {
                // B2：先取服务器副本的 Message-ID 再删本地（删完关联就丢了）
                let server_mid = store.draft_message_id(id).ok().flatten();
                let _ = store.delete_draft(id);
                self.emit(Event::DraftDeleted(id));
                let drafts = store.list_drafts().unwrap_or_default();
                self.emit(Event::DraftsListed(drafts));
                if let Some(mid) = server_mid {
                    if let Conn::Up { backend, .. } = &mut *conn {
                        // best-effort：删不掉（离线/找不到）不阻本地删除，残留的服务器
                        // 副本下次保存同名草稿时不会误删（Message-ID 每次新生成）
                        match backend.as_mut().delete_draft_by_mid(&mid) {
                            Ok(true) => {}
                            Ok(false) => {}
                            Err(e) => self.emit(Event::Status(self.trf(
                                "服务器草稿清理失败：{}",
                                "Failed to clean server drafts: {}",
                                &[&e],
                            ))),
                        }
                    }
                }
            }
            Cmd::MarkSeen(hash, seen) => {
                store.set_seen(hash, seen).ok();
                self.emit(Event::FlagsChanged {
                    hash,
                    seen: Some(seen),
                    flagged: None,
                });
                match (&mut *conn, store.get(hash).ok().flatten()) {
                    (Conn::Up { backend, .. }, Some(row)) => {
                        // 服务器失败/未连接 → 入回放队列（重连后推回，不再静默丢）
                        if backend
                            .as_mut()
                            .set_flags(&row.folder, &[hash as u64], FlagChange::Seen(seen))
                            .is_err()
                        {
                            store.queue_flag(hash, Some(seen), None).ok();
                        }
                    }
                    _ => {
                        store.queue_flag(hash, Some(seen), None).ok();
                    }
                }
            }
            Cmd::ToggleFlagged(hash, on) => {
                store.set_flagged(hash, on).ok();
                self.emit(Event::FlagsChanged {
                    hash,
                    seen: None,
                    flagged: Some(on),
                });
                match (&mut *conn, store.get(hash).ok().flatten()) {
                    (Conn::Up { backend, .. }, Some(row)) => {
                        if backend
                            .as_mut()
                            .set_flags(&row.folder, &[hash as u64], FlagChange::Flagged(on))
                            .is_err()
                        {
                            store.queue_flag(hash, None, Some(on)).ok();
                        }
                    }
                    _ => {
                        store.queue_flag(hash, None, Some(on)).ok();
                    }
                }
            }
            Cmd::DeleteMessage(hash) => {
                // Foxmail 式删除语义：普通文件夹 → 移废纸篓（可恢复）；
                // 在废纸篓里删除 → 服务器永久删除（EXPUNGE）
                let Some(row) = store.get(hash).ok().flatten() else {
                    return false;
                };
                if smail_backend::folder_kind(&row.folder) == smail_backend::FolderKind::Trash {
                    if let Conn::Up { backend, .. } = &mut *conn {
                        let _ = backend
                            .as_mut()
                            .delete_messages(&row.folder, &[hash as u64]);
                    }
                    store.delete_message(hash).ok();
                    let _ = std::fs::remove_file(self.body_path(hash));
                    self.emit(Event::MessageDeleted(hash));
                } else {
                    let trash = match &mut *conn {
                        Conn::Up { backend, .. } => backend.as_mut().ensure_trash_folder(),
                        Conn::Down { .. } => {
                            self.emit(Event::Status(
                                self.tr("未连接，稍后重试", "Not connected, will retry later"),
                            ));
                            return false;
                        }
                    };
                    match trash {
                        Ok(dest) => {
                            self.emit(Event::Status(self.trf(
                                "移到废纸篓（可恢复）…",
                                "Moving to Trash (recoverable)…",
                                &[],
                            )));
                            self.move_messages_cmd(conn, store, &[hash], &dest);
                            // Trash 可能刚被 ensure_trash_folder 创建——本地无数据，
                            // emit_folder_counts 看不见它；必须 LIST 让侧栏知道新文件夹
                            if let Conn::Up { backend, .. } = &mut *conn {
                                self.emit_folders(backend.as_mut(), store);
                            }
                            // 移动成功=源视图本地行已清（消费方按“已删”语义处理，如 m2 e2e）
                            if store.get(hash).ok().flatten().is_none() {
                                self.emit(Event::MessageDeleted(hash));
                            }
                        }
                        Err(e) => {
                            // 服务器不支持建废纸篓：退回永久删除（确认条已确认过）
                            self.emit(Event::Status(self.trf(
                                "废纸篓不可用（{}），改为永久删除",
                                "Trash unavailable ({}), deleting permanently",
                                &[&e],
                            )));
                            if let Conn::Up { backend, .. } = &mut *conn {
                                let _ = backend
                                    .as_mut()
                                    .delete_messages(&row.folder, &[hash as u64]);
                            }
                            store.delete_message(hash).ok();
                            let _ = std::fs::remove_file(self.body_path(hash));
                            self.emit(Event::MessageDeleted(hash));
                        }
                    }
                }
            }
            Cmd::Search(query) => {
                let results = store.search_with_snippets(&query, 200).unwrap_or_default();
                let rows = results.iter().map(|(r, _)| r.clone()).collect();
                let snippets = results.into_iter().map(|(_, s)| s).collect();
                self.emit(Event::SearchResults {
                    query,
                    rows,
                    snippets,
                });
            }
            Cmd::SaveAttachment { hash, name } => {
                self.save_attachment(hash, name);
            }
            // ---- M7 ----
            Cmd::MoveMessage { hash, dest } => {
                self.move_messages_cmd(conn, store, &[hash], &dest);
            }
            Cmd::BatchFlags {
                hashes,
                seen,
                flagged,
            } => self.batch_flags(conn, store, &hashes, seen, flagged),
            Cmd::BatchMove { hashes, dest } => {
                self.move_messages_cmd(conn, store, &hashes, &dest);
            }
            Cmd::BatchDelete { hashes } => self.batch_delete(conn, store, &hashes),
            Cmd::CreateFolder(path) => {
                if let Conn::Up { backend, .. } = &mut *conn {
                    match backend.create_folder(&path) {
                        Ok(()) => {
                            self.emit(Event::Status(self.trf(
                                "已创建文件夹 {}",
                                "Created folder {}",
                                &[&path],
                            )));
                            self.emit_folders(backend.as_mut(), store);
                        }
                        Err(e) => self.emit(Event::Status(self.trf(
                            "创建文件夹失败：{}",
                            "Failed to create folder: {}",
                            &[&e],
                        ))),
                    }
                }
            }
            Cmd::DeleteFolder(path) => {
                if path.eq_ignore_ascii_case("INBOX") {
                    self.emit(Event::Status(
                        self.tr("不能删除 INBOX", "Cannot delete INBOX"),
                    ));
                    return false;
                }
                if let Conn::Up { backend, .. } = &mut *conn {
                    match backend.delete_folder(&path) {
                        Ok(()) => {
                            // 本地记录与正文缓存一并清理（hash 先取再删）
                            for h in store.hashes_in_folder(&path).unwrap_or_default() {
                                let _ = std::fs::remove_file(self.body_path(h));
                            }
                            store.delete_folder_rows(&path).ok();
                            self.emit(Event::Status(self.trf(
                                "已删除文件夹 {}",
                                "Deleted folder {}",
                                &[&path],
                            )));
                            self.emit_folders(backend.as_mut(), store);
                        }
                        Err(e) => self.emit(Event::Status(self.trf(
                            "删除文件夹失败：{}",
                            "Failed to delete folder: {}",
                            &[&e],
                        ))),
                    }
                }
            }
            Cmd::RenameFolder(old, new) => {
                if let Conn::Up { backend, .. } = &mut *conn {
                    match backend.rename_folder(&old, &new) {
                        Ok(()) => {
                            store.rename_folder_rows(&old, &new).ok();
                            self.emit(Event::Status(self.trf(
                                "已改名 {} → {}",
                                "Renamed {} → {}",
                                &[&old, &new],
                            )));
                            self.emit_folders(backend.as_mut(), store);
                        }
                        Err(e) => {
                            self.emit(Event::Status(self.trf(
                                "改名文件夹失败：{}",
                                "Failed to rename folder: {}",
                                &[&e],
                            )));
                            // UI 侧是乐观改名：失败时重发真实文件夹列表拉回真相，
                            // 否则侧栏停在不存在的新名字上点击即报错
                            self.emit_folders(backend.as_mut(), store);
                        }
                    }
                }
            }
            Cmd::Archive(hashes) => {
                let Conn::Up { backend, .. } = &mut *conn else {
                    self.emit(Event::Status(
                        self.tr("未连接，稍后重试", "Not connected, will retry later"),
                    ));
                    return false;
                };
                match backend.as_mut().ensure_archive_folder() {
                    Ok(dest) => {
                        self.emit(Event::Status(self.trf(
                            "归档到 {}…",
                            "Archiving to {}…",
                            &[&dest],
                        )));
                        self.move_messages_cmd(conn, store, &hashes, &dest);
                    }
                    Err(e) => self.emit(Event::Status(self.trf(
                        "归档失败：{}",
                        "Archive failed: {}",
                        &[&e],
                    ))),
                }
            }
            Cmd::ExportEml { hash, path } => {
                let src = self.body_path(hash);
                match std::fs::copy(&src, &path) {
                    Ok(_) => self.emit(Event::Status(self.trf(
                        "已导出到 {}",
                        "Exported to {}",
                        &[&path],
                    ))),
                    Err(e) => self.emit(Event::Status(self.trf(
                        "导出 .eml 失败：{}",
                        "Failed to export .eml: {}",
                        &[&e],
                    ))),
                }
            }
            Cmd::LoadImages { hash, urls } => {
                // 逐个下载（单图 2MB、总量 8MB 上限；失败跳过不计错）
                const MAX_EACH: u64 = 2 * 1024 * 1024;
                const MAX_TOTAL: u64 = 8 * 1024 * 1024;
                let mut images = Vec::new();
                let mut total: u64 = 0;
                for url in &urls {
                    if total >= MAX_TOTAL {
                        break;
                    }
                    match ureq::get(url).timeout(Duration::from_secs(10)).call() {
                        Ok(resp) if resp.status() == 200 => {
                            let mut buf = Vec::new();
                            use std::io::Read as _;
                            if resp
                                .into_reader()
                                .take(MAX_EACH)
                                .read_to_end(&mut buf)
                                .is_ok()
                                && !buf.is_empty()
                            {
                                total += buf.len() as u64;
                                images.push((url.clone(), buf));
                            }
                        }
                        _ => continue,
                    }
                }
                self.emit(Event::ImagesLoaded { hash, images });
            }
            Cmd::LocalList(folder) => {
                let rows = store
                    .list_folder(&folder, self.folder_limit(&folder))
                    .unwrap_or_default();
                self.emit(Event::UnifiedRows { rows });
            }
            Cmd::Spam(hashes) => {
                let Conn::Up { backend, .. } = &mut *conn else {
                    self.emit(Event::Status(
                        self.tr("未连接，稍后重试", "Not connected, will retry later"),
                    ));
                    return false;
                };
                match backend.as_mut().ensure_junk_folder() {
                    Ok(dest) => {
                        self.emit(Event::Status(self.trf(
                            "移到 {}（标为垃圾）…",
                            "Moving to {} (mark as junk)…",
                            &[&dest],
                        )));
                        self.move_messages_cmd(conn, store, &hashes, &dest);
                    }
                    Err(e) => self.emit(Event::Status(self.trf(
                        "标为垃圾失败：{}",
                        "Failed to mark as junk: {}",
                        &[&e],
                    ))),
                }
            }
            Cmd::ListContactsFull => {
                let rows = store.list_contacts_full().unwrap_or_default();
                self.emit(Event::ContactsFull(rows));
            }
            Cmd::DeleteContact(id) => {
                store.delete_contact(id).ok();
                let rows = store.list_contacts_full().unwrap_or_default();
                self.emit(Event::ContactsFull(rows));
            }
            Cmd::ListContacts => {
                let addrs = store.list_contacts_display(30).unwrap_or_default();
                self.emit(Event::ContactsListed(addrs));
            }
        }
        false
    }

    // ---------------- 同步 ----------------

    /// B2 草稿云同步（v1 单向）：本地保存成功后上传服务器 Drafts。
    /// 编辑已上传草稿：先传新副本（新 Message-ID）再删旧副本——append 先行，
    /// 失败最多残留旧版不丢内容。离线/失败：仅本地，下次在线保存时上传
    fn upload_draft(&self, conn: &mut Conn, store: &Store, draft_id: i64, row: &DraftRow) {
        let Conn::Up { backend, .. } = &mut *conn else {
            return; // 离线：仅本地，下次在线保存时上传
        };
        let backend = backend.as_mut();
        let old_mid = store.draft_message_id(draft_id).ok().flatten();
        let new_mid = format!(
            "<s-mail-draft-{}.{}@{}>",
            draft_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            self.cfg.imap.username
        );
        let mail = OutgoingMail {
            from: self.cfg.identity.clone(),
            to: split_addrs(&row.to_addr),
            cc: split_addrs(&row.cc),
            bcc: split_addrs(&row.bcc),
            subject: row.subject.clone(),
            body: row.body.clone(),
            attachments: vec![],
        };
        let bytes = match smail_backend::build_draft_bytes(&mail, &new_mid) {
            Ok(b) => b,
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "草稿上传跳过：{}",
                    "Draft upload skipped: {}",
                    &[&e],
                )));
                return;
            }
        };
        match backend.append_draft(bytes) {
            Ok(folder) => {
                store.set_draft_message_id(draft_id, &new_mid).ok();
                if let Some(old) = old_mid {
                    let _ = backend.delete_draft_by_mid(&old);
                }
                self.emit(Event::Status(self.trf(
                    "草稿已同步到服务器 {}",
                    "Draft synced to server {}",
                    &[&folder],
                )));
            }
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "草稿仅保存在本地（上传失败：{}）",
                    "Draft kept local only (upload failed: {})",
                    &[&e],
                )));
            }
        }
    }

    /// 离线标记回放（v4 队列）：重连/初连后先把本地改动推回服务器，成功一项清一项；
    /// 失败保留队列下轮重试（意图语义：后到覆盖，最终收敛）。
    /// 必须在 sync_folder 之前调：本地意图先落地，同步 diff 才不会把本地改动反向覆盖
    fn push_pending_flags(&self, backend: &mut dyn MailBackend, store: &Store) {
        let pending = store.pending_flags().unwrap_or_default();
        if pending.is_empty() {
            return;
        }
        self.emit(Event::Status(self.trf(
            "回放离线标记（{} 项）…",
            "Replaying offline flag changes ({})…",
            &[&pending.len()],
        )));
        let mut done: Vec<i64> = Vec::new();
        for (hash, seen, flagged) in pending {
            let Some(row) = store.get(hash).ok().flatten() else {
                done.push(hash); // 本地行已不在（删/移）：意图作废直接清
                continue;
            };
            let mut ok = true;
            if let Some(v) = seen {
                ok &= backend
                    .set_flags(&row.folder, &[hash as u64], FlagChange::Seen(v))
                    .is_ok();
            }
            if let Some(v) = flagged {
                ok &= backend
                    .set_flags(&row.folder, &[hash as u64], FlagChange::Flagged(v))
                    .is_ok();
            }
            if ok {
                done.push(hash);
            }
        }
        if !done.is_empty() {
            store.remove_pending_flags(&done).ok();
        }
    }

    /// 草稿双向同步（v2）：以 Message-ID 为身份键对服务器 Drafts 对账。
    /// 服务器有/本地无 → 拉回正文导入（含我们前缀的孤儿也导入：DB 重置场景内容优先，
    ///   不自动删服务器——删错了丢用户数据）；
    /// 本地已关联但服务器无 → 对端已删 → 删本地；同 ID 冲突保留本地
    /// （IMAP 客户端改草稿 = 新 APPEND 新 Message-ID，天然收敛）。
    /// 空 Message-ID 的服务器草稿无法做身份对账，跳过（不导入不删除）
    fn sync_drafts(&self, backend: &mut dyn MailBackend, store: &Store) {
        let Some(folder) = backend.drafts_folder_name() else {
            return;
        };
        let envs = match backend.fetch_envelopes(&folder) {
            Ok(e) => e,
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "草稿对账失败：{}",
                    "Draft reconciliation failed: {}",
                    &[&e],
                )));
                return;
            }
        };
        let norm = |s: &str| {
            s.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        };
        let local = store.all_draft_mids().unwrap_or_default();
        let local_mids: std::collections::HashSet<String> =
            local.iter().map(|(_, m)| norm(m)).collect();
        let mut imported = 0usize;
        for env in &envs {
            let mid = norm(&env.message_id);
            if mid.is_empty() || local_mids.contains(&mid) {
                continue;
            }
            let bytes = match backend.fetch_body(env.hash) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let (body, _) = extract_body(&bytes);
            let (to, cc) = extract_recipients(&bytes);
            let row = DraftRow {
                id: 0,
                to_addr: to,
                cc,
                bcc: String::new(),
                subject: env.subject.clone(),
                body,
                updated_at: 0,
            };
            match store.save_draft(&row) {
                Ok(new_id) => {
                    store.set_draft_message_id(new_id, &env.message_id).ok();
                    imported += 1;
                }
                Err(e) => self.emit(Event::Status(self.trf(
                    "导入服务器草稿失败：{}",
                    "Failed to import server drafts: {}",
                    &[&e],
                ))),
            }
        }
        let server_mids: std::collections::HashSet<String> =
            envs.iter().map(|e| norm(&e.message_id)).collect();
        let mut removed = 0usize;
        for (id, mid) in &local {
            if !server_mids.contains(&norm(mid)) {
                store.delete_draft(*id).ok();
                removed += 1;
            }
        }
        if imported + removed > 0 {
            self.emit(Event::Status(self.trf(
                "草稿对账：导入 {}、随服务器删除 {}",
                "Drafts reconciled: {} imported, {} removed per server",
                &[&imported, &removed],
            )));
        }
    }

    fn sync_folder(&self, backend: &mut dyn MailBackend, store: &Store, folder: &str) {
        self.emit(Event::Status(self.trf(
            "正在同步 {}…",
            "Syncing {}…",
            &[&folder],
        )));
        let envelopes = match backend.fetch_envelopes(folder) {
            Ok(e) => e,
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "同步 {} 失败：{}",
                    "Failed to sync {}: {}",
                    &[&folder, &e],
                )));
                return;
            }
        };
        let known = store.known_flags(folder).unwrap_or_default();
        let mut new_rows: Vec<MessageRow> = Vec::new();
        // 服务器 flags 回传（A1 修复）：旧路径对已知 hash 直接跳过，手机上读掉的
        // 邮件在桌面端永远未读。离线期间的本地改动已由 pending 队列先行推回（重连时），
        // 此处差异一律果服务器真相（push 失败的项仍在队列，下轮收敛）
        let mut flag_updates: Vec<(i64, bool, bool)> = Vec::new();
        for env in &envelopes {
            match known.get(&(env.hash as i64)) {
                None => new_rows.push(to_row(env, folder)),
                Some(&(seen, flagged)) => {
                    if seen != env.seen || flagged != env.flagged {
                        flag_updates.push((env.hash as i64, env.seen, env.flagged));
                    }
                }
            }
        }
        let new_count = new_rows.len();
        // 单事务批量落库（含信封级 FTS；旧路径逐条隐式事务，首次同步万封级明显放大 I/O）
        if !new_rows.is_empty() {
            if let Err(e) = store.upsert_new_batch(&new_rows) {
                self.emit(Event::Status(self.trf(
                    "写入 {} 索引失败：{}",
                    "Failed to write index for {}: {}",
                    &[&folder, &e],
                )));
            }
            // 通讯录自动收集：新邮件发件人入 contacts（单事务批量）
            let contacts: Vec<(String, String)> = new_rows
                .iter()
                .map(|r| parse_from_addr(&r.from_addr))
                .filter(|(email, _)| email.contains('@'))
                .collect();
            if !contacts.is_empty() {
                store.upsert_contacts_batch(&contacts).ok();
            }
        }
        if !flag_updates.is_empty() {
            let n = flag_updates.len();
            if store.apply_flags_batch(&flag_updates).is_ok() {
                self.emit(Event::Status(self.trf(
                    "{}：{} 封标记已随服务器更新",
                    "{}: {} flag(s) updated from server",
                    &[&folder, &n],
                )));
            }
        }
        // M7：INBOX 增量新信 → 系统通知（初次全量同步 known 为空，不打扰）
        if !known.is_empty() && folder.eq_ignore_ascii_case("INBOX") {
            for row in new_rows.iter().filter(|r| !r.seen).take(3) {
                self.emit(Event::NewMail {
                    from: row.from_addr.clone(),
                    subject: row.subject.clone(),
                    folder: row.folder.clone(),
                    hash: row.envelope_hash,
                });
            }
        }
        // M5-b：规则只打新邮件（已知邮件不重放）；先于 FolderLoaded 发射，
        // 列表直接呈现规则效果
        if !new_rows.is_empty() && !self.cfg.rules.is_empty() {
            self.apply_rules(backend, store, folder, new_rows);
        }
        // M6 排查：服务端删除同步——本地有而服务端全量里没有的视为已删
        //（服务器真相为准；UIDVALIDITY 变化时全量集合同样收敛正确）
        let fetched: std::collections::HashSet<i64> =
            envelopes.iter().map(|e| e.hash as i64).collect();
        let vanished_hashes: Vec<i64> = known
            .iter()
            .filter(|(k, _)| !fetched.contains(*k))
            .map(|(k, _)| *k)
            .collect();
        if !vanished_hashes.is_empty() {
            if store.delete_messages_batch(&vanished_hashes).is_ok() {
                for k in &vanished_hashes {
                    let _ = std::fs::remove_file(self.body_path(*k));
                }
            }
            self.emit(Event::Status(self.trf(
                "{}：{} 封已从服务器删除，本地同步清理",
                "{}: {} message(s) deleted on server, cleaned locally",
                &[&folder, &vanished_hashes.len()],
            )));
        }
        let rows = store
            .list_folder(folder, self.folder_limit(folder))
            .unwrap_or_default();
        self.last_sync
            .borrow_mut()
            .insert(folder.to_string(), Instant::now());
        self.emit(Event::FolderLoaded {
            folder: folder.to_string(),
            rows,
        });
        self.emit(Event::Status(self.trf(
            "{}：{} 封（新 {}）",
            "{}: {} messages ({} new)",
            &[&folder, &envelopes.len(), &new_count],
        )));
    }

    // ---------------- M7：移动 / 批量操作 ----------------

    /// 移动（单封/批量共用）：按 folder 分组服务器搬运，成功后清本地记录与正文缓存。
    /// UI 侧发命令时已乐观移除列表行；失败仅报错（下次同步自然回真）。
    fn move_messages_cmd(&self, conn: &mut Conn, store: &Store, hashes: &[i64], dest: &str) {
        let Conn::Up { backend, .. } = &mut *conn else {
            self.emit(Event::Status(
                self.tr("未连接，稍后重试", "Not connected, will retry later"),
            ));
            return;
        };
        let backend = backend.as_mut();
        let mut moved = 0usize;
        for (folder, group) in group_by_folder(store, hashes) {
            if folder.eq_ignore_ascii_case(dest) {
                continue; // 目的地=源：跳过，防自移动
            }
            let batch: Vec<u64> = group.iter().map(|h| *h as u64).collect();
            match backend.move_messages(&folder, &batch, dest) {
                Ok(()) => {
                    store.delete_messages_batch(&group).ok();
                    for h in &group {
                        let _ = std::fs::remove_file(self.body_path(*h));
                    }
                    moved += group.len();
                }
                Err(e) => self.emit(Event::Status(self.trf(
                    "移动 {}→{} 失败：{}",
                    "Failed to move {} → {}: {}",
                    &[&folder, &dest, &e],
                ))),
            }
        }
        if moved > 0 {
            self.emit(Event::Status(self.trf(
                "已移动 {} 封到 {}",
                "Moved {} message(s) to {}",
                &[&moved, &dest],
            )));
            // 侧栏计数即时刷新（不等 IDLE；本地计数零网络往返——
            // LIST 只留给结构性变化：建/删/改名文件夹）
            self.emit_folder_counts(store);
        } else if !hashes.is_empty() {
            self.emit(Event::Status(self.tr(
                "没有可移动的邮件（可能已在目标文件夹或本地已删除）",
                "Nothing to move (already in target folder or deleted locally)",
            )));
        }
    }

    fn batch_flags(
        &self,
        conn: &mut Conn,
        store: &Store,
        hashes: &[i64],
        seen: Option<bool>,
        flagged: Option<bool>,
    ) {
        let Conn::Up { backend, .. } = &mut *conn else {
            self.emit(Event::Status(
                self.tr("未连接，稍后重试", "Not connected, will retry later"),
            ));
            return;
        };
        let backend = backend.as_mut();
        let mut done = 0usize;
        for (folder, group) in group_by_folder(store, hashes) {
            let batch: Vec<u64> = group.iter().map(|h| *h as u64).collect();
            if let Some(v) = seen {
                if backend
                    .set_flags(&folder, &batch, FlagChange::Seen(v))
                    .is_ok()
                {
                    for h in &group {
                        store.set_seen(*h, v).ok();
                    }
                } else {
                    // 服务器失败：意图入回放队列（重连后推回）
                    for h in &group {
                        store.queue_flag(*h, Some(v), None).ok();
                    }
                }
            }
            if let Some(v) = flagged {
                if backend
                    .set_flags(&folder, &batch, FlagChange::Flagged(v))
                    .is_ok()
                {
                    for h in &group {
                        store.set_flagged(*h, v).ok();
                    }
                } else {
                    for h in &group {
                        store.queue_flag(*h, None, Some(v)).ok();
                    }
                }
            }
            done += group.len();
            for h in &group {
                self.emit(Event::FlagsChanged {
                    hash: *h,
                    seen,
                    flagged,
                });
            }
        }
        if done > 0 {
            self.emit(Event::Status(self.trf(
                "已更新 {} 封的标记",
                "Updated flags on {} message(s)",
                &[&done],
            )));
            self.emit_folders(backend, store);
        }
    }

    fn batch_delete(&self, conn: &mut Conn, store: &Store, hashes: &[i64]) {
        let Conn::Up { backend, .. } = &mut *conn else {
            self.emit(Event::Status(
                self.tr("未连接，稍后重试", "Not connected, will retry later"),
            ));
            return;
        };
        // 同单封语义：废纸篓内的批量删除=永久；其余=移入废纸篓
        let trash_dest = backend.as_mut().ensure_trash_folder().ok();
        let (perm, to_trash): (Vec<i64>, Vec<i64>) = hashes.iter().partition(|h| {
            store
                .get(**h)
                .ok()
                .flatten()
                .map(|r| smail_backend::folder_kind(&r.folder) == smail_backend::FolderKind::Trash)
                .unwrap_or(false)
        });
        let mut perm = perm;
        if !to_trash.is_empty() {
            match &trash_dest {
                Some(dest) => {
                    let d = dest.clone();
                    self.move_messages_cmd(conn, store, &to_trash, &d);
                    // Trash 可能新建（同上）：LIST 确保侧栏可见
                    if let Conn::Up { backend, .. } = &mut *conn {
                        self.emit_folders(backend.as_mut(), store);
                    }
                    for h in &to_trash {
                        if store.get(*h).ok().flatten().is_none() {
                            self.emit(Event::MessageDeleted(*h));
                        }
                    }
                }
                None => perm.extend(to_trash), // 无废纸篓：退回永久删
            }
        }
        let hashes = &perm;
        let backend = match &mut *conn {
            Conn::Up { backend, .. } => backend.as_mut(),
            Conn::Down { .. } => return,
        };
        let mut deleted = 0usize;
        for (folder, group) in group_by_folder(store, hashes) {
            let batch: Vec<u64> = group.iter().map(|h| *h as u64).collect();
            match backend.delete_messages(&folder, &batch) {
                Ok(()) => {
                    store.delete_messages_batch(&group).ok();
                    for h in &group {
                        let _ = std::fs::remove_file(self.body_path(*h));
                        self.emit(Event::MessageDeleted(*h));
                    }
                    deleted += group.len();
                }
                Err(e) => self.emit(Event::Status(self.trf(
                    "删除 {} 中 {} 封失败：{}",
                    "Failed to delete {} message(s) in {}: {}",
                    &[&folder, &group.len(), &e],
                ))),
            }
        }
        if deleted > 0 {
            self.emit(Event::Status(self.trf(
                "已删除 {} 封",
                "Deleted {} message(s)",
                &[&deleted],
            )));
            self.emit_folder_counts(store);
        }
    }

    /// M5-b：对新邮件匹配规则（首条命中即用），动作按规则分桶后批量执行，
    /// 动作失败发错误事件不中断同步。
    ///（旧版逐封逐 flag 服务器往返：N 封命中 = 2N+ 次 IMAP 往返，批量后每桶空flag/1 次）
    fn apply_rules(
        &self,
        backend: &mut dyn MailBackend,
        store: &Store,
        folder: &str,
        new_rows: Vec<MessageRow>,
    ) {
        // 首次同步时 folders 映射可能还没填（run() 里 sync 先于 emit_folders）；
        // 之后跳过：emit_folders 马上会做，避免每次同步多一次 LIST 往返
        if !self.folders_ready.get() {
            backend.list_folders().ok();
            self.folders_ready.set(true);
        }
        // 逐行找首条命中规则 → 按规则索引分桶（保持“首条命中即用”语义）
        let mut buckets: Vec<(usize, Vec<MessageRow>)> = Vec::new();
        for row in new_rows {
            let Some(ri) = self.cfg.rules.iter().position(|r| {
                r.folder
                    .as_ref()
                    .map(|f| f.eq_ignore_ascii_case(folder))
                    .unwrap_or(true)
                    && r.matches(&row)
            }) else {
                continue;
            };
            match buckets.iter_mut().find(|(b, _)| *b == ri) {
                Some((_, rows)) => rows.push(row),
                None => buckets.push((ri, vec![row])),
            }
        }
        let mut seen_n = 0usize;
        let mut flag_n = 0usize;
        let mut moved_n = 0usize;
        for (ri, rows) in buckets {
            let rule = &self.cfg.rules[ri];
            let hashes: Vec<i64> = rows.iter().map(|r| r.envelope_hash).collect();
            let batch: Vec<u64> = hashes.iter().map(|h| *h as u64).collect();
            if rule.mark_seen {
                match backend.set_flags(folder, &batch, FlagChange::Seen(true)) {
                    Ok(()) => {
                        for h in &hashes {
                            store.set_seen(*h, true).ok();
                        }
                        seen_n += rows.len();
                    }
                    Err(e) => self.emit(Event::Status(self.trf(
                        "规则标已读失败：{}",
                        "Rule mark-read failed: {}",
                        &[&e],
                    ))),
                }
            }
            if rule.flag {
                match backend.set_flags(folder, &batch, FlagChange::Flagged(true)) {
                    Ok(()) => {
                        for h in &hashes {
                            store.set_flagged(*h, true).ok();
                        }
                        flag_n += rows.len();
                    }
                    Err(e) => self.emit(Event::Status(self.trf(
                        "规则星标失败：{}",
                        "Rule star failed: {}",
                        &[&e],
                    ))),
                }
            }
            if let Some(dest) = &rule.move_to {
                if !dest.eq_ignore_ascii_case(folder) {
                    // 目的地=当前文件夹：跳过，防自移动死循环
                    match backend.move_messages(folder, &batch, dest) {
                        Ok(()) => {
                            store.delete_messages_batch(&hashes).ok();
                            moved_n += rows.len();
                        }
                        Err(e) => self.emit(Event::Status(self.trf(
                            "规则移动 {}→{} 失败：{}",
                            "Rule move {} → {} failed: {}",
                            &[&folder, &dest, &e],
                        ))),
                    }
                }
            }
        }
        if seen_n + flag_n + moved_n > 0 {
            self.emit(Event::Status(self.trf(
                "规则：标已读 {}、星标 {}、移动 {}",
                "Rules: {} marked read, {} starred, {} moved",
                &[&seen_n, &flag_n, &moved_n],
            )));
        }
    }

    /// 离线优先：启动即从本地索引广播文件夹清单——侧栏不等服务器。
    /// 连接建立后由服务器真值整体替换（UI 侧 retain+push 语义）。
    /// folders_ready 不置位：它代表"见过服务器真值"。
    fn emit_folders_local(&self, store: &Store) {
        let mut stats = store.folder_stats().unwrap_or_default();
        if stats.is_empty() {
            return; // 全新账号：没有本地数据，等服务器（与旧行为一致）
        }
        stats.sort_by(|a, b| {
            let key = |f: &str| (f != "INBOX", f.to_lowercase());
            key(&a.0).cmp(&key(&b.0))
        });
        let infos = stats
            .into_iter()
            .map(|(path, total, unseen)| FolderInfo {
                kind: smail_backend::folder_kind(&path),
                total,
                unseen,
                path,
            })
            .collect();
        self.emit(Event::Folders(infos));
    }

    /// 本地计数刷新（无网络往返）：sync 后侧栏数字即时更新。
    /// LIST（emit_folders）只留给结构性变化（建/删/改名文件夹、首连、重连）
    fn emit_folder_counts(&self, store: &Store) {
        if !self.folders_ready.get() {
            return;
        }
        let stats = store.folder_stats().unwrap_or_default();
        if stats.is_empty() {
            return;
        }
        let infos = stats
            .into_iter()
            .map(|(path, total, unseen)| FolderInfo {
                kind: smail_backend::folder_kind(&path),
                total,
                unseen,
                path,
            })
            .collect();
        self.emit(Event::Folders(infos));
    }

    fn emit_folders(&self, backend: &mut dyn MailBackend, store: &Store) {
        let names = backend.list_folders().unwrap_or_default();
        self.folders_ready.set(true);
        let infos = names
            .into_iter()
            .map(|path| FolderInfo {
                kind: smail_backend::folder_kind(&path),
                total: store.count_folder(&path).unwrap_or(0),
                unseen: store.count_unseen(&path).unwrap_or(0),
                path,
            })
            .collect();
        self.emit(Event::Folders(infos));
    }

    fn open_message(&self, hash: i64, backend: &mut dyn MailBackend, store: &Store) {
        let Some(row) = store.get(hash).ok().flatten() else {
            self.emit(Event::Status(self.trf(
                "找不到邮件 {}",
                "Message {} not found",
                &[&hash],
            )));
            return;
        };
        let path = self.body_path(hash);
        let bytes = if path.exists() {
            match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    self.emit(Event::Status(self.trf(
                        "读缓存失败：{}",
                        "Failed to read cache: {}",
                        &[&e],
                    )));
                    return;
                }
            }
        } else {
            let bytes = match backend.fetch_body(hash as u64) {
                Ok(b) => b,
                Err(e) => {
                    self.emit(Event::Status(self.trf(
                        "拉取正文失败：{}",
                        "Failed to fetch body: {}",
                        &[&e],
                    )));
                    return;
                }
            };
            if let Err(e) = std::fs::write(&path, &bytes) {
                self.emit(Event::Status(self.trf(
                    "正文落盘失败：{}",
                    "Failed to write body: {}",
                    &[&e],
                )));
                return;
            }
            store.mark_has_body(hash).ok();
            bytes
        };

        if !row.seen {
            store.set_seen(hash, true).ok();
            let _ = backend.set_flags(&row.folder, &[hash as u64], FlagChange::Seen(true));
        }

        // M5-c：PGP/MIME 先行处理（解密/验签），结果再进正文管线（HTML 渲染复用）
        let (parse_bytes, pgp_status) = self.pgp_process(&bytes);
        let (body, is_html) = extract_body(&parse_bytes);
        if pgp_status.is_none() {
            store.index_body(hash, &body).ok(); // 加密邮件正文不进全文索引（安全默认）
        }
        let attachments = extract_attachments(&parse_bytes);
        let (to_addrs, cc_addrs) = extract_recipients(&parse_bytes);
        let unsubscribe = extract_unsubscribe(&parse_bytes);
        let inline_images = extract_inline_images(&parse_bytes);
        let remote_images = if let Some(html) = parse_bytes_html(&parse_bytes) {
            html::remote_img_urls(html.as_ref())
        } else {
            vec![]
        };
        // 图片附件预览：魔数过滤防解码 panic，单张 2MB 上限（与远程图同护栏）
        let attachment_images: Vec<(String, Vec<u8>)> = m_atch_images(&parse_bytes);
        // PDF 附件缩略图：QuickLook（macOS 原生 qlmanage；其他平台空）
        let attachment_previews = pdf_thumbnails(&parse_bytes, &self.data_dir);
        self.emit(Event::BodyLoaded {
            hash,
            attachment_images,
            attachment_previews,
            subject: row.subject.clone(),
            to_addrs,
            cc_addrs,
            unsubscribe,
            inline_images,
            remote_images,
            body,
            is_html,
            attachments,
            pgp: pgp_status,
        });
        if !row.seen {
            self.emit(Event::FlagsChanged {
                hash,
                seen: Some(true),
                flagged: None,
            });
        }
    }

    /// M5-c：顶层 PGP/MIME 处理。返回（用于正文解析的字节, 状态行）。
    /// 失败不炸：返回原始字节 + 状态说明，正文管线照常跑（显示密文/原文）。
    fn pgp_process(&self, raw: &[u8]) -> (Vec<u8>, Option<String>) {
        match pgp::detect_pgp_kind(raw) {
            Some(pgp::PgpKind::Encrypted) => {
                let rt = match self.pgp_runtime() {
                    Err(e) => return (raw.to_vec(), Some(format!("🔒 加密邮件（{e}）"))),
                    Ok(None) => {
                        return (raw.to_vec(), Some("🔒 加密邮件（未配置 PGP 私钥）".into()))
                    }
                    Ok(Some(rt)) => rt,
                };
                let Some(ciphertext) = pgp::extract_pgp_ciphertext(raw) else {
                    return (raw.to_vec(), Some("🔒 加密邮件（找不到密文部分）".into()));
                };
                match rt.decrypt(&ciphertext) {
                    Ok((plain, status)) => (plain, Some(status)),
                    Err(e) => (raw.to_vec(), Some(format!("🔒 解密失败：{e}"))),
                }
            }
            Some(pgp::PgpKind::Signed) => {
                let Some((content, sig)) = pgp::signed_mime_parts(raw) else {
                    return (raw.to_vec(), None);
                };
                let status = match self.pgp_runtime() {
                    Err(e) => format!("⚠ 签名未验证（{e}）"),
                    Ok(None) => "⚠ 签名未验证（未配置 PGP）".to_string(),
                    Ok(Some(rt)) => rt.verify_detached(&content, &sig),
                };
                (raw.to_vec(), Some(status))
            }
            None => (raw.to_vec(), None),
        }
    }

    fn body_path(&self, hash: i64) -> PathBuf {
        self.data_dir.join("bodies").join(format!("{hash}.eml"))
    }

    // ---------------- 发信 ----------------

    /// 组装发信凭据（oauth token 注入）+ 建可复用 sender。
    /// oauth/token 失败返 Err：单发路径报错留 pending，flush 路径整轮暂缓重试
    fn smtp_sender(&self) -> Result<smail_backend::SmtpSender, String> {
        let Some(mut smtp) = self.cfg.smtp.clone() else {
            return Err("未配置 [smtp] 段，无法发信".into());
        };
        if let Some(o) = &self.cfg.oauth {
            let tok = oauth::ensure_token(o, &self.data_dir)?;
            smtp.password = Some(tok);
            smtp.auth = Some("xoauth2".into());
        }
        smail_backend::build_sender(&smtp).map_err(|e| e.to_string())
    }

    fn flush_outbox(&self, backend: &mut dyn MailBackend, store: &Store) {
        if let Ok(n) = store.fail_exhausted_outbox() {
            if n > 0 {
                self.emit(Event::Status(self.trf("{} 封发件重试穷尽，已标 failed（请检查收件人/附件后重发）", "{} outgoing message(s) exhausted retries, marked failed (check recipients/attachments and resend)", &[&n])));
            }
        }
        // counts 先于 early-return：最后一封发走（队列清空）时也要广播 (0,0)，
        // 否则侧栏徽标永远残留（旧版空队列提前 return，从不归零）
        let (p, f) = store.outbox_counts().unwrap_or((0, 0));
        self.emit(Event::OutboxPending {
            pending: p,
            failed: f,
        });
        let pending = store.pending_outbox().unwrap_or_default();
        if pending.is_empty() {
            return;
        }
        // 一轮一握手：sender 复用（旧路径每封一次 TCP+TLS+AUTH，断网重试期间尤为浪费）
        let sender = match self.smtp_sender() {
            Ok(s) => s,
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "发件队列暂缓（下轮重试）：{}",
                    "Outbox deferred (retry next round): {}",
                    &[&e],
                )));
                return;
            }
        };
        for p in pending {
            // outbox 重试不携带 PGP（PGP 失败一律标 failed 不会回到 pending）
            self.try_send(
                backend,
                store,
                p.id,
                &p.to_addr,
                &p.cc,
                &p.bcc,
                &p.subject,
                &p.body,
                &p.attachments,
                None,
                &sender,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn try_send(
        &self,
        backend: &mut dyn MailBackend,
        store: &Store,
        outbox_id: i64,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
        attachment_paths: &[String],
        pgp_mode: Option<pgp::PgpSendMode>,
        sender: &smail_backend::SmtpSender,
    ) {
        let mut attachments: Vec<(String, Vec<u8>)> = Vec::new();
        for path in attachment_paths {
            match std::fs::read(path) {
                Ok(bytes) => {
                    let name = Path::new(path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("attachment.bin")
                        .to_string();
                    attachments.push((name, bytes));
                }
                Err(e) => {
                    store
                        .outbox_mark(outbox_id, "pending", &format!("附件读取失败 {path}: {e}"))
                        .ok();
                    self.emit(Event::Status(self.trf(
                        "附件读取失败 {}: {}",
                        "Failed to read attachment {}: {}",
                        &[&path, &e],
                    )));
                    return;
                }
            }
        }
        let mail = OutgoingMail {
            from: self.cfg.identity.clone(),
            to: split_addrs(to),
            cc: split_addrs(cc),
            bcc: split_addrs(bcc),
            subject: subject.to_string(),
            body: body.to_string(),
            attachments,
        };
        // M5-d：PGP 发信（构造完整 MIME 后走 raw 通道；
        // 密钥类失败是永久错误 → failed，不进重试队列）
        let sent_bytes: std::result::Result<Vec<u8>, BackendError> = if let Some(mode) = pgp_mode {
            let fail_permanent = |msg: String| BackendError(msg);
            let built = (|| -> Result<Vec<u8>, String> {
                let rt = self
                    .pgp_runtime()?
                    .ok_or("未配置 [accounts.pgp]，无法 PGP 发信")?;
                let signer = rt.secret_cert()?;
                let wkd_on = self.cfg.pgp.as_ref().map(|p| p.wkd).unwrap_or(false);
                let mut rcpt_certs = Vec::new();
                let mut missing = Vec::new();
                for addr in mail.to.iter().chain(mail.cc.iter()).chain(mail.bcc.iter()) {
                    match rt.find_cert_by_email(addr) {
                        Some(c) if mode == pgp::PgpSendMode::EncryptSign => rcpt_certs.push(c),
                        _ => {
                            // WKD：向收件人域名查询公开托管的公钥（best-effort，未命中回退报错）
                            if mode == pgp::PgpSendMode::EncryptSign && wkd_on {
                                if let Some(cert) = wkd::discover_cert(addr) {
                                    rcpt_certs.push(cert);
                                    continue;
                                }
                            }
                            missing.push(addr.clone());
                        }
                    }
                }
                if mode == pgp::PgpSendMode::EncryptSign && !missing.is_empty() {
                    return Err(format!(
                        "以下收件人没有公钥，无法加密：{}（先把对方公钥加进 pubring）",
                        missing.join(", ")
                    ));
                }
                let mid = format!(
                    "<{}.pgp@{}>",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                    &self.cfg.imap.username
                );
                pgp::build_outgoing(&pgp::OutgoingPgp {
                    from: &mail.from,
                    to: &mail.to,
                    cc: &mail.cc,
                    subject: &mail.subject,
                    body: &mail.body,
                    attachments: &mail.attachments,
                    mode,
                    signer: &signer,
                    recipient_certs: &rcpt_certs,
                    self_cert: &signer,
                    message_id: &mid,
                })
            })();
            match built {
                Ok(bytes) => {
                    // 密送进信封不进 MIME（PGP 路径自组内容，Bcc 头本就不写）
                    let mut rcpts = mail.to.clone();
                    rcpts.extend(mail.cc.iter().cloned());
                    rcpts.extend(mail.bcc.iter().cloned());
                    sender.send_raw(&mail.from, &rcpts, &bytes).map(|_| bytes)
                }
                Err(e) => Err(fail_permanent(e)),
            }
        } else {
            sender.send(&mail)
        };
        match sent_bytes {
            Ok(bytes) => {
                // MailSent 先行：SMTP 已成功，用户立刻看到"已发送"——
                // Sent 归档（IMAP APPEND + SELECT + FETCH 2-5s）不阻塞反馈
                store.outbox_mark(outbox_id, "sent", "").ok();
                self.emit(Event::MailSent {
                    to: to.to_string(),
                    subject: subject.to_string(),
                });
                // 成功即时归零徽标（不等 30s tick）
                if let Ok((p, f)) = store.outbox_counts() {
                    self.emit(Event::OutboxPending {
                        pending: p,
                        failed: f,
                    });
                }
                // Sent 归档 + 同步（尽力而为；失败只记状态不影响"已发送"事实）
                match backend.append_to_sent(bytes) {
                    Ok(sent_folder) => {
                        self.emit(Event::Status(self.trf(
                            "已归档到 {}",
                            "Archived to {}",
                            &[&sent_folder],
                        )));
                        let f = sent_folder;
                        self.sync_folder(backend, store, &f);
                        // 侧栏 Sent 计数即时刷新（本地计数零往返）
                        self.emit_folder_counts(store);
                    }
                    Err(e) => {
                        self.emit(Event::Status(self.trf(
                            "已发出（归档失败：{}）",
                            "Sent (archive failed: {})",
                            &[&e],
                        )));
                    }
                }
                // #9 staging 清理：转发附件的暂存文件发送成功后删掉
                //（只清「attachments-staging」目录下的，用户自选文件不动）
                for path in attachment_paths {
                    let p = std::path::Path::new(path);
                    if p.parent().and_then(|d| d.file_name())
                        == Some("attachments-staging".as_ref())
                    {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }
            Err(e) => {
                let permanent = pgp_mode.is_some();
                let status = if permanent { "failed" } else { "pending" };
                let hint = if permanent {
                    "PGP 发信失败（不自动重试，请检查密钥后重发）"
                } else {
                    "发送失败（稍后自动重试）"
                };
                store.outbox_mark(outbox_id, status, &e.0).ok();
                self.emit(Event::Status(self.trf("{}：{}", "{}: {}", &[&hint, &e])));
            }
        }
    }

    // ---------------- M3：附件保存 + 后台索引 ----------------

    fn save_attachment(&self, hash: i64, name: String) {
        // 内存护栏：超大邮件/附件不做全量进 RAM 的提取（用户仍可导出 .eml）
        if let Ok(meta) = std::fs::metadata(self.body_path(hash)) {
            const MAX_EML: u64 = 512 * 1024 * 1024;
            if meta.len() > MAX_EML {
                self.emit(Event::Status(self.trf(
                    "邮件过大（{} MB）不做附件提取，请用「存为 .eml」",
                    "Mail too large ({} MB) for attachment extraction; use Save .eml",
                    &[&(meta.len() / 1024 / 1024)],
                )));
                return;
            }
        }
        let bytes = match std::fs::read(self.body_path(hash)) {
            Ok(b) => b,
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "读邮件缓存失败：{}",
                    "Failed to read mail cache: {}",
                    &[&e],
                )));
                return;
            }
        };
        let Some(data) = extract_attachment_bytes(&bytes, &name) else {
            self.emit(Event::Status(self.trf(
                "找不到附件 {name:?}",
                "Attachment {} not found",
                &[],
            )));
            return;
        };
        let dir: PathBuf = std::env::var_os("SMAIL_DOWNLOADS")
            .map(Into::into)
            .unwrap_or_else(|| {
                let mut d = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
                d.push("Downloads");
                d
            });
        let _ = std::fs::create_dir_all(&dir);
        let safe: String = name
            .chars()
            .map(|c| if c == '/' || c == '\\' { '_' } else { c })
            .collect();
        let mut target = dir.join(&safe);
        let mut n = 1;
        while target.exists() {
            let stem = safe.trim_end_matches(|c| c != '.').trim_end_matches('.');
            target = dir.join(format!("{stem}-{n}"));
            n += 1;
        }
        if let Err(e) = std::fs::write(&target, data) {
            self.emit(Event::Status(self.trf(
                "附件写入失败：{}",
                "Failed to write attachment: {}",
                &[&e],
            )));
            return;
        }
        let path = target.display().to_string();
        self.emit(Event::AttachmentSaved { path: path.clone() });
        #[cfg(target_os = "macos")]
        if std::env::var_os("SMAIL_NO_REVEAL").is_none() {
            let _ = std::process::Command::new("open")
                .arg("-R")
                .arg(&target)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    /// 后台索引器：分批拉正文建 FTS。批内逐条让路：用户命令到达即中断，
    /// 把命令交还主循环优先处理（索引是后台活，不该挡交互）
    fn index_pass(
        &self,
        backend: &mut dyn MailBackend,
        store: &Store,
        cmd_rx: &Receiver<Cmd>,
    ) -> Option<Cmd> {
        let todo = store.unindexed(INDEX_BATCH).unwrap_or_default();
        for hash in todo {
            if let Ok(cmd) = cmd_rx.try_recv() {
                return Some(cmd);
            }
            let bytes = match backend.fetch_body(hash as u64) {
                Ok(b) => b,
                Err(_) => return None,
            };
            if !self.body_path(hash).exists() {
                let _ = std::fs::write(self.body_path(hash), &bytes);
                store.mark_has_body(hash).ok();
            }
            // 加密邮件：只置位不建正文索引（密文进 FTS = 噪声污染 + 违反设计承诺）
            if pgp::detect_pgp_kind(&bytes) == Some(pgp::PgpKind::Encrypted) {
                store.mark_indexed(hash).ok();
                continue;
            }
            let (text, _) = extract_body(&bytes);
            store.index_body(hash, &text).ok();
        }
        None
    }
}

fn backoff(attempts: u32) -> Duration {
    let secs = 1u64.saturating_mul(1 << attempts.min(6));
    Duration::from_secs(secs.min(60))
}

/// 从原始邮件抽 To/Cc 头（回复全部用；简单按头名取值，不解码 MIME 名字）
fn extract_recipients(raw: &[u8]) -> (String, String) {
    let txt = String::from_utf8_lossy(raw);
    let header_end = txt.find("\r\n\r\n").unwrap_or(txt.len());
    let head = &txt[..header_end];
    let mut to = String::new();
    let mut cc = String::new();
    // 0=To 1=Cc 2=无（折行续接归属上一个头）
    let mut cur = 2u8;
    for line in head.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("to:") {
            cur = 0;
            to.push_str(line[3..].trim());
        } else if lower.starts_with("cc:") {
            cur = 1;
            cc.push_str(line[3..].trim());
        } else if line.starts_with(' ') || line.starts_with('\t') {
            match cur {
                0 => {
                    to.push(' ');
                    to.push_str(line.trim());
                }
                1 => {
                    cc.push(' ');
                    cc.push_str(line.trim());
                }
                _ => {}
            }
        } else {
            cur = 2;
        }
    }
    (to.trim().to_string(), cc.trim().to_string())
}

/// "名字 <a@b>" → (a@b, 名字)；无尖括号 → (整串, "")。通讯录收集用
fn parse_from_addr(s: &str) -> (String, String) {
    match s.find('<') {
        Some(st) => {
            let email = s[st + 1..].trim_end_matches('>').trim().to_string();
            let name = s[..st].trim().to_string();
            (email, name)
        }
        None => (s.trim().to_string(), String::new()),
    }
}

fn split_addrs(s: &str) -> Vec<String> {
    s.split([',', ';', '，', '；'])
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// 状态文案双语（服务线程用；zh 由 SetLang 运行时切换）
pub(crate) fn tr_str(zh: bool, zh_s: &'static str, en_s: &'static str) -> String {
    if zh {
        zh_s.into()
    } else {
        en_s.into()
    }
}
pub(crate) fn trf_str(
    zh: bool,
    zh_s: &'static str,
    en_s: &'static str,
    args: &[&dyn std::fmt::Display],
) -> String {
    let mut out = if zh { zh_s } else { en_s }.to_string();
    for a in args {
        out = out.replacen("{}", &a.to_string(), 1);
    }
    out
}

#[cfg(test)]
mod unsubscribe_tests {
    use super::extract_unsubscribe;

    #[test]
    fn prefers_http_over_mailto() {
        let raw = b"From: a@b\r\n\
List-Unsubscribe: <https://ml.example.com/u/1>, <mailto:unsub@example.com>\r\n\
\r\nbody";
        assert_eq!(
            extract_unsubscribe(raw).as_deref(),
            Some("https://ml.example.com/u/1")
        );
    }

    #[test]
    fn mailto_fallback_and_folding() {
        // 折行续接 + 无 http 候选
        let raw = b"From: a@b\r\n\
List-Unsubscribe: <mailto:leave@example.com?subject=bye>,\r\n <https://x/1>\r\n\
\r\nbody";
        assert_eq!(extract_unsubscribe(raw).as_deref(), Some("https://x/1"));
        let raw2 = b"Subject: s\r\nList-Unsubscribe: <mailto:unsub@x>\r\n\r\nb";
        assert_eq!(extract_unsubscribe(raw2).as_deref(), Some("mailto:unsub@x"));
    }

    #[test]
    fn absent_or_garbage() {
        assert_eq!(extract_unsubscribe(b"From: a\r\n\r\nb"), None);
        // 值里的候选都不是 URL 形态
        assert_eq!(
            extract_unsubscribe(b"List-Unsubscribe: <ftp://x>, noise\r\n\r\nb"),
            None
        );
    }
}

/// 按名取任意头值（含折行续接；大小写不敏感）
fn extract_header_value(head: &str, name: &str) -> Option<String> {
    let lower_name = name.to_lowercase();
    let mut val = String::new();
    let mut hit = false;
    for line in head.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with(&format!("{lower_name}:")) {
            hit = true;
            val.push_str(line.split_once(':').map(|x| x.1).unwrap_or("").trim());
        } else if hit && (line.starts_with(' ') || line.starts_with('\t')) {
            val.push_str(line.trim());
        } else if hit {
            break;
        }
    }
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// List-Unsubscribe（RFC 2369/8058）：`<https://…>, <mailto:…>`，
/// 优先 http(s)（浏览器一键退订），退回 mailto
fn extract_unsubscribe(raw: &[u8]) -> Option<String> {
    let txt = String::from_utf8_lossy(raw);
    let header_end = txt.find("\r\n\r\n").unwrap_or(txt.len());
    let head = &txt[..header_end];
    let v = extract_header_value(head, "List-Unsubscribe")?;
    let mut http = None;
    let mut mailto = None;
    for part in v.split(',') {
        let p = part.trim().trim_start_matches('<').trim_end_matches('>');
        if p.starts_with("http://") || p.starts_with("https://") {
            http = http.or(Some(p.to_string()));
        } else if p.starts_with("mailto:") {
            mailto = mailto.or(Some(p.to_string()));
        }
    }
    http.or(mailto)
}

fn to_row(env: &EnvelopeSummary, folder: &str) -> MessageRow {
    MessageRow {
        envelope_hash: env.hash as i64,
        folder: folder.to_string(),
        message_id: env.message_id.clone(),
        in_reply_to: env.in_reply_to.clone(),
        subject: env.subject.clone(),
        from_addr: env.from_addr.clone(),
        date_unix: env.date_unix,
        seen: env.seen,
        flagged: env.flagged,
        has_body: false,
    }
}

/// M7：哈希按所在文件夹分组（批量操作一次 IMAP 往返一组）；
/// 本地已不存在的哈希安全跳过
fn group_by_folder(store: &Store, hashes: &[i64]) -> Vec<(String, Vec<i64>)> {
    let mut groups: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
    for h in hashes {
        if let Ok(Some(row)) = store.get(*h) {
            groups.entry(row.folder).or_default().push(*h);
        }
    }
    groups.into_iter().collect()
}

/// HTML 部分（无则 None）——远程图片 URL 提取用
fn parse_bytes_html(bytes: &[u8]) -> Option<String> {
    use mail_parser::MessageParser;
    Some(
        MessageParser::default()
            .parse(bytes)?
            .body_html(0)?
            .into_owned(),
    )
}

/// 内嵌图片（Content-Disposition: inline）：(归一化 cid, 字节)。
/// HTML 的 <img src="cid:…"> 引用它们；随邮件自带无隐私问题，自动显示
fn extract_inline_images(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    use mail_parser::{MessageParser, MimeHeaders};
    let Some(m) = MessageParser::default().parse(bytes) else {
        return vec![];
    };
    m.attachments()
        .filter(|a| {
            a.content_disposition()
                .map(|d| d.is_inline())
                .unwrap_or(false)
        })
        .filter_map(|a| {
            let cid = a
                .content_id()?
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string();
            if cid.is_empty() {
                return None;
            }
            Some((cid, a.contents().to_vec()))
        })
        .collect()
}

/// 附件清单（名字 + 大小）
/// 图片附件字节（预览用）：PNG/JPEG/GIF/WebP/BMP 魔数 + 单张 2MB
fn m_atch_images(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    use mail_parser::{MessageParser, MimeHeaders};
    const MAX: usize = 2 * 1024 * 1024;
    let Some(m) = MessageParser::default().parse(bytes) else {
        return vec![];
    };
    m.attachments()
        .filter_map(|a| {
            let name = a.attachment_name()?.to_string();
            let c = a.contents();
            if c.len() > MAX || !is_image_content(c) {
                return None;
            }
            Some((name, c.to_vec()))
        })
        .collect()
}

pub fn is_image_content(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG")
        || b.starts_with(b"\xff\xd8\xff")
        || b.starts_with(b"GIF8")
        || b.starts_with(b"RIFF") && b.len() > 11 && &b[8..12] == b"WEBP"
        || b.starts_with(b"BM")
}

/// PDF 附件 → QuickLook 缩略图 PNG（macOS qlmanage；非 macOS / 失败返回空）
fn pdf_thumbnails(bytes: &[u8], data_dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (bytes, data_dir);
        return vec![];
    }
    #[cfg(target_os = "macos")]
    {
        use mail_parser::{MessageParser, MimeHeaders};
        let Some(m) = MessageParser::default().parse(bytes) else {
            return vec![];
        };
        let tmp_dir = data_dir.join("ql-tmp");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let mut out = Vec::new();
        for (i, a) in m.attachments().enumerate() {
            let Some(name) = a.attachment_name() else {
                continue;
            };
            if !name.to_lowercase().ends_with(".pdf") {
                continue;
            }
            let contents = a.contents();
            if contents.len() > 20 * 1024 * 1024 {
                continue;
            } // 20MB 上限
            let pdf_path = tmp_dir.join(format!("preview-{i}.pdf"));
            if std::fs::write(&pdf_path, contents).is_err() {
                continue;
            }
            // qlmanage -t -s 480：480px 宽 PNG 缩略图
            let png_path = tmp_dir.join(format!("preview-{i}.pdf.png"));
            let ok = std::process::Command::new("qlmanage")
                .args([
                    "-t",
                    "-s",
                    "480",
                    "-o",
                    tmp_dir.to_str().unwrap_or("/tmp"),
                    pdf_path.to_str().unwrap_or(""),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                if let Ok(png) = std::fs::read(&png_path) {
                    out.push((name.to_string(), png));
                }
                let _ = std::fs::remove_file(&png_path);
            }
            let _ = std::fs::remove_file(&pdf_path);
        }
        let _ = std::fs::remove_dir(&tmp_dir);
        out
    }
}

fn extract_attachments(bytes: &[u8]) -> Vec<(String, usize)> {
    use mail_parser::{MessageParser, MimeHeaders};
    let Some(m) = MessageParser::default().parse(bytes) else {
        return vec![];
    };
    m.attachments()
        .filter_map(|a| {
            a.attachment_name()
                .map(|n| (n.to_string(), a.contents().len()))
        })
        .collect()
}

/// 按名取附件字节
fn extract_attachment_bytes(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
    use mail_parser::{MessageParser, MimeHeaders};
    let m = MessageParser::default().parse(bytes)?;
    let found = m
        .attachments()
        .find(|a| a.attachment_name() == Some(name))?;
    Some(found.contents().to_vec())
}

/// 从原始 EML 提取可读内容。
/// 策略（M4）：有 text/html 就走消毒管线转 markdown（现代邮件 HTML 表达力完整）；
/// 否则 text/plain 原样（GBK 等字符集由 mail-parser 处理）。
/// 返回 (内容, 是否为 markdown)。
pub fn extract_body(bytes: &[u8]) -> (String, bool) {
    use mail_parser::MessageParser;
    let Some(message) = MessageParser::default().parse(bytes) else {
        return (format!("[无法解析邮件正文]（{} 字节）", bytes.len()), false);
    };
    if let Some(html) = message.body_html(0) {
        return (crate::html::html_to_markdown(html.as_ref()), true);
    }
    if let Some(text) = message.body_text(0) {
        return (text.into_owned(), false);
    }
    (format!("[无文本部分]（{} 字节）", bytes.len()), false)
}

#[cfg(test)]
mod tr_tests {
    use super::{tr_str, trf_str};

    #[test]
    fn bilingual_selection_and_fill() {
        assert_eq!(tr_str(true, "已重连", "Reconnected"), "已重连");
        assert_eq!(tr_str(false, "已重连", "Reconnected"), "Reconnected");
        assert_eq!(
            trf_str(true, "已删除 {} 封", "Deleted {} message(s)", &[&3]),
            "已删除 3 封"
        );
        assert_eq!(
            trf_str(false, "已删除 {} 封", "Deleted {} message(s)", &[&3]),
            "Deleted 3 message(s)"
        );
        // 多参数依序填充
        assert_eq!(trf_str(false, "{} → {}", "{} → {}", &[&"a", &"b"]), "a → b");
    }
}

#[cfg(test)]
mod recipient_tests {
    use super::extract_recipients;

    #[test]
    fn to_cc_with_folding() {
        let raw = b"From: a@b\r\nTo: Name One <one@x>, two@x,\r\n three@x\r\nCc: cc1@x\r\nSubject: s\r\n\r\nbody";
        let (to, cc) = extract_recipients(raw);
        assert!(
            to.contains("one@x") && to.contains("two@x") && to.contains("three@x"),
            "{to}"
        );
        assert_eq!(cc, "cc1@x");
    }
}
