//! M3 端到端：带附件发信 → Sent 归档 → 打开（附件可见）→ 保存附件 → 全文搜索。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   python3 spikes/spike-melib/imap-test/mini_smtp.py 11525 &
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m3 \
//!   SMAIL_DOWNLOADS=/tmp/smail-m3-dl SMAIL_NO_REVEAL=1 \
//!     cargo run -p smail-core --example m3 -- 60

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{AccountService, Cmd, Event};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m3".into());

    // 准备源附件与下载目录
    let src_dir = std::path::Path::new("/tmp/smail-m3-src");
    let _ = std::fs::remove_dir_all(src_dir);
    std::fs::create_dir_all(src_dir).unwrap();
    let attach_path = src_dir.join("测试附件-m3.txt");
    let attach_body = "s-mail M3 attachment payload 独一无二正文词XYZ";
    std::fs::write(&attach_path, attach_body).unwrap();
    let dl_dir = PathBuf::from("/tmp/smail-m3-dl");
    let _ = std::fs::remove_dir_all(&dl_dir);
    std::fs::create_dir_all(&dl_dir).unwrap();

    let (mut service, mut rx) = match AccountService::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m3 启动失败] {e}");
            std::process::exit(1);
        }
    };

    let subject = "M3附件搜索测试";
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut sent_ok = false;
    let mut opened_ok = false;
    let mut saved_ok = false;
    let mut search_subject_ok = false;
    let mut search_body_ok = false;
    let mut sent_hash: Option<i64> = None;
    let mut attach_name = String::new();
    let mut send_sent = false;
    let mut opened_sent = false;

    while Instant::now() < deadline {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Status(s) => println!("[status] {s}"),
                Event::Error(s) => println!("[error ] {s}"),
                Event::Folders(f) => {
                    println!(
                        "[folders] {:?}",
                        f.iter().map(|x| x.path.as_str()).collect::<Vec<_>>()
                    );
                    if !send_sent {
                        send_sent = true;
                        println!("[send   ] 带附件发送");
                        let _ = service.cmd_tx.send(Cmd::SendMail {
                            to: "test@local".into(),
                            cc: String::new(),
                            bcc: String::new(),
                            subject: subject.into(),
                            body: "正文包含：独一无二正文词XYZ".into(),
                            attachments: vec![attach_path.display().to_string()],
                            sign: false,
                            encrypt: false,
                            high_priority: false,
                        });
                    }
                }
                Event::FolderLoaded { folder, rows } => {
                    if folder.to_lowercase().contains("sent") {
                        if let Some(r) = rows.iter().find(|r| r.subject == subject) {
                            sent_hash = Some(r.envelope_hash);
                            if !opened_sent {
                                opened_sent = true;
                                println!("[open   ] 打开发出的邮件");
                                let _ = service.cmd_tx.send(Cmd::OpenMessage(r.envelope_hash));
                            }
                        }
                    }
                }
                Event::MailSent { to, subject } => {
                    sent_ok = true;
                    println!("[mailsent] → {to} ({subject})");
                }
                Event::BodyLoaded {
                    hash, attachments, ..
                } => {
                    if Some(hash) == sent_hash {
                        opened_ok = !attachments.is_empty();
                        println!(
                            "[opened ] 附件 {} 个: {:?}",
                            attachments.len(),
                            attachments.iter().map(|a| &a.0).collect::<Vec<_>>()
                        );
                        if let Some((name, _)) = attachments.first() {
                            attach_name = name.clone();
                            println!("[save   ] 保存附件 {name:?}");
                            let _ = service.cmd_tx.send(Cmd::SaveAttachment {
                                hash,
                                name: name.clone(),
                            });
                        }
                    }
                }
                Event::AttachmentSaved { path } => {
                    let content = std::fs::read(&path).unwrap_or_default();
                    saved_ok = content == attach_body.as_bytes();
                    println!(
                        "[saved  ] {path} 内容匹配: {saved_ok}（{} 字节）",
                        content.len()
                    );
                }
                Event::SearchResults { query, rows, .. } => {
                    let hit = rows.iter().any(|r| r.subject == subject);
                    if query.contains("附件") && hit {
                        search_subject_ok = true;
                        println!("[search ] 主题搜索 “{query}” 命中（{} 条）", rows.len());
                    }
                    if query.contains("独一无二") && hit {
                        search_body_ok = true;
                        println!("[search ] 正文搜索 “{query}” 命中（{} 条）", rows.len());
                    }
                }
                _ => {}
            }
        }

        // 依次触发搜索
        if sent_ok && !search_subject_ok {
            let _ = service.cmd_tx.send(Cmd::Search("附件".into()));
        }
        if saved_ok && !search_body_ok {
            let _ = service.cmd_tx.send(Cmd::Search("独一无二正文词".into()));
        }

        if sent_ok
            && opened_ok
            && saved_ok
            && search_subject_ok
            && search_body_ok
            && !attach_name.is_empty()
        {
            println!("\n[m3 PASS] 发送 ✅ 附件可见 ✅ 保存内容一致 ✅ 主题搜索 ✅ 正文搜索 ✅");
            service.stop();
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "\n[m3 FAIL] sent={sent_ok} opened={opened_ok} saved={saved_ok} \
         search_subj={search_subject_ok} search_body={search_body_ok}"
    );
    service.stop();
    std::process::exit(1);
}
