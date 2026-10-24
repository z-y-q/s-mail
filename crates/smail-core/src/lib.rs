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
pub mod session;
pub mod threads;
pub mod wkd;

pub use error::{CoreError, Result, StartError};

use pgp::PgpCfg;

use oauth::OAuthCfg;

use config::SyncConfig;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smail_backend::{connect, BackendError, FlagChange, MailBackend, OutgoingMail, ServerEvent};
pub use smail_backend::{test_connection, FolderKind, ImapConfig, SmtpConfig};
use smail_store::{DraftRow, MessageRow, Store};

// Re-exports from new modules
pub use config::{load_accounts, save_accounts, slug_for, AccountConfig, Rule};
pub use protocol::{Cmd, Event, FolderInfo};
pub use reader::{extract_body, ReaderCtx};
pub use session::{Session, SessionCmd, TaggedEvent};

// 内部使用的辅助函数
use reader::{
    backoff, extract_attachment_bytes, extract_recipients, group_by_folder, parse_from_addr,
    split_addrs, to_row,
};

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

/// 列表单次返回上限（无分页；UI 在满额时提示）
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
        // 离线优先：连接不在启动路径上阻塞——直接以 Down 态进命令循环，
        // 首连由重连机制接管（连接 → 回放标记 → 同步当前文件夹 → LIST → 发件箱）。
        // 命令循环立即可服务：SelectFolder 的本地先行即时出列表，
        // 打开应用秒见缓存邮件，不必等「连接+同步 INBOX+草稿对账」数秒串行完成
        let mut conn = Conn::Down {
            attempts: 0,
            // 首连前给 UI 恢复命令一个到达窗口：冷启动的 SelectFolder（本地先行）
            // 必须赶在连接（数秒）之前落地——否则 recv_timeout(0) 立即去连接，
            // 恢复命令又被堵在连接后面，离线优先失效（用户实测回归点）
            retry_at: Instant::now() + Duration::from_millis(200),
        };
        // slot 与 conn 状态同步（Reader 取拉用）：Up 时暴露 backend，Down 时清空
        *self.backend_slot.write().unwrap() = None;

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
                        *self.backend_slot.write().unwrap() = None;
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
                        self.sync_folder(&**backend, &store, &f);
                        // Drafts 变更 → 草稿对账
                        if let Some(df) = backend.drafts_folder_name() {
                            if f == df {
                                self.sync_drafts(&**backend, &store);
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
                        *self.backend_slot.write().unwrap() = None;
                        conn = Conn::Down {
                            attempts: 0,
                            retry_at: Instant::now(),
                        };
                        continue;
                    }
                    if tick.is_multiple_of(30) {
                        self.flush_outbox(&**backend, &store);
                    }
                    if tick.is_multiple_of(15) {
                        deferred = self.index_pass(&**backend, &store, &cmd_rx);
                    }
                }
                Conn::Down { attempts, retry_at } => {
                    if Instant::now() < *retry_at {
                        continue;
                    }
                    // 首连（含冷启动）与重连文案分开：用户区分“第一次连”与“掉了重连”
                    if *attempts == 0 {
                        self.emit(Event::Status(
                            self.tr("连接服务器…", "Connecting to server…"),
                        ));
                    } else {
                        self.emit(Event::Status(self.trf(
                            "重连中（第 {} 次）…",
                            "Reconnecting (attempt {})…",
                            &[&(*attempts + 1)],
                        )));
                    }
                    // 首连用快速模式（不刷新 token，避免阻塞启动）；重连时强制刷新
                    match self.connect_backend(*attempts > 0) {
                        Ok(backend) => {
                            let watch_rx = match backend.watch() {
                                Ok(rx) => rx,
                                Err(e) => {
                                    self.emit(Event::Status(self.trf(
                                        "恢复 IDLE 失败：{}",
                                        "Failed to resume IDLE: {}",
                                        &[&e],
                                    )));
                                    *attempts += 1;
                                    *retry_at = Instant::now()
                                        + backoff(*attempts, self.cfg.sync_config.max_backoff_secs);
                                    continue;
                                }
                            };
                            let first = *attempts == 0;
                            self.emit(Event::Status(if first {
                                self.tr("已连接", "Connected")
                            } else {
                                self.tr("已重连", "Reconnected")
                            }));
                            // 先回放离线标记再同步：本地意图先落地，同步 diff 才不会反向覆盖
                            self.emit(Event::ConnState(true));
                            self.push_pending_flags(&*backend, &store);
                            // 断网期间其他设备可能改过草稿：重连即对账
                            self.sync_drafts(&*backend, &store);
                            // 无选中文件夹时默认同步 INBOX（原启动语义兑底：
                            // UI 未能复视图/无 last_view 时也能拿到新邮件；
                            // headless e2e 用例大多依赖此行为）
                            let target = current_folder
                                .clone()
                                .unwrap_or_else(|| "INBOX".to_string());
                            // 重连时清空 pending_sync：避免断线前的旧文件夹残留导致
                            // 重复同步（重连已同步当前文件夹，延迟同步队列应清空）
                            pending_sync = None;
                            self.sync_folder(&*backend, &store, &target);
                            self.emit_folders(&*backend, &store);
                            self.flush_outbox(&*backend, &store);
                            *self.backend_slot.write().unwrap() = Some(backend.clone());
                            // P2 优化：backend 就绪后通知 Reader 线程（Condvar 唤醒等待）
                            {
                                let (lock, cvar) = &*self.backend_ready;
                                *lock.lock().unwrap() = true;
                                cvar.notify_all();
                            }
                            conn = Conn::Up { backend, watch_rx };
                        }
                        Err(e) => {
                            // 连接失败可见：不打印的话 macOS 下 melib Happy-Eyeballs 竞态
                            //（OS error 56 EISCONN）只能看到无限重连，无从诊断
                            let first = *attempts == 0;
                            self.emit(Event::Status(self.trf(
                                if first {
                                    "连接失败：{}"
                                } else {
                                    "重连失败：{}"
                                },
                                if first {
                                    "Connection failed: {}"
                                } else {
                                    "Reconnect failed: {}"
                                },
                                &[&e],
                            )));
                            *attempts += 1;
                            *retry_at = Instant::now()
                                + backoff(*attempts, self.cfg.sync_config.max_backoff_secs);
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
                    self.sync_folder(&**backend, &store, &folder);
                    self.emit_folder_counts(&store);
                }
            }
        }
    }

    /// oauth 账号：解析/刷新 token 后建后端（重连与初连共用，token 每次重建都最新）
    ///
    /// `force_refresh`: 是否强制检查刷新 token（重连时 true，首连时 false 避免阻塞启动）
    fn connect_backend(
        &self,
        force_refresh: bool,
    ) -> std::result::Result<Arc<dyn MailBackend>, String> {
        let mut imap = self.cfg.imap.clone();
        if let Some(o) = &self.cfg.oauth {
            // 首连时用 try_token_no_refresh 快速取 token（可能过期），连接失败后重连时再刷新
            let token_result = if force_refresh {
                oauth::ensure_token(o, &self.data_dir)
            } else {
                oauth::try_token_no_refresh(&self.data_dir)
            };

            if let Err(e) = token_result {
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
            let tok = if force_refresh {
                oauth::ensure_token(o, &self.data_dir)?
            } else {
                oauth::try_token_no_refresh(&self.data_dir)?
            };
            imap.password = Some(oauth::sasl_b64(&imap.username, &tok));
            imap.use_oauth2 = true;
        }
        connect(&imap)
            .map(|(b, _)| Arc::new(b) as Arc<dyn MailBackend>)
            .map_err(|e| e.to_string())
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
            Cmd::SetLang(zh) => self.zh.store(zh, std::sync::atomic::Ordering::Relaxed),
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
                    Some(t) => t.elapsed() > self.cfg.sync_config.folder_resync_ttl(),
                    None => true,
                };
                if fresh {
                    // TTL 过期：立即同步，不延迟（否则用户看到过期缓存直到命令队列
                    // drain 完才刷新）；连接中则直接同步，断线时设置 pending_sync
                    if let Conn::Up { backend, .. } = &conn {
                        self.sync_folder(&**backend, store, &folder);
                    } else {
                        *pending_sync = Some(folder);
                    }
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
                // seen 上推当场做（快命令，不涉大传输）：本地置灰立即，服务器 STORE 失败入回放
                if let Some(row) = store.get(hash).ok().flatten() {
                    if !row.seen {
                        store.set_seen(hash, true).ok();
                        self.emit(Event::FlagsChanged {
                            hash,
                            seen: Some(true),
                            flagged: None,
                        });
                        match &mut *conn {
                            Conn::Up { backend, .. } => {
                                if (**backend)
                                    .set_flags(&row.folder, &[hash as u64], FlagChange::Seen(true))
                                    .is_err()
                                {
                                    store.queue_flag(hash, Some(true), None).ok();
                                }
                            }
                            Conn::Down { .. } => {
                                store.queue_flag(hash, Some(true), None).ok();
                            }
                        }
                    }
                }
                // 正文管线（网络 FETCH + PGP + 解析 + PDF 缩略图）分流到 Reader 线程：
                // 大附件取拉期间命令循环照常处理同步/写命令，不再整循环停摆
                let _ = self.read_tx.send(hash);
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
                                &**backend,
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
                    self.flush_outbox(&**backend, store);
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
                    self.sync_drafts(&**backend, store);
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
                        match (**backend).delete_draft_by_mid(&mid) {
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
                        if (**backend)
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
                        if (**backend)
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
                        let _ = (**backend).delete_messages(&row.folder, &[hash as u64]);
                    }
                    store.delete_message(hash).ok();
                    let _ = std::fs::remove_file(self.body_path(hash));
                    self.emit(Event::MessageDeleted(hash));
                } else {
                    let trash = match &mut *conn {
                        Conn::Up { backend, .. } => (**backend).ensure_trash_folder(),
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
                                self.emit_folders(&**backend, store);
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
                                let _ = (**backend).delete_messages(&row.folder, &[hash as u64]);
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
                            self.emit_folders(&**backend, store);
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
                            self.emit_folders(&**backend, store);
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
                            self.emit_folders(&**backend, store);
                        }
                        Err(e) => {
                            self.emit(Event::Status(self.trf(
                                "改名文件夹失败：{}",
                                "Failed to rename folder: {}",
                                &[&e],
                            )));
                            // UI 侧是乐观改名：失败时重发真实文件夹列表拉回真相，
                            // 否则侧栏停在不存在的新名字上点击即报错
                            self.emit_folders(&**backend, store);
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
                match (**backend).ensure_archive_folder() {
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
                match (**backend).ensure_junk_folder() {
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
        let backend = &**backend;
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
    fn push_pending_flags(&self, backend: &dyn MailBackend, store: &Store) {
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
    fn sync_drafts(&self, backend: &dyn MailBackend, store: &Store) {
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

    fn sync_folder(&self, backend: &dyn MailBackend, store: &Store, folder: &str) {
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
        let total = envelopes.len();
        // 进度起点：0/total
        self.emit(Event::SyncProgress {
            folder: folder.to_string(),
            total,
            processed: 0,
        });
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
        // 进度更新：批量处理完成（新邮件 + 标记更新视为已处理部分）
        let processed = new_count + flag_updates.len();
        self.emit(Event::SyncProgress {
            folder: folder.to_string(),
            total,
            processed: processed.min(total),
        });
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
        // 进度完成：total/total
        self.emit(Event::SyncProgress {
            folder: folder.to_string(),
            total,
            processed: total,
        });
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
        let backend = &**backend;
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
        let backend = &**backend;
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
        let trash_dest = (**backend).ensure_trash_folder().ok();
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
                        self.emit_folders(&**backend, store);
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
            Conn::Up { backend, .. } => &**backend,
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
        backend: &dyn MailBackend,
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

    fn emit_folders(&self, backend: &dyn MailBackend, store: &Store) {
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

    fn body_path(&self, hash: i64) -> PathBuf {
        self.data_dir.join("bodies").join(format!("{hash}.eml"))
    }

    // ---------------- 发信 ----------------

    /// 组装发信凭据（oauth token 注入）+ 建可复用 sender。
    /// oauth/token 失败返 Err：单发路径报错留 pending，flush 路径整轮暂缓重试
    fn smtp_sender(&self) -> std::result::Result<smail_backend::SmtpSender, String> {
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

    fn flush_outbox(&self, backend: &dyn MailBackend, store: &Store) {
        if let Ok(n) = store.fail_exhausted_outbox() {
            if n > 0 {
                self.emit(Event::Status(self.trf("{} 封发件重试穷尽，已标 failed（请检查收件人/附件后重发）", "{} outgoing message(s) exhausted retries, marked failed (check recipients/attachments and resend)", &[&n])));
            }
        }
        // sent 行清理（保留最近 100 条；防磁盘慢泄漏）
        store.outbox_clean_sent();
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
        backend: &dyn MailBackend,
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
            let fail_permanent = |msg: String| BackendError::Other(msg);
            let built = (|| -> std::result::Result<Vec<u8>, String> {
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
                store.outbox_mark(outbox_id, status, &e.to_string()).ok();
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
        backend: &dyn MailBackend,
        store: &Store,
        cmd_rx: &Receiver<Cmd>,
    ) -> Option<Cmd> {
        // 有排队命令时不启动索引（旧版总是先 fetch 一封再 yield——
        // 命令高峰期每次 tick 都白等 1-2s 网络）
        if let Ok(cmd) = cmd_rx.try_recv() {
            return Some(cmd);
        }
        let todo = store
            .unindexed(self.cfg.sync_config.index_batch)
            .unwrap_or_default();
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
