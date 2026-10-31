//! M2 端到端（无 UI）：发信 → Sent 归档 → 删除。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh              # dovecot（含 Sent 文件夹）
//!   python3 spikes/spike-melib/imap-test/mini_smtp.py 11525 &
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m2 \
//!     cargo run -p smail-core --example m2 -- 40
//!
//! 通过标准：MailSent + Sent 出现主题 "M2端到端" + MessageDeleted + 退出码 0。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{AccountService, Cmd, Event};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);

    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m2".into());

    let (mut service, mut rx) = match AccountService::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m2 启动失败] {e}");
            std::process::exit(1);
        }
    };

    let subject = "M2端到端";
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut sent_ok = false;
    let mut sent_loaded = false;
    let mut deleted_ok = false;
    let mut send_sent = false;

    // 轮询而非阻塞 recv：deadline 稳定生效
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
                        println!("[send  ] 发送测试邮件");
                        let _ = service.cmd_tx.send(Cmd::SendMail {
                            to: "test@local".into(),
                            cc: String::new(),
                            bcc: String::new(),
                            subject: subject.into(),
                            body: "M2 端到端测试正文。".into(),
                            attachments: vec![],
                            sign: false,
                            encrypt: false,
                            high_priority: false,
                        });
                    }
                }
                Event::FolderLoaded { folder, rows } => {
                    println!("[loaded ] {folder}: {} 封", rows.len());
                    if folder.to_lowercase().contains("sent") {
                        if let Some(r) = rows.iter().find(|r| r.subject == subject) {
                            sent_loaded = true;
                            println!("[sent   ] Sent 里找到了 {:?}", r.subject);
                            if !deleted_ok {
                                println!("[delete ] 删除它");
                                let _ = service.cmd_tx.send(Cmd::DeleteMessage(r.envelope_hash));
                            }
                        }
                    }
                }
                Event::MailSent { to, subject } => {
                    sent_ok = true;
                    println!("[mailsent] → {to} ({subject})");
                }
                Event::MessageDeleted(hash) => {
                    deleted_ok = true;
                    println!("[deleted ] {hash}");
                }
                _ => {}
            }
        }
        if sent_ok && sent_loaded && deleted_ok {
            println!("\n[m2 PASS] 发送 ✅ + Sent 归档 ✅ + 删除 ✅");
            service.stop();
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("\n[m2 FAIL] sent={sent_ok} sent_loaded={sent_loaded} deleted={deleted_ok}");
    service.stop();
    std::process::exit(1);
}
