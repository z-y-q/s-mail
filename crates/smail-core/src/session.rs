//! 多账号会话管理
//!
//! ## 职责
//!
//! - 启动多个账号服务线程（每账号一个 `AccountService`）
//! - 统一事件流：各账号事件打标签后回流到 UI
//! - 命令路由：根据账号索引分发 UI 命令
//! - 广播操作：全账号搜索、统一收件箱、语言切换
//!
//! ## 架构
//!
//! ```text
//! UI ──SessionCmd──> Session 多路复用器
//!                      ├──Cmd──> 账号0服务线程
//!                      ├──Cmd──> 账号1服务线程
//!                      └──Cmd──> 账号2服务线程
//!
//!    <──TaggedEvent── (account: 0, event: Folders(...))
//!    <──TaggedEvent── (account: 1, event: Status(...))
//! ```
//!
//! ## 关键类型
//!
//! - `Session`：多账号会话句柄（持有 cmd_tx 和 mux 线程）
//! - `SessionCmd`：多账号命令（Route、SearchAll、UnifiedInbox、SetLang）
//! - `TaggedEvent`：带账号标签的事件（UI 根据 account 字段分组显示）

use crate::config::{load_accounts, StartError};
use crate::protocol::{Cmd, Event};
use crate::AccountService;
use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

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
    /// 每账号正文请求直通通道（下标=账号索引）：UI 打开邮件直接发给
    /// Reader 线程，不经主循环——主循环堵在长同步里时正文照常秒出
    pub read_txs: Vec<std::sync::mpsc::Sender<i64>>,
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
        std::fs::create_dir_all(data_root)?;

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
        // 直通通道在 services 被 move 进 mux 线程前收集
        let read_txs: Vec<std::sync::mpsc::Sender<i64>> =
            services.iter().map(|(svc, _)| svc.read_tx.clone()).collect();

        // 终报必须在 mux 线程内、循环退出后发——此前误放主线程（每次启动
        // 立即误报"会话已结束"，m5 e2e 苔出）；loop 内用 break 退出。
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
            // 而不是事件流悄悄断掉（用户表现为"再无任何同步"）
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
                read_txs,
                mux: Some(mux),
            },
            event_rx,
        ))
    }
}

fn tr_str(zh: bool, zh_text: &str, en_text: &str) -> String {
    if zh {
        zh_text.to_string()
    } else {
        en_text.to_string()
    }
}
