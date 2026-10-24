//! IMAP/SMTP 后端封装
//!
//! ## 职责
//!
//! - **IMAP 同步**：基于 melib 的文件夹列表、信封拉取、IDLE 监听
//! - **SMTP 发信**：基于 lettre 的 OAuth/密码认证、TLS 连接
//! - **文件夹分类**：识别 Inbox/Drafts/Sent/Trash/Junk/Archive 特殊文件夹
//!
//! ## melib 集成要点（S3 实证，勿删）
//!
//! 1. **is_online() 必须最先调用**：否则所有操作返回 "Offline" 错误
//! 2. **server_* 配置是顶层 flatten**：server_port 也要引号（字符串表）
//! 3. **watch 事件可能重复到达**：上层只当信号，靠 diff 补差
//!
//! ## 同步模型
//!
//! ```text
//! 服务线程 ──阻塞调用──> Backend::fetch_envelopes()
//!                                 (melib 阻塞 I/O)
//!
//! melib 内部线程 ──watch 事件──> channel ──> 服务线程 IDLE 推送
//! ```
//!
//! ## 关键类型
//!
//! - `Backend`：IMAP 后端实例（melib MailBackend 包装）
//! - `SmtpClient`：SMTP 发信客户端（lettre SmtpTransport 包装）
//! - `FolderKind`：特殊文件夹类型（UI 图标/动作路由）

mod error;

pub use error::{BackendError, Result};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use melib::backends::utf7::encode_utf7_imap;
use melib::backends::{
    BackendEvent, BackendEventConsumer, EnvelopeHashBatch, FlagOp, IsSubscribedFn,
    MailBackend as MelibMailBackend, MailboxHash,
};
use melib::conf::AccountSettings;
use melib::imap::ImapType;
use melib::EnvelopeHash;
use melib::Flag;

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// None 且账号配了 [oauth] 时走 XOAUTH2（由上层填 base64 SASL 串）
    pub password: Option<String>,
    #[serde(default)]
    pub starttls: bool,
    /// false = 纯文本（仅本地测试）
    #[serde(default = "default_true")]
    pub tls: bool,
    /// melib use_oauth2：server_password 须为 base64("user=..\x01auth=Bearer ..\x01\x01")
    #[serde(default)]
    pub use_oauth2: bool,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// None 且配了 oauth 时由上层填 access token
    pub password: Option<String>,
    /// "xoauth2" = XOAUTH2（password 为 access token）；缺省 PLAIN/LOGIN
    #[serde(default)]
    pub auth: Option<String>,
    /// From 地址覆盖（username 不是合法邮箱时用，例如本地测试 test → test@local）
    #[serde(default)]
    pub from: Option<String>,
    /// "ssl"=465隐式TLS | "starttls"=587 | "none"=本地测试
    #[serde(default)]
    pub tls: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 一封待发邮件（UI 编辑器 → 后端）
#[derive(Debug, Clone)]
pub struct OutgoingMail {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// 密送：进信封收件人不进 MIME 头（PGP raw 路径；lettre 路径发送时自动剔头）
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// (文件名, 字节)。M3：附件随 multipart 发送
    pub attachments: Vec<(String, Vec<u8>)>,
}

// ---------------------------------------------------------------------------
// 对外类型
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ServerEvent {
    FolderChanged { folder: String },
    ConnectionLost { reason: String },
}

#[derive(Debug, Clone)]
pub struct EnvelopeSummary {
    pub hash: u64,
    pub message_id: String,
    /// M5：会话聚合用（"<id>" 形式，空串=无）
    pub in_reply_to: String,
    pub subject: String,
    pub from_addr: String,
    pub date_unix: i64,
    pub seen: bool,
    pub flagged: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum FlagChange {
    Seen(bool),
    Flagged(bool),
}

/// 特殊文件夹类型（侧栏图标/重定向/垃圾动作用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    Inbox,
    Sent,
    Drafts,
    Junk,
    Archive,
    Trash,
    Plain,
}

/// 按名字识别特殊文件夹（大小写不敏感；中英常见命名）
pub fn folder_kind(path: &str) -> FolderKind {
    use FolderKind::*;
    let l = path.to_lowercase();
    if l == "inbox" {
        Inbox
    } else if l.contains("draft") || l.contains("草稿") {
        Drafts
    } else if l.contains("sent") || l.contains("已发送") {
        Sent
    } else if l.contains("junk") || l.contains("spam") || l.contains("垃圾") {
        Junk
    } else if l.contains("archive") || l.contains("归档") || l.contains("all mail") {
        Archive
    } else if l.contains("trash")
        || l.contains("deleted")
        || path.contains("已删除")
        || path.contains("废纸篓")
    {
        Trash
    } else {
        Plain
    }
}

// ---------------------------------------------------------------------------
// trait
// ---------------------------------------------------------------------------

/// 所有方法 `&self`：实现方用内部锁保证并发安全（melib 层自带连接池），
/// 使正文取拉可在 Reader 线程与服务主循环的同步/写操作真正并行
pub trait MailBackend: Send + Sync {
    fn is_online(&self) -> Result<()>;
    fn list_folders(&self) -> Result<Vec<String>>;
    fn fetch_envelopes(&self, folder: &str) -> Result<Vec<EnvelopeSummary>>;
    /// 并发安全取正文：Reader 线程与服务主循环可同时调用
    /// （melib envelope_bytes_by_hash 内部 Arc 连接池，天然并行）
    fn fetch_body(&self, hash: u64) -> Result<Vec<u8>>;
    fn watch(&self) -> Result<std::sync::mpsc::Receiver<ServerEvent>>;

    // --- M2 ---
    fn set_flags(&self, folder: &str, hashes: &[u64], change: FlagChange) -> Result<()>;
    fn delete_messages(&self, folder: &str, hashes: &[u64]) -> Result<()>;
    /// APPEND 到服务器 Sent 文件夹（找不到类 Sent 文件夹时报错，由上层降级）
    fn append_to_sent(&self, bytes: Vec<u8>) -> Result<String>;

    // --- M5-b ---
    /// 服务器端移动（IMAP MOVE 或 COPY + 删除标记 + EXPUNGE）。成功后由上层删本地行。
    fn move_messages(&self, folder: &str, hashes: &[u64], dest: &str) -> Result<()>;
    // --- M7：文件夹管理（IMAP CREATE/DELETE/RENAME） ---
    fn create_folder(&self, path: &str) -> Result<()>;
    fn delete_folder(&self, path: &str) -> Result<()>;
    fn rename_folder(&self, path: &str, new_path: &str) -> Result<()>;

    // --- B2：草稿云同步（v1 单向：本地保存→上传，本地删除→服务器清理） ---
    /// APPEND 草稿到服务器 Drafts 类文件夹，返回文件夹名。默认不支持
    fn append_draft(&self, _bytes: Vec<u8>) -> Result<String> {
        Err(BackendError::Other("该后端不支持草稿上传".into()))
    }
    /// 在 Drafts 类文件夹按 Message-ID 找到并删除服务器草稿。
    /// 返回是否找到并删（找不到不算错——对方设备可能已处理）
    fn delete_draft_by_mid(&self, _message_id: &str) -> Result<bool> {
        Ok(false)
    }
    /// Drafts 类文件夹名（草稿双向同步对账用）；无则 None。默认 None
    fn drafts_folder_name(&self) -> Option<String> {
        None
    }
    /// 归档目标文件夹：找到既有（archive/归档/All Mail），没有则新建 Archive。
    /// 一键归档动作用。默认不支持
    fn ensure_archive_folder(&self) -> Result<String> {
        Err(BackendError::Other("该后端不支持归档".into()))
    }
    /// 垃圾目标文件夹：找到既有（junk/spam/垃圾），没有则新建 Junk。
    /// 「标为垃圾」动作用。默认不支持
    fn ensure_junk_folder(&self) -> Result<String> {
        Err(BackendError::Other("该后端不支持垃圾标记".into()))
    }
    /// 废纸篓：找到既有（trash/deleted/已删除/废纸篓），没有则新建 Trash。
    /// 「删除=进废纸篓可恢复」语义用（Foxmail/Outlook 惯例）。默认不支持
    fn ensure_trash_folder(&self) -> Result<String> {
        Err(BackendError::Other("该后端不支持废纸篓".into()))
    }
}

// ---------------------------------------------------------------------------
// melib 实现
// ---------------------------------------------------------------------------

pub struct MelibBackend {
    /// melib 后端。Mutex 保护 + 全部 `&self` 方法：锁只在构造 melib future 时
    /// 短暂持有（内部仅 Arc clone，微秒级），网络 await 在锁外走 melib 连接池——
    /// 服务主循环与 Reader 线程（正文取拉）可真正并发，互不阻塞命令循环
    backend: Arc<Mutex<Box<dyn MelibMailBackend>>>,
    folders: Mutex<HashMap<String, MailboxHash>>,
    folder_index: Arc<Mutex<HashMap<MailboxHash, String>>>,
}

impl MelibBackend {
    /// 短暂持锁执行构造闭包。绝不能在持锁期间 await（网络秒级会饿死另一条线程）
    fn folders_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, MailboxHash>> {
        self.folders.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn with_backend<R>(&self, f: impl FnOnce(&mut Box<dyn MelibMailBackend>) -> R) -> R {
        let mut g = self.backend.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut g)
    }

    pub fn new(cfg: &ImapConfig) -> Result<Self> {
        ensure_xdg_data_home();
        if cfg.use_oauth2 {
            // melib 会 base64 解码 server_password 后作为 SASL 初始响应发出
            if cfg.password.as_deref().unwrap_or("").is_empty() {
                return Err(BackendError::ConfigError(
                    "use_oauth2=true 需要 password 为 base64 SASL 串".into(),
                ));
            }
        } else if cfg.password.as_deref().unwrap_or("").is_empty() {
            return Err(BackendError::ConfigError(
                "缺少 imap 密码（或改配 [oauth]）".into(),
            ));
        }
        // 安全：经 serde 序列化生成配置文档（自动转义 " 与换行）。
        // 此前 format! 直拼 TOML，密码/用户名含 " 或换行可注入任意配置项
        //（如 use_tls=false 降级 TLS），且合法密码会解析失败。
        // 注意：字段全用 String——melib 的 extra 是 IndexMap<String,String>，
        // TOML 整数/布尔无法 flatten 进 String map。
        #[derive(serde::Serialize)]
        struct ImapSettingsDoc {
            name: String,
            root_mailbox: &'static str,
            format: &'static str,
            identity: String,
            server_hostname: String,
            server_port: String,
            server_username: String,
            server_password: String,
            use_tls: String,
            use_starttls: String,
            use_oauth2: String,
            timeout: String,
        }
        let doc = ImapSettingsDoc {
            // 每账号唯一：melib IMAP 缓存 DB 文件名由 account name 拼出，
            // 全部叫 "s-mail" 会让多账号互撞同一份缓存；且 name 不能含路径
            // 分隔符（melib 校验拒绝后缓存会静默降级），邮箱地址里的 / 也防御掉
            name: format!("s-mail.{}", cfg.username.replace('/', "_")),
            root_mailbox: "INBOX",
            format: "imap",
            identity: cfg.username.clone(),
            server_hostname: cfg.host.clone(),
            server_port: cfg.port.to_string(),
            server_username: cfg.username.clone(),
            server_password: cfg.password.clone().unwrap_or_default(),
            use_tls: cfg.tls.to_string(),
            use_starttls: cfg.starttls.to_string(),
            use_oauth2: cfg.use_oauth2.to_string(),
            timeout: "30".into(),
        };
        let settings_toml = toml::to_string(&doc)
            .map_err(|e| BackendError::ConfigError(format!("序列化 IMAP 配置失败: {e}")))?;
        let settings: AccountSettings = toml::from_str(&settings_toml)
            .map_err(|e| BackendError::ConfigError(format!("构造 AccountSettings 失败: {e}")))?;

        let event_consumer = BackendEventConsumer::new(Arc::new(|_acct, _ev| {}));
        let backend: Box<dyn MelibMailBackend> =
            ImapType::new(&settings, IsSubscribedFn::default(), event_consumer)?;
        Ok(Self {
            backend: Arc::new(Mutex::new(backend)),
            folders: Mutex::new(HashMap::new()),
            folder_index: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn mailbox_of(&self, folder: &str) -> Result<MailboxHash> {
        self.folders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|(p, _)| p.eq_ignore_ascii_case(folder))
            .map(|(_, h)| *h)
            .ok_or_else(|| BackendError::FolderNotFound(folder.to_string()))
    }

    fn batch_of(&self, hashes: &[u64]) -> Result<EnvelopeHashBatch> {
        let mut it = hashes.iter();
        let first = it
            .next()
            .ok_or_else(|| BackendError::Other("空哈希列表".into()))?;
        Ok(EnvelopeHashBatch {
            first: EnvelopeHash(*first),
            rest: it.map(|h| EnvelopeHash(*h)).collect(),
        })
    }

    /// Drafts 类文件夹（draft / 草稿；大小写不敏感）
    fn find_drafts_folder(&self) -> Result<String> {
        self.folders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .find(|p| {
                let l = p.to_lowercase();
                l.contains("draft") || l.contains("草稿")
            })
            .cloned()
            .ok_or_else(|| BackendError::FolderNotFound("Drafts".into()))
    }
}

/// mUTF-7 → UTF-8。melib 自带 utf7 模块但 IMAP 后端不用它，邮箱名原样透传，
/// QQ 邮箱的中文文件夹会以 "&UXZO1mWHTvZZOQ-" 形式漏到 UI（修复：2026-10 dogfood）。
/// decode 内部对畸形输入会 unwrap panic——这是服务器可控输入，catch 住回退原名
fn folder_name_decode(raw: &str) -> String {
    std::panic::catch_unwind(|| melib::backends::utf7::decode_utf7_imap(raw))
        .unwrap_or_else(|_| raw.to_string())
}

/// melib IMAP 缓存（sqlite3 feature）落盘在 `$XDG_DATA_HOME/meli/`，且路径不可配。
/// 进程首次建后端前把未设置的 XDG_DATA_HOME 归拢到 s-mail 数据根
///（SMAIL_HOME 或 ~/.s-mail），维持“数据都在自己家”的纪律；用户已显式设置则尊重。
/// Once 保证多服务线程并发时只设一次（set_var 非线程安全）
fn ensure_xdg_data_home() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("XDG_DATA_HOME").is_some() {
            return;
        }
        let base = std::env::var_os("SMAIL_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let mut p =
                    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
                p.push(".s-mail");
                p
            });
        let _ = std::fs::create_dir_all(&base);
        std::env::set_var("XDG_DATA_HOME", base);
    });
}

impl MailBackend for MelibBackend {
    fn drafts_folder_name(&self) -> Option<String> {
        self.find_drafts_folder().ok()
    }

    fn ensure_archive_folder(&self) -> Result<String> {
        // archive/归档/All Mail（Gmail 的 All Mail 即归档语义）
        if let Some(existing) = self.folders_lock().keys().find(|p| {
            p.to_lowercase().contains("archive")
                || p.contains("归档")
                || p.to_lowercase().contains("all mail")
        }) {
            return Ok(existing.clone());
        }
        let name = "Archive".to_string();
        self.create_folder(&name)?;
        Ok(name)
    }

    fn ensure_junk_folder(&self) -> Result<String> {
        if let Some(existing) = self.folders_lock().keys().find(|p| {
            let l = p.to_lowercase();
            l.contains("junk") || l.contains("spam") || p.contains("垃圾")
        }) {
            return Ok(existing.clone());
        }
        let name = "Junk".to_string();
        self.create_folder(&name)?;
        Ok(name)
    }

    fn ensure_trash_folder(&self) -> Result<String> {
        if let Some(existing) = self
            .folders_lock()
            .keys()
            .find(|p| folder_kind(p) == FolderKind::Trash)
        {
            return Ok(existing.clone());
        }
        let name = "Trash".to_string();
        self.create_folder(&name)?;
        Ok(name)
    }

    fn is_online(&self) -> Result<()> {
        let fut = self.with_backend(|b| b.is_online());
        futures::executor::block_on(fut?)?;
        Ok(())
    }

    fn list_folders(&self) -> Result<Vec<String>> {
        let fut = self.with_backend(|b| b.mailboxes());
        let mailboxes = futures::executor::block_on(fut?)?;
        let mut names = Vec::with_capacity(mailboxes.len());
        self.folders_lock().clear();
        self.folder_index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        for (hash, mb) in &mailboxes {
            // 解码后的名字作为对外唯一形态；对服务器的所有操作都走 MailboxHash，
            // 不受编码影响（create/rename 是例外：发往服务器的名字要重编码）
            let path = folder_name_decode(mb.path());
            names.push(path.clone());
            self.folders_lock().insert(path.clone(), *hash);
            self.folder_index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(*hash, path);
        }
        names.sort();
        Ok(names)
    }

    fn fetch_envelopes(&self, folder: &str) -> Result<Vec<EnvelopeSummary>> {
        let hash = self.mailbox_of(folder)?;
        let mut stream = self.with_backend(|b| b.fetch(hash))?;
        let mut out = Vec::new();
        while let Some(batch) = futures::executor::block_on(stream.next()) {
            for env in batch? {
                out.push(EnvelopeSummary {
                    hash: env.hash().0,
                    message_id: env.message_id().to_string(),
                    // melib：in_reply_to() 是 Cow<References>，refs 链尾 = 直系父件
                    in_reply_to: env
                        .in_reply_to()
                        .and_then(|r| r.refs().last().map(|m| m.to_string()))
                        .unwrap_or_default(),
                    subject: env.subject().to_string(),
                    from_addr: env
                        .from()
                        .first()
                        .map(|a| a.to_string())
                        .unwrap_or_default(),
                    date_unix: env.date() as i64,
                    seen: env.is_seen(),
                    flagged: env.flags().contains(Flag::FLAGGED),
                });
            }
        }
        Ok(out)
    }

    fn fetch_body(&self, hash: u64) -> Result<Vec<u8>> {
        let fut = self.with_backend(|b| b.envelope_bytes_by_hash(EnvelopeHash(hash)));
        Ok(futures::executor::block_on(fut?)?)
    }

    fn watch(&self) -> Result<std::sync::mpsc::Receiver<ServerEvent>> {
        let index = self.folder_index.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut stream = self.with_backend(|b| b.watch())?;
        std::thread::spawn(move || {
            while let Some(ev) = futures::executor::block_on(stream.next()) {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(err) => {
                        let _ = tx.send(ServerEvent::ConnectionLost {
                            reason: format!("{err}"),
                        });
                        continue;
                    }
                };
                let folder = match &ev {
                    BackendEvent::Refresh(r) => index
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&r.mailbox_hash)
                        .cloned(),
                    _ => None,
                };
                if let Some(folder) = folder {
                    let _ = tx.send(ServerEvent::FolderChanged { folder });
                }
            }
            // 流正常结束（服务器关闭连接）也视为断线
            let _ = tx.send(ServerEvent::ConnectionLost {
                reason: "IDLE 结束（服务器关闭连接）".into(),
            });
        });
        Ok(rx)
    }

    // --- M2 ---

    fn set_flags(&self, folder: &str, hashes: &[u64], change: FlagChange) -> Result<()> {
        let mb = self.mailbox_of(folder)?;
        let batch = self.batch_of(hashes)?;
        let op = match change {
            FlagChange::Seen(true) => FlagOp::Set(Flag::SEEN),
            FlagChange::Seen(false) => FlagOp::UnSet(Flag::SEEN),
            FlagChange::Flagged(true) => FlagOp::Set(Flag::FLAGGED),
            FlagChange::Flagged(false) => FlagOp::UnSet(Flag::FLAGGED),
        };
        let fut = self.with_backend(|b| b.set_flags(batch, mb, vec![op]));
        futures::executor::block_on(fut?)?;
        Ok(())
    }

    fn delete_messages(&self, folder: &str, hashes: &[u64]) -> Result<()> {
        let mb = self.mailbox_of(folder)?;
        let batch = self.batch_of(hashes)?;
        let fut = self.with_backend(|b| b.delete_messages(batch, mb));
        futures::executor::block_on(fut?)?;
        Ok(())
    }

    fn move_messages(&self, folder: &str, hashes: &[u64], dest: &str) -> Result<()> {
        let src = self.mailbox_of(folder)?;
        let dst = self.mailbox_of(dest)?;
        let batch = self.batch_of(hashes)?;
        let fut = self.with_backend(|b| b.copy_messages(batch, src, dst, true));
        futures::executor::block_on(fut?)?;
        Ok(())
    }

    fn create_folder(&self, path: &str) -> Result<()> {
        // 服务器只认 mUTF-7：中文文件夹名编码后发（ASCII 原样透传）
        let encoded = encode_utf7_imap(path);
        let fut = self.with_backend(|b| b.create_mailbox(encoded));
        futures::executor::block_on(fut?)?;
        self.list_folders()?; // 刷新映射：新邮箱立即可选为目标
        Ok(())
    }

    fn delete_folder(&self, path: &str) -> Result<()> {
        let mb = self.mailbox_of(path)?;
        let fut = self.with_backend(|b| b.delete_mailbox(mb));
        futures::executor::block_on(fut?)?;
        self.folders_lock().remove(path);
        Ok(())
    }

    fn rename_folder(&self, path: &str, new_path: &str) -> Result<()> {
        let mb = self.mailbox_of(path)?;
        let encoded = encode_utf7_imap(new_path);
        let fut = self.with_backend(|b| b.rename_mailbox(mb, encoded));
        futures::executor::block_on(fut?)?;
        self.list_folders()?;
        Ok(())
    }

    fn append_draft(&self, bytes: Vec<u8>) -> Result<String> {
        let drafts = self.find_drafts_folder()?;
        let mb = self.mailbox_of(&drafts)?;
        let fut = self.with_backend(|b| b.save(bytes, mb, None));
        futures::executor::block_on(fut?)?;
        Ok(drafts)
    }

    fn delete_draft_by_mid(&self, message_id: &str) -> Result<bool> {
        let drafts = self.find_drafts_folder()?;
        // 拉一遍 Drafts 信封：刷新 melib 的 hash→UID 缓存，删除才能命中
        let envs = self.fetch_envelopes(&drafts)?;
        fn norm(s: &str) -> &str {
            s.trim().trim_start_matches('<').trim_end_matches('>')
        }
        let Some(e) = envs
            .iter()
            .find(|e| norm(&e.message_id) == norm(message_id))
        else {
            return Ok(false);
        };
        self.delete_messages(&drafts, &[e.hash])?;
        Ok(true)
    }

    fn append_to_sent(&self, bytes: Vec<u8>) -> Result<String> {
        let sent = self
            .folders_lock()
            .keys()
            .find(|p| {
                let l = p.to_lowercase();
                l.contains("sent") || l.contains("已发送") || l.contains("已发送消息")
            })
            .cloned()
            .ok_or_else(|| BackendError::FolderNotFound("Sent".into()))?;
        let mb = self.mailbox_of(&sent)?;
        let fut = self.with_backend(|b| b.save(bytes, mb, Some(Flag::SEEN)));
        futures::executor::block_on(fut?)?;
        Ok(sent)
    }
}

/// 便捷：连接 + 列文件夹
pub fn connect(cfg: &ImapConfig) -> Result<(MelibBackend, Vec<String>)> {
    let b = MelibBackend::new(cfg)?;
    b.is_online()?;
    let folders = b.list_folders()?;
    Ok((b, folders))
}

/// M6：配置引导的连接测试。成功返回文件夹数（只连接不建 watch，用完即弃）。
/// 错误翻译成人话（用户没有日志可看，这句话就是全部诊断信息）。
pub fn test_connection(cfg: &ImapConfig) -> std::result::Result<usize, String> {
    (|| -> std::result::Result<usize, String> {
        let b = MelibBackend::new(cfg).map_err(|e| e.to_string())?;
        b.is_online().map_err(|e| e.to_string())?;
        Ok(b.list_folders().map_err(|e| e.to_string())?.len())
    })()
    .map_err(|e| friendly_conn_error(cfg, &e))
}

fn friendly_conn_error(cfg: &ImapConfig, e: &str) -> String {
    let l = e.to_lowercase();
    if l.contains("connection refused") || l.contains("os error 61") {
        format!(
            "无法连接 {host}:{port}：服务器地址/端口错误，或服务未开放",
            host = cfg.host,
            port = cfg.port
        )
    } else if l.contains("timed out") {
        format!("连接 {host} 超时：检查网络或防火墙", host = cfg.host)
    } else if l.contains("authentication") || l.contains("login") || l.contains("password") {
        if cfg.host.contains("163.com") {
            "认证失败：163 需要先在网页版开启 IMAP 并用授权码（不是登录密码）".to_string()
        } else if cfg.host.contains("qq.com") {
            "认证失败：QQ 需要在网页版 设置→账户 生成授权码（不是QQ密码）".to_string()
        } else {
            "认证失败：账号或密码/授权码错误".to_string()
        }
    } else if l.contains("unsafe login") {
        "服务商拒绝登录（Unsafe Login）：需在网页版开启 IMAP/SMTP 授权".to_string()
    } else if l.contains("tls") || l.contains("certificate") {
        format!("TLS 失败：{e}（尝试核对端口：993=SSL，143=STARTTLS）")
    } else {
        e.to_string()
    }
}

// ---------------------------------------------------------------------------
// SMTP 发信（lettre）
// ---------------------------------------------------------------------------

/// 发送邮件。成功返回完整 RFC822 字节（含我们生成的 Message-ID），
/// 供上层 APPEND 到 Sent 文件夹（幂等键 = Message-ID）。
pub fn send_mail(cfg: &SmtpConfig, mail: &OutgoingMail) -> Result<Vec<u8>> {
    let message_id = format!(
        "<{}.{}@s-mail>",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        cfg.username
    );
    let email = build_mime_message(&message_id, mail)?;
    let sender = build_sender(cfg)?;
    use lettre::Transport;
    sender
        .transport
        .send(&email)
        .map_err(|e| BackendError::SmtpProtocol(format!("发送失败: {e}")))?;
    // 返回与发送内容一致的完整字节（lettre 的 formatted 输出）
    Ok(email.formatted())
}

/// 构造 SMTP transport（TLS 模式 + 凭据），send_mail / send_raw_mail 共用
fn build_transport(cfg: &SmtpConfig) -> Result<lettre::SmtpTransport> {
    use lettre::transport::smtp::authentication::{Credentials, Mechanism};
    use lettre::transport::smtp::client::{Tls, TlsParameters};

    let tls_mode = cfg.tls.as_deref().unwrap_or(match cfg.port {
        465 => "ssl",
        _ => "starttls",
    });
    let mut transport_builder = match tls_mode {
        "none" => lettre::SmtpTransport::builder_dangerous(&cfg.host).port(cfg.port),
        "ssl" => {
            let params = TlsParameters::new(cfg.host.clone())
                .map_err(|e| BackendError::TlsFailed(e.to_string()))?;
            lettre::SmtpTransport::builder_dangerous(&cfg.host)
                .port(cfg.port)
                .tls(Tls::Wrapper(params))
        }
        _ => lettre::SmtpTransport::relay(&cfg.host)
            .map_err(|e| BackendError::SmtpProtocol(format!("relay 失败: {e}")))?
            .port(cfg.port),
    };
    let password = cfg.password.clone().ok_or_else(|| {
        BackendError::ConfigError("缺少 smtp 密码（oauth 账号由上层填 token）".into())
    })?;
    transport_builder =
        transport_builder.credentials(Credentials::new(cfg.username.clone(), password));
    if cfg.auth.as_deref() == Some("xoauth2") {
        // lettre：secret 为裸 access token，XOAUTH2 串由 lettre 拼
        transport_builder = transport_builder.authentication(vec![Mechanism::Xoauth2]);
    }
    Ok(transport_builder.build())
}

/// 可复用 SMTP 发送器：lettre transport 内部维持连接，多封连发只需一次
/// TCP+TLS+AUTH 握手（旧路径每封重建，断网重试期间每 6s × N 封反复握手）
pub struct SmtpSender {
    transport: lettre::SmtpTransport,
    username: String,
}

pub fn build_sender(cfg: &SmtpConfig) -> Result<SmtpSender> {
    Ok(SmtpSender {
        transport: build_transport(cfg)?,
        username: cfg.username.clone(),
    })
}

impl SmtpSender {
    /// 同 [`send_mail`] 语义（分内容构造与发送两段；返回完整字节供归档 Sent）
    pub fn send(&self, mail: &OutgoingMail) -> Result<Vec<u8>> {
        let message_id = format!(
            "<{}.{}@s-mail>",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            self.username
        );
        let email = build_mime_message(&message_id, mail)?;
        use lettre::Transport;
        self.transport
            .send(&email)
            .map_err(|e| BackendError::SmtpProtocol(format!("发送失败: {e}")))?;
        Ok(email.formatted())
    }

    /// 同 [`send_raw_mail`] 语义（PGP 邮件由上层组好 MIME）
    pub fn send_raw(&self, from: &str, recipients: &[String], bytes: &[u8]) -> Result<()> {
        use lettre::address::{Address, Envelope};
        use lettre::Transport;
        let sender: Address = from
            .parse()
            .map_err(|e| BackendError::InvalidMailFormat(format!("发件人 {from:?} 无效: {e}")))?;
        let to: Vec<Address> = recipients
            .iter()
            .map(|r| {
                r.parse::<Address>()
                    .map_err(|e| BackendError::InvalidMailFormat(format!("收件人 {r:?} 无效: {e}")))
            })
            .collect::<std::result::Result<_, _>>()?;
        let envelope = Envelope::new(Some(sender), to)
            .map_err(|e| BackendError::InvalidMailFormat(format!("信封构造失败: {e}")))?;
        self.transport
            .send_raw(&envelope, bytes)
            .map_err(|e| BackendError::SmtpProtocol(format!("发送失败: {e}")))?;
        Ok(())
    }
}

/// 构造完整 MIME 邮件（send_mail / SmtpSender::send / 草稿上传共用）
fn build_mime_message(message_id: &str, mail: &OutgoingMail) -> Result<lettre::Message> {
    use lettre::message::header::ContentType;
    use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};

    let from: Mailbox = mail
        .from
        .parse()
        .map_err(|e| BackendError::InvalidMailFormat(format!("发件人地址无效: {e}")))?;
    let mut builder = Message::builder().from(from);
    for to in &mail.to {
        let m: Mailbox = to
            .parse()
            .map_err(|e| BackendError::InvalidMailFormat(format!("收件人 {to:?} 无效: {e}")))?;
        builder = builder.to(m);
    }
    for cc in &mail.cc {
        let m: Mailbox = cc
            .parse()
            .map_err(|e| BackendError::InvalidMailFormat(format!("抄送 {cc:?} 无效: {e}")))?;
        builder = builder.cc(m);
    }
    for bcc in &mail.bcc {
        let m: Mailbox = bcc
            .parse()
            .map_err(|e| BackendError::InvalidMailFormat(format!("密送 {bcc:?} 无效: {e}")))?;
        builder = builder.bcc(m);
    }
    let builder = builder
        .message_id(Some(message_id.to_string()))
        .subject(&mail.subject);

    let text_part = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(mail.body.clone());
    enum MaybeMultipart {
        Plain(SinglePart),
        Multi(MultiPart),
    }
    let body = if mail.attachments.is_empty() {
        MaybeMultipart::Plain(text_part)
    } else {
        let mut multipart = MultiPart::mixed().singlepart(text_part);
        for (name, bytes) in &mail.attachments {
            let mime = mime_guess::from_path(name)
                .first_or_octet_stream()
                .to_string();
            let ct = ContentType::parse(&mime).map_err(|e| {
                BackendError::InvalidMailFormat(format!("附件 {name:?} 类型无效: {e}"))
            })?;
            multipart = multipart.singlepart(Attachment::new(name.clone()).body(bytes.clone(), ct));
        }
        MaybeMultipart::Multi(multipart)
    };
    match body {
        MaybeMultipart::Plain(p) => builder.singlepart(p),
        MaybeMultipart::Multi(m) => builder.multipart(m),
    }
    .map_err(|e| BackendError::InvalidMailFormat(format!("构造邮件失败: {e}")))
}

/// 构造草稿完整字节（B2 云同步：不走 SMTP，只组 MIME；Message-ID 由调用方
/// 确定性生成以便后续按 ID 删服务器副本）
pub fn build_draft_bytes(mail: &OutgoingMail, message_id: &str) -> Result<Vec<u8>> {
    Ok(build_mime_message(message_id, mail)?.formatted())
}

/// 发送上层构造好的完整 RFC822 字节（M5-d：PGP 邮件由 smail-core 组好 MIME）
pub fn send_raw_mail(
    cfg: &SmtpConfig,
    from: &str,
    recipients: &[String],
    bytes: &[u8],
) -> Result<()> {
    build_sender(cfg)?.send_raw(from, recipients, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mail() -> OutgoingMail {
        OutgoingMail {
            from: "me@local".into(),
            to: vec!["a@x.com".into(), "b@x.com".into()],
            cc: vec!["c@x.com".into()],
            bcc: vec!["secret-bcc@x.com".into()],
            subject: "主题报告".into(),
            body: "正文内容".into(),
            attachments: vec![],
        }
    }

    #[test]
    fn folder_kind_common_names() {
        use FolderKind::*;
        assert_eq!(folder_kind("INBOX"), Inbox);
        assert_eq!(folder_kind("inbox"), Inbox);
        assert_eq!(folder_kind("Drafts"), Drafts);
        assert_eq!(folder_kind("my-draft-box"), Drafts);
        assert_eq!(folder_kind("草稿箱"), Drafts);
        assert_eq!(folder_kind("Sent"), Sent);
        assert_eq!(folder_kind("已发送"), Sent);
        assert_eq!(folder_kind("Junk"), Junk);
        assert_eq!(folder_kind("Spam"), Junk);
        assert_eq!(folder_kind("垃圾邮件"), Junk);
        assert_eq!(folder_kind("Archive"), Archive);
        assert_eq!(folder_kind("归档"), Archive);
        assert_eq!(folder_kind("[Gmail]/All Mail"), Archive);
        assert_eq!(folder_kind("Trash"), Trash);
        assert_eq!(folder_kind("Deleted Items"), Trash);
        assert_eq!(folder_kind("已删除"), Trash);
        assert_eq!(folder_kind("废纸篓"), Trash);
        assert_eq!(folder_kind("Projects/alpha"), Plain);
        // 顺序即语义：junk/spam 先于 trash 判定，含 "deleted" 的自定义
        // 垃圾名（"Deleted-Junk"）归 Junk——与实现顺序一致地钉住
        assert_eq!(folder_kind("Deleted-Junk"), Junk);
    }

    #[test]
    fn backend_new_rejects_missing_password_before_network() {
        let mut cfg = ImapConfig {
            host: "127.0.0.1".into(),
            port: 1143,
            username: "u".into(),
            password: None,
            starttls: false,
            tls: false,
            use_oauth2: false,
        };
        match MelibBackend::new(&cfg) {
            Err(BackendError::ConfigError(msg)) => {
                assert!(msg.contains("密码"), "报错应提示密码：{msg}")
            }
            Err(other) => panic!("错误类型不对：{other}"),
            Ok(_) => panic!("不应成功"),
        }
        cfg.use_oauth2 = true;
        match MelibBackend::new(&cfg) {
            Err(BackendError::ConfigError(msg)) => assert!(
                msg.contains("oauth2") || msg.contains("SASL"),
                "oauth2 提示：{msg}"
            ),
            Err(other) => panic!("错误类型不对：{other}"),
            Ok(_) => panic!("不应成功"),
        }
    }

    #[test]
    fn mime_headers_to_cc_present_and_subject_encoded() {
        let bytes = build_draft_bytes(&sample_mail(), "<t1@x>").unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("To: "), "应有 To 头\n{s}");
        assert!(s.contains("a@x.com") && s.contains("b@x.com"));
        assert!(s.contains("Cc: c@x.com"));
        // RFC 2047：非 ASCII 主题必须是 encoded-word（B/Q 编码大小写均可），
        // 不裸写 UTF-8（严格服务器/客户端会拒收）
        let subject_line = s.lines().find(|l| l.starts_with("Subject:")).unwrap_or("");
        let subj_lower = subject_line.to_lowercase();
        assert!(
            subj_lower.contains("=?utf-8?b?") || subj_lower.contains("=?utf-8?q?"),
            "中文主题应 encoded-word：{subject_line}"
        );
        assert!(!s.contains("Subject: 主题报告"));
    }

    #[test]
    fn mime_bcc_privacy_on_the_wire() {
        // 属性：发往服务器的字节绝不能含 Bcc 收件人地址（收件人不可见谁被密送）。
        // OutgoingMail.bcc 的契约：「进信封收件人不进 MIME 头」。lettre 的
        // transport.send 使用 envelope(RCPT TO 含 bcc) + formatted() 作为 DATA；
        // 本测试钉住 formatted() 输出不含 bcc 地址——若未来升级 lettre 后
        // 变了行为，这里会立刻红。
        let bytes = build_draft_bytes(&sample_mail(), "<t2@x>").unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            !s.contains("secret-bcc@x.com"),
            "Bcc 地址泄漏进邮件字节（发途/存 Sent 副本都会泄漏给后续读者）\n{s}"
        );
    }

    #[test]
    fn mime_attachment_multipart_and_filename() {
        let mut mail = sample_mail();
        mail.attachments
            .push(("报表.pdf".into(), b"%PDF-fake".to_vec()));
        let bytes = build_draft_bytes(&mail, "<t3@x>").unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("multipart/mixed"));
        assert!(s.contains("application/pdf"));
        // 文件名非 ASCII：encoded-word / RFC 2231，不裸写
        assert!(!s.contains("filename=\"报表\""));
    }

    /// QQ 邮箱真实样本：其他文件夹 = &UXZO1mWHTvZZOQ-（UTF-16BE + base64 + mUTF-7）
    #[test]
    fn folder_names_utf7_roundtrip() {
        assert_eq!(folder_name_decode("&UXZO1mWHTvZZOQ-"), "其他文件夹");
        assert_eq!(encode_utf7_imap("其他文件夹"), "&UXZO1mWHTvZZOQ-");
        // ASCII 名字原样透传
        assert_eq!(folder_name_decode("INBOX"), "INBOX");
        assert_eq!(folder_name_decode("Sent Messages"), "Sent Messages");
        assert_eq!(encode_utf7_imap("INBOX"), "INBOX");
        // &- 是字面 &（RFC 3501 转义）；混合名局部编码
        assert_eq!(folder_name_decode("A&-B"), "A&B");
        assert_eq!(folder_name_decode("其他&UXZO1mWHTvZZOQ-"), "其他其他文件夹");
        // 畸形输入不 panic（decode 内部 unwrap，服务器可控输入必须兑底）：
        // 未闭合段、非法 base64 字符都回退原名
        assert_eq!(folder_name_decode("&!!!"), "&!!!");
        assert_eq!(folder_name_decode("&??-"), "&??-");
    }
}
