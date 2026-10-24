//! 文件夹：列表发射（本地先行 + 服务器）/ 未读计数 / 规则引擎应用

use crate::protocol::{Event, FolderInfo};
use crate::ServiceInner;
use smail_backend::{FlagChange, MailBackend};
use smail_store::{MessageRow, Store};
use std::path::PathBuf;

impl ServiceInner {
    /// M5-b：对新邮件匹配规则（首条命中即用），动作按规则分桶后批量执行，
    /// 动作失败发错误事件不中断同步。
    ///（旧版逐封逐 flag 服务器往返：N 封命中 = 2N+ 次 IMAP 往返，批量后每桶空flag/1 次）
    pub(crate) fn apply_rules(
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
        // 逐行扫规则 → 按规则索引分桶执行（批量 IMAP 往返）。
        // continue_after=false（旧默认）：首条命中即停，每封只进一个桶；
        // =true：同一封可叠加后续命中规则的动作（链式）
        let mut buckets: Vec<(usize, Vec<MessageRow>)> = Vec::new();
        for row in new_rows {
            for (ri, r) in self.cfg.rules.iter().enumerate() {
                let folder_ok = r
                    .folder
                    .as_ref()
                    .map(|f| f.eq_ignore_ascii_case(folder))
                    .unwrap_or(true);
                if !folder_ok || !r.matches(&row) {
                    continue;
                }
                match buckets.iter_mut().find(|(b, _)| *b == ri) {
                    Some((_, rows)) => rows.push(row.clone()),
                    None => buckets.push((ri, vec![row.clone()])),
                }
                if !r.continue_after {
                    break;
                }
            }
        }
        let mut seen_n = 0usize;
        let mut flag_n = 0usize;
        let mut moved_n = 0usize;
        for (ri, rows) in buckets {
            let rule = &self.cfg.rules[ri];
            let hashes: Vec<i64> = rows.iter().map(|r| r.envelope_hash).collect();
            let batch: Vec<u64> = hashes.iter().map(|h| *h as u64).collect();
            for action in &rule.actions {
                match action {
                    crate::RuleAction::MarkSeen => {
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
                    crate::RuleAction::Flag => {
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
                    crate::RuleAction::Move(dest) => {
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
                                    &[&folder, dest, &e],
                                ))),
                            }
                        }
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
    pub(crate) fn emit_folders_local(&self, store: &Store) {
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
    pub(crate) fn emit_folder_counts(&self, store: &Store) {
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

    pub(crate) fn emit_folders(&self, backend: &dyn MailBackend, store: &Store) {
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

    pub(crate) fn body_path(&self, hash: i64) -> PathBuf {
        self.data_dir.join("bodies").join(format!("{hash}.eml"))
    }

    // ---------------- 发信 ----------------
}
