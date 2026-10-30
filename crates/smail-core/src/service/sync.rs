//! 同步：文件夹全量 diff 同步 / 草稿云同步对账 / 离线标记回放 / 后台索引

use crate::protocol::{Cmd, Event};
use crate::reader::{extract_body, extract_recipients, parse_from_addr, to_row};
use crate::{pgp, ServiceInner};
use smail_backend::{FlagChange, MailBackend};
use smail_store::{DraftRow, MessageRow, Store};
use std::sync::mpsc::Receiver;
use std::time::Instant;

impl ServiceInner {
    /// 离线标记回放（v4 队列）：重连/初连后先把本地改动推回服务器，成功一项清一项；
    /// 失败保留队列下轮重试（意图语义：后到覆盖，最终收敛）。
    /// 必须在 sync_folder 之前调：本地意图先落地，同步 diff 才不会把本地改动反向覆盖
    pub(crate) fn push_pending_flags(&self, backend: &dyn MailBackend, store: &Store) {
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
    pub(crate) fn sync_drafts(&self, backend: &dyn MailBackend, store: &Store) {
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

    /// 同步失败分类：None=成功；Some(Timeout)=连接半死（须重连）；
    /// Some(其他)=单次失败（Status 已报，不重试）。
    /// `allow_probe`：允许 SELECT 探测短路（EXISTS/UIDNEXT 与上次全量一致
    /// 且在低频校准窗口内 → 跳过整包 FETCH）。**IDLE 变更与重连路径必须传
    /// false**——远端 flags-only 变化（别的设备标已读）不改这两个值，短路
    /// 会吞掉推送；用户切文件夹的 TTL 重拉传 true 省流量
    pub(crate) fn sync_folder(
        &self,
        backend: &dyn MailBackend,
        store: &Store,
        folder: &str,
        allow_probe: bool,
    ) -> Option<smail_backend::BackendError> {
        // 探测短路：记录齐全、探测一致、且距上次全量在低频校准窗口内 →
        // 本地行即服务器真相，省一次整包 FETCH
        let ttl_secs = self.cfg.sync_config.folder_resync_ttl_secs;
        if allow_probe {
            let probe_hit = (|| -> Option<bool> {
                let p = store.get_folder_probe(folder).ok()??;
                // 低频校准：超过 10 倍 TTL 强制一次真实全量，兜住远端
                // flags-only 变化（EXISTS/UIDNEXT 不变但 FLAGS 被改）
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if now - p.synced_at >= 10 * ttl_secs as i64 {
                    return Some(false);
                }
                let (exists, uidnext, uidvalidity) = backend.probe_folder(folder).ok()?;
                // 三值全等才短路：UIDVALIDITY 防 UID 空间重置后的数值巧合
                Some(
                    exists as i64 == p.exists_n
                        && uidnext as i64 == p.uidnext
                        && uidvalidity as i64 == p.uidvalidity,
                )
            })();

            if probe_hit == Some(true) {
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
                // 文件夹名本地化（与全量同步的状态行一致）
                let fname = if folder.eq_ignore_ascii_case("INBOX") {
                    self.tr("收件箱", "INBOX")
                } else {
                    folder.to_string()
                };
                self.emit(Event::Status(self.trf(
                    "{} 无新变化（跳过整包拉取）",
                    "{} unchanged (full fetch skipped)",
                    &[&fname],
                )));
                return None;
            }
        }
        self.emit(Event::Status(self.trf(
            "正在同步 {}…",
            "Syncing {}…",
            &[&folder],
        )));
        let probe = backend.probe_folder(folder).ok();
        let envelopes = match backend.fetch_envelopes(folder) {
            Ok(e) => e,
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "同步 {} 失败：{}",
                    "Failed to sync {}: {}",
                    &[&folder, &e],
                )));
                return Some(e);
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
        // 全量完成：记录探测快照（下次 TTL 重拉的短路依据）。
        // probe 取的是 fetch 前的 SELECT——fetch 期间新到的邮件会让快照
        // 偏旧，下次探测发现 exists/uidnext 差异自动走全量，自洽
        if let Some((exists, uidnext, uidvalidity)) = probe {
            store
                .set_folder_probe(
                    folder,
                    &smail_store::FolderProbe {
                        uidnext: uidnext as i64,
                        exists_n: exists as i64,
                        uidvalidity: uidvalidity as i64,
                        synced_at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    },
                )
                .ok();
        }
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
        // 状态行文件夹名本地化（顶栏不再出现「INBOX：46 封」中英混排）
        let fname = if folder.eq_ignore_ascii_case("INBOX") {
            self.tr("收件箱", "INBOX")
        } else {
            folder.to_string()
        };
        self.emit(Event::Status(self.trf(
            "{}：{} 封（新 {}）",
            "{}: {} messages ({} new)",
            &[&fname, &envelopes.len(), &new_count],
        )));
        None
    }

    // ---------------- M7：移动 / 批量操作 ----------------

    /// 后台索引器：分批拉正文建 FTS。批内逐条让路：用户命令到达即中断，
    /// 把命令交还主循环优先处理（索引是后台活，不该挡交互）
    pub(crate) fn index_pass(
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
