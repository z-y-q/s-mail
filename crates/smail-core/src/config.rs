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

/// M5-b：过滤规则（同步时对新见到的邮件应用，天然幂等）。
/// v2 条件模型：all_of（全部满足）/any_of（任一满足，两组并存时都要过）、
/// 多动作、命中链控制；v0 旧字段（from/subject/mark_seen/flag/move_to）
/// 加载时归一化进 v2 结构，保存只写 v2（首次保存即自迁移）
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Rule {
    /// 全部满足（空=不约束）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_of: Vec<RuleCondition>,
    /// 任一满足（空=不约束；与 all_of 并存时两组都要过）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any_of: Vec<RuleCondition>,
    /// 动作列表（一条规则可多动作，按序执行）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<RuleAction>,
    /// 命中后是否继续试后续规则（默认 false=旧「首条命中即用」语义；
    /// true 时同一封邮件可叠加多条规则的动作）
    #[serde(default, skip_serializing_if = "is_false")]
    pub continue_after: bool,
    /// 只在该文件夹应用（缺省=任意文件夹）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    // ---- v0 旧格式输入（load 归一化，不回写）----
    /// 发件人子串（不区分大小写）→ all_of(from contains)
    #[serde(default, skip_serializing)]
    pub from: Option<String>,
    /// 主题子串（不区分大小写）→ all_of(subject contains)
    #[serde(default, skip_serializing)]
    pub subject: Option<String>,
    #[serde(default, skip_serializing)]
    pub mark_seen: bool,
    #[serde(default, skip_serializing)]
    pub flag: bool,
    /// 移动到该服务器文件夹 → actions(Move)
    #[serde(default, skip_serializing)]
    pub move_to: Option<String>,
}

/// 单个匹配条件（字段 × 谓词 × 值；比较一律不区分大小写）
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RuleCondition {
    pub field: RuleField,
    pub predicate: RulePredicate,
    pub value: String,
}

/// 匹配字段（to 需要库里没有的收件人列，暂不支持—— serde 拒绝，明而不默）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleField {
    From,
    Subject,
}

/// 谓词
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RulePredicate {
    Contains,
    Equals,
    StartsWith,
    EndsWith,
}

/// 动作
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum RuleAction {
    MarkSeen,
    Flag,
    /// 移动到服务器文件夹（目的地=当前文件夹时跳过，防自移动死循环）
    Move(String),
}

impl Rule {
    /// v0 旧字段归一化进 v2 结构（已是 v2 的不动）。load 后调用
    pub(crate) fn normalize_v0(&mut self) {
        if !self.all_of.is_empty() || !self.any_of.is_empty() || !self.actions.is_empty() {
            return;
        }
        if let Some(f) = self.from.take() {
            self.all_of.push(RuleCondition {
                field: RuleField::From,
                predicate: RulePredicate::Contains,
                value: f,
            });
        }
        if let Some(s) = self.subject.take() {
            self.all_of.push(RuleCondition {
                field: RuleField::Subject,
                predicate: RulePredicate::Contains,
                value: s,
            });
        }
        if self.mark_seen {
            self.actions.push(RuleAction::MarkSeen);
        }
        if self.flag {
            self.actions.push(RuleAction::Flag);
        }
        if let Some(dest) = self.move_to.take() {
            self.actions.push(RuleAction::Move(dest));
        }
    }

    /// 匹配（不含 folder 守卫——由调用方判，语义与旧版一致）
    pub fn matches(&self, m: &MessageRow) -> bool {
        if !self.all_of.iter().all(|c| cond_matches(c, m)) {
            return false;
        }
        if !self.any_of.is_empty() && !self.any_of.iter().any(|c| cond_matches(c, m)) {
            return false;
        }
        true
    }

    pub fn has_action(&self) -> bool {
        !self.actions.is_empty()
    }
}

/// 单条件匹配（一律不区分大小写，与旧版子串语义一致）。
/// From 字段匹配用裸地址：from_addr 存的是 “显示名 <a@b>” 全形态，
/// 直接 ends_with/equals 会撞上尖括号（旧 contains 碰巧能用，谓词化后必须别开）
fn cond_matches(c: &RuleCondition, m: &MessageRow) -> bool {
    let text = match c.field {
        RuleField::From => crate::reader::parse_from_addr(&m.from_addr).0,
        RuleField::Subject => m.subject.clone(),
    };
    let (t, v) = (text.to_lowercase(), c.value.to_lowercase());
    match c.predicate {
        RulePredicate::Contains => t.contains(&v),
        RulePredicate::Equals => t == v,
        RulePredicate::StartsWith => t.starts_with(&v),
        RulePredicate::EndsWith => t.ends_with(&v),
    }
}

fn is_false(b: &bool) -> bool {
    !b
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
    // 规则归一化 + 校验：v0 字段转 v2；无匹配条件或无动作的规则直接报
    // 配置错（静默失效更糟）
    for (i, a) in out.iter_mut().enumerate() {
        for (j, r) in a.rules.iter_mut().enumerate() {
            r.normalize_v0();
            if r.all_of.is_empty() && r.any_of.is_empty() {
                return Err(StartError::Core(CoreError::ConfigParseFailed(format!(
                    "账号#{} 规则#{} 缺少匹配条件（all_of/any_of 或旧格式 from/subject）",
                    i + 1,
                    j + 1
                ))));
            }
            if !r.has_action() {
                return Err(StartError::Core(CoreError::ConfigParseFailed(format!(
                    "账号#{} 规则#{} 没有任何动作（actions 或旧格式 mark_seen/flag/move_to）",
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

#[cfg(test)]
mod rule_tests {
    use super::*;

    fn row(from: &str, subject: &str) -> MessageRow {
        MessageRow {
            envelope_hash: 1,
            folder: "INBOX".into(),
            message_id: String::new(),
            subject: subject.into(),
            from_addr: from.into(),
            date_unix: 0,
            seen: false,
            flagged: false,
            has_body: false,
            in_reply_to: String::new(),
        }
    }

    fn cond(field: RuleField, predicate: RulePredicate, value: &str) -> RuleCondition {
        RuleCondition {
            field,
            predicate,
            value: value.into(),
        }
    }

    /// From 匹配用裸地址：from_addr 是 “名字 <a@b>” 全形态，谓词不能撞尖括号
    #[test]
    fn from_field_matches_bare_address() {
        let mut r = toml::from_str::<Rule>(
            r#"
all_of = [{ field = "from", predicate = "ends_with", value = "@v2test.x" }]
actions = ["MarkSeen"]
"#,
        )
        .unwrap();
        r.normalize_v0();
        // 存储形态：显示名 + 尖括号（melib 产出）
        assert!(r.matches(&row("v2new <v2new@v2test.x>", "s")));
        assert!(r.matches(&row("v2new@v2test.x", "s")));
        assert!(!r.matches(&row("v2new <v2new@v2test.x.evil>", "s")));
        // equals 也用裸地址
        let mut eq = toml::from_str::<Rule>(
            r#"
all_of = [{ field = "from", predicate = "equals", value = "boss@corp.x" }]
actions = ["MarkSeen"]
"#,
        )
        .unwrap();
        eq.normalize_v0();
        assert!(eq.matches(&row("老板 <Boss@Corp.X>", "s")));
    }

    /// v0 旧格式加载→归一化→匹配，语义与旧版完全一致（子串、不区分大小写、AND）
    #[test]
    fn v0_format_normalizes_and_matches_like_old() {
        let mut r = toml::from_str::<Rule>(
            r#"from = "PROMO@"
subject = "优惠"
mark_seen = true
flag = true
move_to = "Archive"
"#,
        )
        .unwrap();
        r.normalize_v0();
        assert_eq!(
            r.all_of,
            vec![
                cond(RuleField::From, RulePredicate::Contains, "PROMO@"),
                cond(RuleField::Subject, RulePredicate::Contains, "优惠"),
            ]
        );
        assert_eq!(
            r.actions,
            vec![
                RuleAction::MarkSeen,
                RuleAction::Flag,
                RuleAction::Move("Archive".into())
            ]
        );
        // 大小写不敏感子串（旧语义）
        assert!(r.matches(&row("noreply@promo@x.com", "限时优惠大促")));
        assert!(!r.matches(&row("other@x.com", "限时优惠大促")));
        assert!(!r.matches(&row("promo@x.com", "无关主题")));
    }

    /// v2 四谓词，一律不区分大小写
    #[test]
    fn v2_predicates() {
        let base = r#"
actions = ["MarkSeen"]
"#;
        let mk = |all_of: &str| -> Rule {
            let src = format!("{base}\nall_of = [{all_of}]\n");
            let mut r = toml::from_str::<Rule>(&src).unwrap();
            r.normalize_v0();
            r
        };
        let m = &row("Boss@Corp.X", "周报汇总第10期");

        assert!(
            mk(r#"{ field = "subject", predicate = "starts_with", value = "周报" }"#).matches(m)
        );
        assert!(
            mk(r#"{ field = "subject", predicate = "ends_with", value = "第10期" }"#).matches(m)
        );
        assert!(
            mk(r#"{ field = "from", predicate = "equals", value = "boss@corp.x" }"#).matches(m)
        );
        assert!(
            !mk(r#"{ field = "from", predicate = "equals", value = "boss@corp.x" }"#)
                .matches(&row("boss@corp.x.evil", "x"))
        );
        assert!(
            !mk(r#"{ field = "subject", predicate = "starts_with", value = "汇总" }"#).matches(m)
        );
    }

    /// any_of 任一 + 与 all_of 并存时都要过
    #[test]
    fn v2_any_of_semantics() {
        let mut r = toml::from_str::<Rule>(
            r#"
all_of = [{ field = "from", predicate = "ends_with", value = "@corp.x" }]
any_of = [
  { field = "subject", predicate = "contains", value = "周报" },
  { field = "subject", predicate = "contains", value = "月报" },
]
actions = ["Flag"]
"#,
        )
        .unwrap();
        r.normalize_v0();
        assert!(r.matches(&row("a@corp.x", "本周周报")));
        assert!(r.matches(&row("a@corp.x", "十月月报")));
        // any_of 无一命中
        assert!(!r.matches(&row("a@corp.x", "垃圾")));
        // all_of 不过，any_of 命中也不行
        assert!(!r.matches(&row("a@evil.x", "本周周报")));
    }

    /// 保存只写 v2：归一化后的规则序列化不含旧字段（自迁移）
    #[test]
    fn normalized_rule_serializes_v2_only() {
        let mut r = toml::from_str::<Rule>(
            r#"from = "noreply@"
mark_seen = true
"#,
        )
        .unwrap();
        r.normalize_v0();
        let toml_out = toml::to_string(&r).unwrap();
        assert!(toml_out.contains("all_of"), "{toml_out}");
        assert!(toml_out.contains("MarkSeen"), "{toml_out}");
        assert!(!toml_out.contains("from ="), "{toml_out}");
        assert!(!toml_out.contains("mark_seen"), "{toml_out}");
        // 往返：v2 输出可再解析回等价规则
        let back: Rule = toml::from_str(&toml_out).unwrap();
        assert_eq!(
            back.matches(&row("x@noreply@y", "s")),
            r.matches(&row("x@noreply@y", "s"))
        );
    }

    /// continue_after 默认 false（旧「首条命中即用」）
    #[test]
    fn continue_after_defaults_false() {
        let r: Rule = toml::from_str::<Rule>(
            r#"
all_of = [{ field = "subject", predicate = "contains", value = "x" }]
actions = ["MarkSeen"]
"#,
        )
        .unwrap();
        assert!(!r.continue_after);
        let r2: Rule = toml::from_str::<Rule>(
            r#"
all_of = [{ field = "subject", predicate = "contains", value = "x" }]
actions = ["MarkSeen"]
continue_after = true
"#,
        )
        .unwrap();
        assert!(r2.continue_after);
    }
}
