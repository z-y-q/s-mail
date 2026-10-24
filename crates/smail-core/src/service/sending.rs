//! 发信：SMTP 发送 / Sent 归档 / 发件箱冲刷 / 草稿上传

use crate::protocol::Event;
use crate::reader::split_addrs;
use crate::{oauth, pgp, wkd, Conn, ServiceInner};
use smail_backend::{BackendError, MailBackend, OutgoingMail};
use smail_store::{DraftRow, Store};
use std::path::Path;

impl ServiceInner {
    /// B2 草稿云同步（v1 单向）：本地保存成功后上传服务器 Drafts。
    /// 编辑已上传草稿：先传新副本（新 Message-ID）再删旧副本——append 先行，
    /// 失败最多残留旧版不丢内容。离线/失败：仅本地，下次在线保存时上传
    pub(crate) fn upload_draft(
        &self,
        conn: &mut Conn,
        store: &Store,
        draft_id: i64,
        row: &DraftRow,
    ) {
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

    /// 组装发信凭据（oauth token 注入）+ 建可复用 sender。
    /// oauth/token 失败返 Err：单发路径报错留 pending，flush 路径整轮暂缓重试
    pub(crate) fn smtp_sender(&self) -> std::result::Result<smail_backend::SmtpSender, String> {
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

    pub(crate) fn flush_outbox(&self, backend: &dyn MailBackend, store: &Store) {
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
    pub(crate) fn try_send(
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
                    self.cfg.imap.username
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
}
