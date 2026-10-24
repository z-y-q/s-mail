//! 邮件正文读取线程
//!
//! ## 职责
//!
//! - 独立线程处理正文读取（避免阻塞服务主循环）
//! - 缓存优先：命中本地 .eml 文件立即返回
//! - 网络取拉：IMAP FETCH → 解析 → PGP 解密 → 存储
//! - 邮件解析：MIME 多段、HTML 转 Markdown、附件提取
//! - PDF 缩略图生成（首页预览）
//!
//! ## 架构
//!
//! ```text
//! 服务主循环 ──OpenMessage(hash)──> Reader 线程
//!                                     ↓
//!                              检查缓存 bodies/{hash}.eml
//!                                     ↓ 未命中
//!                              IMAP FETCH 原始邮件
//!                                     ↓
//!                              PGP 解密（如需要）
//!                                     ↓
//!                              MIME 解析（正文 + 附件）
//!                                     ↓
//!                              存储到缓存 + 发送 BodyLoaded 事件
//! ```
//!
//! ## 并发模型
//!
//! - Reader 线程与服务主循环并发运行
//! - 共享 SQLite（WAL 模式 + busy_timeout 处理锁冲突）
//! - backend_slot：通过 RwLock 获取当前在线 backend 实例
//!
//! ## 关键函数
//!
//! - `ReaderCtx::run()`：主循环，接收 hash 队列
//! - `extract_body()`：MIME 解析主入口（公开 API）
//! - `extract_recipients()`：提取收件人（To/Cc）
//! - `group_by_folder()`：按文件夹分组（批量操作用）

use crate::protocol::Event;
use crate::{html, pgp};
use smail_backend::{EnvelopeSummary, MailBackend};
use smail_store::{MessageRow, Store};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 正文取拉线程上下文。每账号一个，start_for 时 spawn：
/// 承接 OpenMessage 的完整管线（缓存命中 / 网络 FETCH / PGP / 解析 / PDF 缩略图 /
/// BodyLoaded 事件），使大附件取拉不再阻塞服务主循环（同步/写命令/IDLE 照常）。
/// backend 经 backend_slot 每次请求时取当前在线实例（断线期间取拉直接报错）
pub struct ReaderCtx {
    pub(crate) backend_slot: Arc<std::sync::RwLock<Option<Arc<dyn MailBackend>>>>,
    pub(crate) events: tokio::sync::mpsc::UnboundedSender<Event>,
    pub(crate) data_dir: PathBuf,
    /// PGP 运行时（start_for 预载；加载失败 None：加密件显示未配置提示）
    pub(crate) pgp: Option<Arc<pgp::PgpRuntime>>,
    pub(crate) zh: Arc<std::sync::atomic::AtomicBool>,
    /// 冷启动等待超时（配置值）
    pub(crate) cold_start_timeout: Duration,
    /// backend 就绪通知：连接成功时主线程 notify，Reader 等待而非轮询
    pub(crate) backend_ready: Arc<(Mutex<bool>, Condvar)>,
}

impl ReaderCtx {
    pub fn new(
        backend_slot: Arc<std::sync::RwLock<Option<Arc<dyn MailBackend>>>>,
        events: tokio::sync::mpsc::UnboundedSender<Event>,
        data_dir: PathBuf,
        pgp: Option<Arc<pgp::PgpRuntime>>,
        zh: Arc<std::sync::atomic::AtomicBool>,
        cold_start_timeout: Duration,
        backend_ready: Arc<(Mutex<bool>, Condvar)>,
    ) -> Self {
        Self {
            backend_slot,
            events,
            data_dir,
            pgp,
            zh,
            cold_start_timeout,
            backend_ready,
        }
    }
}

impl ReaderCtx {
    pub fn run(self, rx: Receiver<i64>) {
        // 独立连接：Reader 与主循环并发读写同一 SQLite（WAL + busy_timeout）。
        // 开库重试：主循环可能正在建表/写事务，短暂撞锁不应让 Reader 永久罢工
        let mut store = None;
        for _ in 0..5 {
            match Store::open(&self.data_dir.join("index.db")) {
                Ok(s) => {
                    store = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
        let Some(store) = store else {
            self.emit(Event::Error(
                self.tr("正文线程打开索引失败", "Reader thread failed to open index"),
            ));
            return;
        };
        while let Ok(hash) = rx.recv() {
            // 最新优先：用户快速切换邮件时，队列积压的旧请求全部跳过，只服务最新的
            let hash = rx.try_iter().last().unwrap_or(hash);
            self.serve(&store, hash);
        }
    }

    fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
    }
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

    /// 单封正文服务。seen 上推已在主循环当场处理，此处只做正文管线
    fn serve(&self, store: &Store, hash: i64) {
        let Some(row) = store.get(hash).ok().flatten() else {
            self.emit(Event::Status(self.trf(
                "找不到邮件 {}",
                "Message {} not found",
                &[&hash],
            )));
            return;
        };
        let path = self.data_dir.join("bodies").join(format!("{hash}.eml"));
        let bytes = if path.exists() {
            match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    self.emit(Event::Status(self.trf(
                        "读缓存失败：{}",
                        "Failed to read cache: {}",
                        &[&e],
                    )));
                    return;
                }
            }
        } else {
            // 每次请求取当前在线 backend。首连进行中（冷启动 200ms 窗口后连接需 1-3s）时
            // 用条件变量高效等待：主线程连接成功时 notify，Reader 不再轮询（P2 优化）。
            // 真断网场景等满配置时长后仍明确报错（可配置，改善冷启动体验）
            let backend = {
                // 先快速检查当前状态（乐观路径：已连接直接返回）
                if let Some(b) = self
                    .backend_slot
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                {
                    Some(b)
                } else {
                    // backend 未就绪：用 Condvar 等待通知（替代 100ms 轮询）
                    let (lock, cvar) = &*self.backend_ready;
                    let start = Instant::now();
                    let mut ready = lock.lock().unwrap_or_else(|p| p.into_inner());
                    loop {
                        // 检查是否已被通知就绪
                        if *ready {
                            if let Some(b) = self
                                .backend_slot
                                .read()
                                .unwrap_or_else(|p| p.into_inner())
                                .clone()
                            {
                                break Some(b);
                            }
                            // ready 标志已置位但 backend_slot 仍空（罕见竞态）：继续等待
                        }
                        // 计算剩余超时时间
                        let elapsed = start.elapsed();
                        if elapsed >= self.cold_start_timeout {
                            break None;
                        }
                        let remaining = self.cold_start_timeout - elapsed;
                        // wait_timeout 高效等待：收到 notify 或超时时唤醒
                        let (guard, timed_out) = cvar
                            .wait_timeout(ready, remaining)
                            .unwrap_or_else(|p| p.into_inner());
                        ready = guard;
                        if timed_out.timed_out() {
                            break None;
                        }
                    }
                }
            };
            let Some(backend) = backend else {
                self.emit(Event::Status(
                    self.tr("未连接，无法拉取正文", "Not connected, cannot fetch body"),
                ));
                return;
            };
            let bytes = match backend.fetch_body(hash as u64) {
                Ok(b) => b,
                Err(e) => {
                    self.emit(Event::Status(self.trf(
                        "拉取正文失败：{}",
                        "Failed to fetch body: {}",
                        &[&e],
                    )));
                    return;
                }
            };
            if let Err(e) = std::fs::write(&path, &bytes) {
                self.emit(Event::Status(self.trf(
                    "正文落盘失败：{}",
                    "Failed to write body: {}",
                    &[&e],
                )));
                return;
            }
            store.mark_has_body(hash).ok();
            bytes
        };

        // 以下与旧 open_message 管线一致（seen 部分已上移主循环）
        let (parse_bytes, pgp_status) = pgp_decode(self.pgp.as_ref(), &bytes);
        let (body, is_html) = extract_body(&parse_bytes);
        if pgp_status.is_none() {
            store.index_body(hash, &body).ok(); // 加密邮件正文不进全文索引（安全默认）
        }
        let attachments = extract_attachments(&parse_bytes);
        let (to_addrs, cc_addrs) = extract_recipients(&parse_bytes);
        let unsubscribe = extract_unsubscribe(&parse_bytes);
        let inline_images = extract_inline_images(&parse_bytes);
        let remote_images = if let Some(html) = parse_bytes_html(&parse_bytes) {
            html::remote_img_urls(html.as_ref())
        } else {
            vec![]
        };
        let attachment_images: Vec<(String, Vec<u8>)> = m_atch_images(&parse_bytes);
        let attachment_previews = pdf_thumbnails(&parse_bytes, &self.data_dir);
        self.emit(Event::BodyLoaded {
            hash,
            attachment_images,
            attachment_previews,
            subject: row.subject.clone(),
            to_addrs,
            cc_addrs,
            unsubscribe,
            inline_images,
            remote_images,
            body,
            is_html,
            attachments,
            pgp: pgp_status,
        });
    }
}

/// M5-c：顶层 PGP/MIME 处理（Reader 线程用）。返回（用于正文解析的字节, 状态行）。
/// 失败不炸：返回原始字节 + 状态说明，正文管线照常跑（显示密文/原文）。
fn pgp_decode(
    rt: Option<&std::sync::Arc<pgp::PgpRuntime>>,
    raw: &[u8],
) -> (Vec<u8>, Option<String>) {
    match pgp::detect_pgp_kind(raw) {
        Some(pgp::PgpKind::Encrypted) => {
            let Some(rt) = rt else {
                return (raw.to_vec(), Some("🔒 加密邮件（未配置 PGP 私钥）".into()));
            };
            let Some(ciphertext) = pgp::extract_pgp_ciphertext(raw) else {
                return (raw.to_vec(), Some("🔒 加密邮件（找不到密文部分）".into()));
            };
            match rt.decrypt(&ciphertext) {
                Ok((plain, status)) => (plain, Some(status)),
                Err(e) => (raw.to_vec(), Some(format!("🔒 解密失败：{e}"))),
            }
        }
        Some(pgp::PgpKind::Signed) => {
            let Some((content, sig)) = pgp::signed_mime_parts(raw) else {
                return (raw.to_vec(), None);
            };
            let status = match rt {
                Some(rt) => rt.verify_detached(&content, &sig),
                None => "⚠ 签名未验证（未配置 PGP）".to_string(),
            };
            (raw.to_vec(), Some(status))
        }
        None => (raw.to_vec(), None),
    }
}

pub(crate) fn backoff(attempts: u32, max_secs: u64) -> Duration {
    let secs = 1u64.saturating_mul(1 << attempts.min(6));
    Duration::from_secs(secs.min(max_secs))
}

/// 从原始邮件抽 To/Cc 头（回复全部用；简单按头名取值，不解码 MIME 名字）
pub(crate) fn extract_recipients(raw: &[u8]) -> (String, String) {
    let txt = String::from_utf8_lossy(raw);
    let header_end = txt.find("\r\n\r\n").unwrap_or(txt.len());
    let head = &txt[..header_end];
    let mut to = String::new();
    let mut cc = String::new();
    // 0=To 1=Cc 2=无（折行续接归属上一个头）
    let mut cur = 2u8;
    for line in head.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("to:") {
            cur = 0;
            to.push_str(line[3..].trim());
        } else if lower.starts_with("cc:") {
            cur = 1;
            cc.push_str(line[3..].trim());
        } else if line.starts_with(' ') || line.starts_with('\t') {
            match cur {
                0 => {
                    to.push(' ');
                    to.push_str(line.trim());
                }
                1 => {
                    cc.push(' ');
                    cc.push_str(line.trim());
                }
                _ => {}
            }
        } else {
            cur = 2;
        }
    }
    (to.trim().to_string(), cc.trim().to_string())
}

/// "名字 <a@b>" → (a@b, 名字)；无尖括号 → (整串, "")。通讯录收集用
pub(crate) fn parse_from_addr(s: &str) -> (String, String) {
    match s.find('<') {
        Some(st) => {
            let email = s[st + 1..].trim_end_matches('>').trim().to_string();
            let name = s[..st].trim().to_string();
            (email, name)
        }
        None => (s.trim().to_string(), String::new()),
    }
}

pub(crate) fn split_addrs(s: &str) -> Vec<String> {
    s.split([',', ';', '，', '；'])
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// 状态文案双语（服务线程用；zh 由 SetLang 运行时切换）
pub(crate) fn tr_str(zh: bool, zh_s: &'static str, en_s: &'static str) -> String {
    if zh {
        zh_s.into()
    } else {
        en_s.into()
    }
}
pub(crate) fn trf_str(
    zh: bool,
    zh_s: &'static str,
    en_s: &'static str,
    args: &[&dyn std::fmt::Display],
) -> String {
    let mut out = if zh { zh_s } else { en_s }.to_string();
    for a in args {
        out = out.replacen("{}", &a.to_string(), 1);
    }
    out
}

#[cfg(test)]
mod unsubscribe_tests {
    use super::extract_unsubscribe;

    #[test]
    fn prefers_http_over_mailto() {
        let raw = b"From: a@b\r\n\
List-Unsubscribe: <https://ml.example.com/u/1>, <mailto:unsub@example.com>\r\n\
\r\nbody";
        assert_eq!(
            extract_unsubscribe(raw).as_deref(),
            Some("https://ml.example.com/u/1")
        );
    }

    #[test]
    fn mailto_fallback_and_folding() {
        // 折行续接 + 无 http 候选
        let raw = b"From: a@b\r\n\
List-Unsubscribe: <mailto:leave@example.com?subject=bye>,\r\n <https://x/1>\r\n\
\r\nbody";
        assert_eq!(extract_unsubscribe(raw).as_deref(), Some("https://x/1"));
        let raw2 = b"Subject: s\r\nList-Unsubscribe: <mailto:unsub@x>\r\n\r\nb";
        assert_eq!(extract_unsubscribe(raw2).as_deref(), Some("mailto:unsub@x"));
    }

    #[test]
    fn absent_or_garbage() {
        assert_eq!(extract_unsubscribe(b"From: a\r\n\r\nb"), None);
        // 值里的候选都不是 URL 形态
        assert_eq!(
            extract_unsubscribe(b"List-Unsubscribe: <ftp://x>, noise\r\n\r\nb"),
            None
        );
    }
}

/// 按名取任意头值（含折行续接；大小写不敏感）
fn extract_header_value(head: &str, name: &str) -> Option<String> {
    let lower_name = name.to_lowercase();
    let mut val = String::new();
    let mut hit = false;
    for line in head.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with(&format!("{lower_name}:")) {
            hit = true;
            val.push_str(line.split_once(':').map(|x| x.1).unwrap_or("").trim());
        } else if hit && (line.starts_with(' ') || line.starts_with('\t')) {
            val.push_str(line.trim());
        } else if hit {
            break;
        }
    }
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// List-Unsubscribe（RFC 2369/8058）：`<https://…>, <mailto:…>`，
/// 优先 http(s)（浏览器一键退订），退回 mailto
fn extract_unsubscribe(raw: &[u8]) -> Option<String> {
    let txt = String::from_utf8_lossy(raw);
    let header_end = txt.find("\r\n\r\n").unwrap_or(txt.len());
    let head = &txt[..header_end];
    let v = extract_header_value(head, "List-Unsubscribe")?;
    let mut http = None;
    let mut mailto = None;
    for part in v.split(',') {
        let p = part.trim().trim_start_matches('<').trim_end_matches('>');
        if p.starts_with("http://") || p.starts_with("https://") {
            http = http.or(Some(p.to_string()));
        } else if p.starts_with("mailto:") {
            mailto = mailto.or(Some(p.to_string()));
        }
    }
    http.or(mailto)
}

pub(crate) fn to_row(env: &EnvelopeSummary, folder: &str) -> MessageRow {
    MessageRow {
        envelope_hash: env.hash as i64,
        folder: folder.to_string(),
        message_id: env.message_id.clone(),
        in_reply_to: env.in_reply_to.clone(),
        subject: env.subject.clone(),
        from_addr: env.from_addr.clone(),
        date_unix: env.date_unix,
        seen: env.seen,
        flagged: env.flagged,
        has_body: false,
    }
}

/// M7：哈希按所在文件夹分组（批量操作一次 IMAP 往返一组）；
/// 本地已不存在的哈希安全跳过
pub(crate) fn group_by_folder(store: &Store, hashes: &[i64]) -> Vec<(String, Vec<i64>)> {
    let mut groups: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
    for h in hashes {
        if let Ok(Some(row)) = store.get(*h) {
            groups.entry(row.folder).or_default().push(*h);
        }
    }
    groups.into_iter().collect()
}

/// HTML 部分（无则 None）——远程图片 URL 提取用
fn parse_bytes_html(bytes: &[u8]) -> Option<String> {
    use mail_parser::MessageParser;
    Some(
        MessageParser::default()
            .parse(bytes)?
            .body_html(0)?
            .into_owned(),
    )
}

/// 内嵌图片（Content-Disposition: inline）：(归一化 cid, 字节)。
/// HTML 的 <img src="cid:…"> 引用它们；随邮件自带无隐私问题，自动显示
fn extract_inline_images(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    use mail_parser::{MessageParser, MimeHeaders};
    let Some(m) = MessageParser::default().parse(bytes) else {
        return vec![];
    };
    m.attachments()
        .filter(|a| {
            a.content_disposition()
                .map(|d| d.is_inline())
                .unwrap_or(false)
        })
        .filter_map(|a| {
            let cid = a
                .content_id()?
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string();
            if cid.is_empty() {
                return None;
            }
            Some((cid, a.contents().to_vec()))
        })
        .collect()
}

/// 附件清单（名字 + 大小）
/// 图片附件字节（预览用）：PNG/JPEG/GIF/WebP/BMP 魔数 + 单张 2MB
fn m_atch_images(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    use mail_parser::{MessageParser, MimeHeaders};
    const MAX: usize = 2 * 1024 * 1024;
    let Some(m) = MessageParser::default().parse(bytes) else {
        return vec![];
    };
    m.attachments()
        .filter_map(|a| {
            let name = a.attachment_name()?.to_string();
            let c = a.contents();
            if c.len() > MAX || !is_image_content(c) {
                return None;
            }
            Some((name, c.to_vec()))
        })
        .collect()
}

pub fn is_image_content(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG")
        || b.starts_with(b"\xff\xd8\xff")
        || b.starts_with(b"GIF8")
        || b.starts_with(b"RIFF") && b.len() > 11 && &b[8..12] == b"WEBP"
        || b.starts_with(b"BM")
}

/// PDF 附件 → QuickLook 缩略图 PNG（macOS qlmanage；非 macOS / 失败返回空）
fn pdf_thumbnails(bytes: &[u8], data_dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (bytes, data_dir);
        vec![]
    }
    #[cfg(target_os = "macos")]
    {
        use mail_parser::{MessageParser, MimeHeaders};
        let Some(m) = MessageParser::default().parse(bytes) else {
            return vec![];
        };
        let tmp_dir = data_dir.join("ql-tmp");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let mut out = Vec::new();
        for (i, a) in m.attachments().enumerate() {
            let Some(name) = a.attachment_name() else {
                continue;
            };
            if !name.to_lowercase().ends_with(".pdf") {
                continue;
            }
            let contents = a.contents();
            if contents.len() > 20 * 1024 * 1024 {
                continue;
            } // 20MB 上限
            let pdf_path = tmp_dir.join(format!("preview-{i}.pdf"));
            if std::fs::write(&pdf_path, contents).is_err() {
                continue;
            }
            // qlmanage -t -s 480：480px 宽 PNG 缩略图
            let png_path = tmp_dir.join(format!("preview-{i}.pdf.png"));
            let ok = std::process::Command::new("qlmanage")
                .args([
                    "-t",
                    "-s",
                    "480",
                    "-o",
                    tmp_dir.to_str().unwrap_or("/tmp"),
                    pdf_path.to_str().unwrap_or(""),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                if let Ok(png) = std::fs::read(&png_path) {
                    out.push((name.to_string(), png));
                }
                let _ = std::fs::remove_file(&png_path);
            }
            let _ = std::fs::remove_file(&pdf_path);
        }
        let _ = std::fs::remove_dir(&tmp_dir);
        out
    }
}

fn extract_attachments(bytes: &[u8]) -> Vec<(String, usize)> {
    use mail_parser::{MessageParser, MimeHeaders};
    let Some(m) = MessageParser::default().parse(bytes) else {
        return vec![];
    };
    m.attachments()
        .filter_map(|a| {
            a.attachment_name()
                .map(|n| (n.to_string(), a.contents().len()))
        })
        .collect()
}

/// 按名取附件字节
pub(crate) fn extract_attachment_bytes(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
    use mail_parser::{MessageParser, MimeHeaders};
    let m = MessageParser::default().parse(bytes)?;
    let found = m
        .attachments()
        .find(|a| a.attachment_name() == Some(name))?;
    Some(found.contents().to_vec())
}

/// 从原始 EML 提取可读内容。
/// 策略（M4）：有 text/html 就走消毒管线转 markdown（现代邮件 HTML 表达力完整）；
/// 否则 text/plain 原样（GBK 等字符集由 mail-parser 处理）。
/// 返回 (内容, 是否为 markdown)。
pub fn extract_body(bytes: &[u8]) -> (String, bool) {
    use mail_parser::MessageParser;
    let Some(message) = MessageParser::default().parse(bytes) else {
        return (format!("[无法解析邮件正文]（{} 字节）", bytes.len()), false);
    };
    if let Some(html) = message.body_html(0) {
        return (crate::html::html_to_markdown(html.as_ref()), true);
    }
    if let Some(text) = message.body_text(0) {
        return (text.into_owned(), false);
    }
    (format!("[无文本部分]（{} 字节）", bytes.len()), false)
}

#[cfg(test)]
mod tr_tests {
    use super::{tr_str, trf_str};

    #[test]
    fn bilingual_selection_and_fill() {
        assert_eq!(tr_str(true, "已重连", "Reconnected"), "已重连");
        assert_eq!(tr_str(false, "已重连", "Reconnected"), "Reconnected");
        assert_eq!(
            trf_str(true, "已删除 {} 封", "Deleted {} message(s)", &[&3]),
            "已删除 3 封"
        );
        assert_eq!(
            trf_str(false, "已删除 {} 封", "Deleted {} message(s)", &[&3]),
            "Deleted 3 message(s)"
        );
        // 多参数依序填充
        assert_eq!(trf_str(false, "{} → {}", "{} → {}", &[&"a", &"b"]), "a → b");
    }
}

#[cfg(test)]
mod recipient_tests {
    use super::extract_recipients;

    #[test]
    fn to_cc_with_folding() {
        let raw = b"From: a@b\r\nTo: Name One <one@x>, two@x,\r\n three@x\r\nCc: cc1@x\r\nSubject: s\r\n\r\nbody";
        let (to, cc) = extract_recipients(raw);
        assert!(
            to.contains("one@x") && to.contains("two@x") && to.contains("three@x"),
            "{to}"
        );
        assert_eq!(cc, "cc1@x");
    }
}
