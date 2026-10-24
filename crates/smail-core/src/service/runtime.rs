//! 服务主循环：命令处理调度 + 连接管理 + 同步调度

use crate::protocol::{Cmd, Event};
use crate::reader::backoff;
use crate::{oauth, Conn, ServiceInner};
use smail_backend::{connect, MailBackend, ServerEvent};
use smail_store::Store;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

impl ServiceInner {
    pub(crate) fn run(self, cmd_rx: Receiver<Cmd>) {
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
        *self.backend_slot.write().unwrap_or_else(|p| p.into_inner()) = None;

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
                        *self.backend_slot.write().unwrap_or_else(|p| p.into_inner()) = None;
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
                        *self.backend_slot.write().unwrap_or_else(|p| p.into_inner()) = None;
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
                            *self.backend_slot.write().unwrap_or_else(|p| p.into_inner()) =
                                Some(backend.clone());
                            // P2 优化：backend 就绪后通知 Reader 线程（Condvar 唤醒等待）
                            {
                                let (lock, cvar) = &*self.backend_ready;
                                *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
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
    pub(crate) fn connect_backend(
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
}
