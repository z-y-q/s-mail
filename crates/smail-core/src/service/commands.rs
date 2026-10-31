//! 命令处理：UI 命令的分发与执行（标记/移动/删除/文件夹管理/附件）

use crate::protocol::{Cmd, Event};
use crate::reader::group_by_folder;
use crate::{pgp, Conn, ServiceInner, UI_LIST_LIMIT};
use smail_backend::FlagChange;
use smail_store::{DraftRow, Store};
use std::time::Duration;

impl ServiceInner {
    pub(crate) fn handle_cmd(
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
                    // drain 完才刷新）；连接中则直接同步，断线时设置 pending_sync。
                    // 失败也回 pending_sync：主循环延迟通道会重试一次
                    //（超时类由主循环转入重连路径）
                    if let Conn::Up { backend, .. } = &conn {
                        // 用户切文件夹：允许探测短路（无变化则省整包拉取）
                        if self.sync_folder(&**backend, store, &folder, true).is_some() {
                            // 失败且是标准文件夹：可能是服务器映射缺失（侧栏
                            // 合成行点击）——ensure 建文件夹后用真实路径重试
                            let kind = smail_backend::folder_kind(&folder);
                            let ensured = match kind {
                                smail_backend::FolderKind::Trash => {
                                    (**backend).ensure_trash_folder().ok()
                                }
                                smail_backend::FolderKind::Junk => {
                                    (**backend).ensure_junk_folder().ok()
                                }
                                smail_backend::FolderKind::Archive => {
                                    (**backend).ensure_archive_folder().ok()
                                }
                                smail_backend::FolderKind::Sent => {
                                    (**backend).ensure_sent_folder().ok()
                                }
                                _ => None,
                            };
                            if let Some(real) = ensured {
                                self.last_sync.borrow_mut().remove(&real);
                                if self.sync_folder(&**backend, store, &real, false).is_none() {
                                    // 真实路径与合成名不同（如 QQ 的废纸篓是
                                    // Deleted Messages）：把导航修正到真实路径
                                    if real != folder {
                                        *current_folder = Some(real.clone());
                                        self.emit(Event::Status(self.trf(
                                            "已定位到 {}",
                                            "Located at {}",
                                            &[&real],
                                        )));
                                    }
                                    *pending_sync = None;
                                }
                            }
                        }
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
                // 清 TTL 再走常规同步（sync_folder 会在成功后重新计时）；
                // 下一次延迟同步禁探测短路（手动刷新=真实全量，含 flags 校准）
                self.last_sync.borrow_mut().remove(&folder);
                self.force_probe_bypass.set(true);
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
                // 正文管线（网络 FETCH + PGP + 解析 + PDF 缩略图）分流到 Reader 线程。
                // UI 另有直通通道（绕过命令队列秒出缓存正文），这里保留转发作为
                // headless/e2e 的兜底；同一封解析两遍的重复回包由 UI 的 body_req
                // 守卫丢弃
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
                // 清空废纸篓 = 整箱 EXPUNGE（SELECT + EXPUNGE）：
                // 语义就是清空整个文件夹，不需要逐封 hash 定位——
                // 逐封删除在部分服务器（QQ）上静默空转（UID EXPUNGE 对
                // 不存在的 UID 也返回 OK），整箱原语最鲁棒
                match &mut *conn {
                    Conn::Up { backend, .. } => {
                        if let Err(e) = (**backend).expunge_folder(&folder) {
                            self.emit(Event::Status(self.trf(
                                "清空废纸篓失败（服务器拒绝）：{}",
                                "Failed to empty trash (server refused): {}",
                                &[&e],
                            )));
                            return false;
                        }
                    }
                    Conn::Down { .. } => {
                        self.emit(Event::Status(
                            self.tr("未连接，稍后重试", "Not connected, will retry later"),
                        ));
                        return false;
                    }
                }
                // 服务器已清：本地行随之清掉，重同步确认（服务器真相拉回）
                let hashes = store.hashes_in_folder(&folder).unwrap_or_default();
                let n = hashes.len();
                if !hashes.is_empty() {
                    store.delete_messages_batch(&hashes).ok();
                    for h in &hashes {
                        let _ = std::fs::remove_file(self.body_path(*h));
                        self.emit(Event::MessageDeleted(*h));
                    }
                }
                self.emit(Event::Status(self.trf(
                    "已清空废纸篓（{} 封）",
                    "Emptied Trash ({} message(s))",
                    &[&n],
                )));
                self.emit_folder_counts(store);
                // 验证重同步：清 TTL 立即拉回服务器真相（EXPUNGE 后应为 0 封）
                self.last_sync.borrow_mut().remove(&folder);
                *pending_sync = Some(folder);
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
                    // 服务器失败必须可见且不删本地：旧路径 `let _ =` 吞错后
                    // 无条件删本地行，服务器上邮件还在 → 下次同步"复活"。
                    // melib 还有一种静默：UID 映射缺失时 EXPUNGE 空转返回 Ok
                    // ——删完清该文件夹同步 TTL 立即重同步，真删了无感，
                    // 没删掉当场回来（服务器真相），用户不再被假象误导
                    match &mut *conn {
                        Conn::Up { backend, .. } => {
                            if let Err(e) = (**backend).delete_messages(&row.folder, &[hash as u64])
                            {
                                self.emit(Event::Status(self.trf(
                                    "删除失败（邮件仍在服务器）：{}",
                                    "Delete failed (message still on server): {}",
                                    &[&e],
                                )));
                                return false;
                            }
                        }
                        Conn::Down { .. } => {
                            self.emit(Event::Status(
                                self.tr("未连接，稍后重试", "Not connected, will retry later"),
                            ));
                            return false;
                        }
                    }
                    store.delete_message(hash).ok();
                    let _ = std::fs::remove_file(self.body_path(hash));
                    self.emit(Event::MessageDeleted(hash));
                    self.last_sync.borrow_mut().remove(&row.folder);
                    *pending_sync = Some(row.folder);
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
            Cmd::BatchDelete { hashes } => {
                // 批量删除可能涉及多个文件夹：删完统一清各自文件夹的同步 TTL，
                // 命令队列 drain 后由各 pending 同步验证（同 DeleteMessage 语义）
                let mut folders: Vec<String> = hashes
                    .iter()
                    .filter_map(|h| store.get(*h).ok().flatten().map(|r| r.folder))
                    .collect();
                folders.sort();
                folders.dedup();
                self.batch_delete(conn, store, &hashes);
                for f in &folders {
                    self.last_sync.borrow_mut().remove(f);
                    *pending_sync = Some(f.clone());
                }
            }
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
            Cmd::RenameFolder {
                old_path: old,
                new_path: new,
            } => {
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
                // 10s 超时；非 2xx 默认报错正好走 continue（best-effort）
                let agent = ureq::Agent::config_builder()
                    .timeout_per_call(Some(Duration::from_secs(10)))
                    .build()
                    .new_agent();
                for url in &urls {
                    if total >= MAX_TOTAL {
                        break;
                    }
                    match agent.get(url).call() {
                        Ok(resp) if resp.status().as_u16() == 200 => {
                            let mut buf = Vec::new();
                            use std::io::Read as _;
                            if resp
                                .into_body()
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
            Cmd::AddContact { email, name } => {
                // 与收信自动收集同一 upsert 通道（同名非空才覆盖旧名）
                store
                    .upsert_contacts_batch(&[(email.clone(), name.clone())])
                    .ok();
                let rows = store.list_contacts_full().unwrap_or_default();
                self.emit(Event::ContactsFull(rows));
                self.emit(Event::Status(self.trf(
                    "已将 {email} 加入通讯录",
                    "Added {email} to contacts",
                    &[&email],
                )));
            }
            Cmd::ListContacts => {
                let addrs = store.list_contacts_display(30).unwrap_or_default();
                self.emit(Event::ContactsListed(addrs));
            }
            Cmd::ApplyRules(ref folder) => {
                let Conn::Up { backend, .. } = &mut *conn else {
                    self.emit(Event::Status(
                        self.tr("未连接，稍后重试", "Not connected, will retry later"),
                    ));
                    return false;
                };
                let backend = &**backend;
                self.apply_rules_folder(backend, store, folder);
            }
            Cmd::Redirect { hash, to } => {
                // 正文已缓存：立刻重投。未缓存：转 Reader 现取（服务循环内联
                // fetch_body 会与 IDLE 连接争锁死锁——m22 实测），取件完成经
                // BodyLoaded 事件由 UI 以 RedirectNow 续发
                if self.body_path(hash).exists() {
                    self.redirect_cmd(store, hash, to);
                } else {
                    let _ = self.read_tx.send(hash);
                    self.emit(Event::Status(self.tr(
                        "正在拉取原信，稍后代转…",
                        "Fetching original message, redirect will follow…",
                    )));
                }
            }
            Cmd::RedirectNow { hash, to } => {
                self.redirect_cmd(store, hash, to);
            }
        }
        false
    }

    // ---------------- 同步 ----------------

    /// 移动（单封/批量共用）：按 folder 分组服务器搬运，成功后清本地记录与正文缓存。
    /// UI 侧发命令时已乐观移除列表行；失败仅报错（下次同步自然回真）。
    pub(crate) fn move_messages_cmd(
        &self,
        conn: &mut Conn,
        store: &Store,
        hashes: &[i64],
        dest: &str,
    ) {
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

    pub(crate) fn batch_flags(
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

    pub(crate) fn batch_delete(&self, conn: &mut Conn, store: &Store, hashes: &[i64]) {
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
}
