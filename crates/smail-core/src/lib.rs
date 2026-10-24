//! smail-core：邮件客户端核心服务
//!
//! ## 功能矩阵
//!
//! - **M1**：收信同步 / 正文懒加载 / IDLE 推送
//! - **M2**：SMTP 发信 + outbox 队列 / 标记双向同步 / 断线重连 / 本地草稿
//! - **M3**：FTS 全文搜索（bigram）+ 后台索引器 + 附件收发
//! - **M5**：PGP 签名/加密 / OAuth 2.0 设备流 + 令牌刷新
//! - **M6**：多账号支持（独立数据目录 + 统一事件流）
//! - **M7**：批量操作 / 文件夹管理 / 联系人补全
//!
//! ## 架构
//!
//! ```text
//! Session (多账号)
//!   ├─> AccountService (账号0) ─┬─> IMAP Backend (收信/同步)
//!   │                           ├─> SMTP Backend (发信)
//!   │                           ├─> Store (SQLite + FTS)
//!   │                           └─> Reader Thread (正文取拉)
//!   │
//!   ├─> AccountService (账号1) ─┬─> ...
//!   └─> ...
//! ```
//!
//! ## 模块划分
//!
//! - `config` - 账号配置管理（TOML 加载/保存，OAuth 集成）
//! - `session` - 多账号会话管理（命令路由 + 事件流多路复用）
//! - `protocol` - 通道协议（Cmd/Event 定义）
//! - `reader` - 邮件正文读取线程（缓存 + 解析 + PGP）
//! - `lib.rs` - AccountService 主循环（IMAP 同步/IDLE/命令处理）
//!
//! ## 同步策略
//!
//! 传输层：melib 启用 sqlite3 离线缓存，FETCH 走 CONDSTORE/QRESYNC 增量
//! （对上层仍全量返回）。上层做全量信封 diff：新信入库、flags 随服务器更新、
//! 本地有而服务器无 → 删本地行（服务端删除传播，m13 e2e 覆盖）；
//! UIDVALIDITY 变化时全量集合同样收敛正确。

mod error;

pub mod config;
pub mod html;
pub mod keychain;
pub mod oauth;
pub mod pgp;
pub mod protocol;
pub mod reader;
mod service;
pub mod session;
pub mod threads;
pub mod wkd;

pub use error::{CoreError, Result, StartError};

use pgp::PgpCfg;

use oauth::OAuthCfg;

use config::SyncConfig;

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;

pub use smail_backend::{test_connection, FolderKind, ImapConfig, SmtpConfig};
use smail_backend::{MailBackend, ServerEvent};

// Re-exports from new modules
pub use config::{load_accounts, save_accounts, slug_for, AccountConfig, Rule};
pub use protocol::{Cmd, Event, FolderInfo};
pub use reader::{extract_body, ReaderCtx};
pub use session::{Session, SessionCmd, TaggedEvent};

// 内部使用的辅助函数

#[derive(Debug)]
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

impl AccountService {
    /// 旧入口（examples 用）：单账号配置文件
    pub fn start(
        cfg_path: &Path,
        data_dir: &Path,
    ) -> std::result::Result<(Self, tokio::sync::mpsc::UnboundedReceiver<Event>), StartError> {
        let mut accounts = load_accounts(cfg_path)?;
        let first = accounts.swap_remove(0);
        Self::start_for(&first, data_dir)
    }

    /// 多账号入口：接收已解析配置
    pub fn start_for(
        cfg: &AccountConfig,
        data_dir: &Path,
    ) -> std::result::Result<(Self, tokio::sync::mpsc::UnboundedReceiver<Event>), StartError> {
        // B6：keychain 标记 → 真实密码（只在此处解析进内存；save_accounts 写回标记）
        let imap_password = keychain::resolve_password(cfg.imap.password.clone(), "IMAP 密码")
            .map_err(|e| StartError::Core(CoreError::KeychainFailed(e.to_string())))?;
        let smtp_password = keychain::resolve_password(
            cfg.smtp.as_ref().and_then(|s| s.password.clone()),
            "SMTP 密码",
        )
        .map_err(|e| StartError::Core(CoreError::KeychainFailed(e.to_string())))?;
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
            sync_config: cfg.sync_config.clone(),
        };
        std::fs::create_dir_all(data_dir)?;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let data_dir = data_dir.to_path_buf();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = stop.clone();
        // Reader 线程（正文取拉）：承接 OpenMessage 完整管线，大附件 FETCH 不阻塞命令循环
        let (read_tx, read_rx) = std::sync::mpsc::channel::<i64>();
        let backend_slot = Arc::new(std::sync::RwLock::new(None::<Arc<dyn MailBackend>>));
        let zh_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        // P2 优化：backend 就绪通知机制（Condvar 替代轮询）
        let backend_ready = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let reader_pgp = match &cfg.pgp {
            Some(p) => pgp::PgpRuntime::load(p).ok().map(std::sync::Arc::new),
            None => None,
        };
        {
            let reader = ReaderCtx::new(
                backend_slot.clone(),
                event_tx.clone(),
                data_dir.clone(),
                reader_pgp,
                zh_flag.clone(),
                cfg.sync_config.reader_cold_start_timeout(),
                backend_ready.clone(),
            );
            std::thread::spawn(move || reader.run(read_rx));
        }
        let handle = std::thread::spawn(move || {
            ServiceInner {
                cfg,
                data_dir,
                zh: zh_flag,
                backend_slot,
                backend_ready,
                read_tx,
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

/// 列表单次返回上限（触底 LoadMore 逐步扩大；见 folder_limit）
pub const UI_LIST_LIMIT: u32 = 500;

/// 内部运行时配置（已解析 keychain 密码）
struct AppConfig {
    imap: ImapConfig,
    smtp: Option<SmtpConfig>,
    oauth: Option<OAuthCfg>,
    rules: Vec<Rule>,
    pgp: Option<PgpCfg>,
    identity: String,
    sync_config: SyncConfig,
}

struct ServiceInner {
    cfg: AppConfig,
    data_dir: PathBuf,
    /// UI 语言（zh=true；状态/错误文案跟随，SetLang 运行时切换）。
    /// Arc<AtomicBool>：Reader 线程（正文管线）与主循环共享同一份
    zh: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 当前在线 backend 的共享槽：Reader 每次请求时 clone 一份。
    /// 断线清 None（旧 backend 的连接已死，取拉直接报错而不是悬挂）
    backend_slot: Arc<std::sync::RwLock<Option<Arc<dyn MailBackend>>>>,
    /// backend 就绪通知：连接成功时 notify Reader 线程（P2 优化：Condvar 替代轮询）
    backend_ready: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    /// Reader 线程请求端：OpenMessage 分流（hash）
    read_tx: Sender<i64>,
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
        backend: Arc<dyn MailBackend>,
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
        tr_str(self.zh.load(std::sync::atomic::Ordering::Relaxed), zh, en)
    }
    fn trf(&self, zh: &'static str, en: &'static str, args: &[&dyn std::fmt::Display]) -> String {
        trf_str(
            self.zh.load(std::sync::atomic::Ordering::Relaxed),
            zh,
            en,
            args,
        )
    }

    /// PGP 运行时（带缓存）：密钥环盘读+证书解析贵，每封重载浪费；
    /// 未配置返回 Ok(None)。加载失败不缓存（下次重试）
    fn pgp_runtime(&self) -> std::result::Result<Option<std::sync::Arc<pgp::PgpRuntime>>, String> {
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
}

// 双语辅助函数
fn tr_str(zh: bool, zh_text: &str, en_text: &str) -> String {
    if zh {
        zh_text.to_string()
    } else {
        en_text.to_string()
    }
}

fn trf_str(zh: bool, zh_tpl: &str, en_tpl: &str, args: &[&dyn std::fmt::Display]) -> String {
    let tpl = if zh { zh_tpl } else { en_tpl };
    let mut out = tpl.to_string();
    for arg in args {
        if let Some(pos) = out.find("{}") {
            out.replace_range(pos..pos + 2, &arg.to_string());
        }
    }
    out
}
