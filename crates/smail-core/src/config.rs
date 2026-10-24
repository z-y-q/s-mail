//! 账号配置管理
//!
//! ## 职责
//!
//! - 从 config.toml 加载账号配置（支持多账号 `[[accounts]]` 和单账号 `[account]`）
//! - 保存配置到磁盘
//! - OAuth 令牌管理（从钥匙串读取/自动刷新）
//! - 账号 slug 生成（用于数据目录隔离）
//!
//! ## 配置文件格式
//!
//! 支持两种格式：
//! ```toml
//! # 多账号（推荐）
//! [[accounts]]
//! name = "Work"
//! identity = "user@example.com"
//! # ...
//!
//! # 单账号（向后兼容）
//! [account]
//! name = "Personal"
//! # ...
//! ```
//!
//! ## 启动流程
//!
//! ```text
//! load_accounts()
//!   ↓
//! 解析 TOML → AccountConfig
//!   ↓
//! OAuth 账号：从钥匙串读取 token（自动刷新过期令牌）
//!   ↓
//! 密码账号：从钥匙串读取密码（或使用明文配置）
//! ```

use crate::error::CoreError;
pub use crate::error::StartError;
use crate::oauth::OAuthCfg;
use crate::pgp::PgpCfg;
use smail_backend::{ImapConfig, SmtpConfig};
use smail_store::MessageRow;
use std::path::Path;
use std::time::Duration;

/// 同步行为配置（可由用户调整）
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SyncConfig {
    /// 文件夹重同步间隔（秒），IDLE 推送期间不主动重拉，避免冗余往返
    #[serde(default = "default_folder_resync_ttl")]
    pub folder_resync_ttl_secs: u64,

    /// 单次索引同步的文件夹批次大小
    #[serde(default = "default_index_batch")]
    pub index_batch: u32,

    /// 重连退避最大间隔（秒）
    #[serde(default = "default_max_backoff")]
    pub max_backoff_secs: u64,

    /// Reader 线程冷启动等待超时（毫秒）
    #[serde(default = "default_reader_cold_start_timeout")]
    pub reader_cold_start_timeout_ms: u64,
}

fn default_folder_resync_ttl() -> u64 {
    60
}
fn default_index_batch() -> u32 {
    5
}
fn default_max_backoff() -> u64 {
    60
}
fn default_reader_cold_start_timeout() -> u64 {
    1000
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            folder_resync_ttl_secs: default_folder_resync_ttl(),
            index_batch: default_index_batch(),
            max_backoff_secs: default_max_backoff(),
            reader_cold_start_timeout_ms: default_reader_cold_start_timeout(),
        }
    }
}

impl SyncConfig {
    pub fn folder_resync_ttl(&self) -> Duration {
        Duration::from_secs(self.folder_resync_ttl_secs)
    }

    pub fn max_backoff(&self) -> Duration {
        Duration::from_secs(self.max_backoff_secs)
    }

    pub fn reader_cold_start_timeout(&self) -> Duration {
        Duration::from_millis(self.reader_cold_start_timeout_ms)
    }
}

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
    pub fn matches(&self, m: &MessageRow) -> bool {
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

    pub fn has_action(&self) -> bool {
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
    /// 同步行为配置（可选，缺省使用默认值）
    #[serde(default)]
    sync: SyncConfig,
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
    /// 同步行为配置
    pub sync_config: SyncConfig,
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
        .map_err(|_| CoreError::ConfigMissing(cfg_path.to_path_buf()))?;
    let file: FileCfgV2 = toml::from_str(&raw).map_err(|e| {
        CoreError::ConfigParseFailed(format!(
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
            sync_config: file.sync.clone(),
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
            sync_config: file.sync.clone(),
        });
    }
    if out.is_empty() {
        return Err(StartError::Core(CoreError::ConfigParseFailed(
            "配置里没有任何账号".into(),
        )));
    }
    // 规则校验：无匹配条件或无动作的规则直接报配置错（静默失效更糟）
    for (i, a) in out.iter().enumerate() {
        for (j, r) in a.rules.iter().enumerate() {
            if r.from.is_none() && r.subject.is_none() {
                return Err(StartError::Core(CoreError::ConfigParseFailed(format!(
                    "账号#{} 规则#{} 缺少 from/subject 匹配条件",
                    i + 1,
                    j + 1
                ))));
            }
            if !r.has_action() {
                return Err(StartError::Core(CoreError::ConfigParseFailed(format!(
                    "账号#{} 规则#{} 没有任何动作（mark_seen/flag/move_to）",
                    i + 1,
                    j + 1
                ))));
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
        .map_err(|e| CoreError::ConfigParseFailed(format!("序列化配置失败: {e}")))?;
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CoreError::ConfigParseFailed(format!("建配置目录失败: {e}")))?;
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
            .map_err(|e| CoreError::ConfigParseFailed(format!("写配置失败: {e}")))?
    };
    #[cfg(not(unix))]
    let mut f = std::fs::File::create(&tmp)
        .map_err(|e| CoreError::ConfigParseFailed(format!("写配置失败: {e}")))?;
    use std::io::Write as _;
    f.write_all(body.as_bytes())
        .map_err(|e| CoreError::ConfigParseFailed(format!("写配置失败: {e}")))?;
    drop(f);
    std::fs::rename(&tmp, cfg_path)
        .map_err(|e| CoreError::ConfigParseFailed(format!("替换配置失败: {e}")))?;
    Ok(())
}
