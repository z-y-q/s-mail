//! headless 端到端验证（无 UI）：
//!
//!   SMAIL_CONFIG=xxx.toml SMAIL_HOME=/tmp/smail-test cargo run -p smail-core --example headless -- [运行秒数]
//!
//! 流程：启动服务 → 等 INBOX 加载 → 打开第一封（拉正文落盘）→ 打印事件流。
//! 配合本地 dovecot：spikes/spike-melib/imap-test/up.sh 起服务器后运行本例。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{AccountService, Cmd, Event};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| {
            let mut p = std::env::temp_dir();
            p.push("smail-headless");
            p
        });

    let (mut service, mut event_rx) = match AccountService::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[headless 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[headless] 服务已启动，data dir = {}", home.display());

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut opened = false;
    while Instant::now() < deadline {
        let Some(ev) = event_rx.try_recv().ok() else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        match ev {
            Event::Status(s) => println!("[status] {s}"),
            Event::Error(s) => println!("[error ] {s}"),
            Event::Folders(f) => println!("[folders] {f:?}"),
            Event::FolderLoaded { folder, rows } => {
                println!("[loaded ] {folder}: {} 封", rows.len());
                for r in rows.iter().take(3) {
                    println!("    {} | {} | {}", r.date_unix, r.from_addr, r.subject);
                }
                if !opened {
                    if let Some(first) = rows.first() {
                        opened = true;
                        println!(
                            "[open   ] 请求正文 {} ({})",
                            first.envelope_hash, first.subject
                        );
                        let _ = service.cmd_tx.send(Cmd::OpenMessage(first.envelope_hash));
                    }
                }
            }
            Event::BodyLoaded {
                hash,
                subject,
                body,
                ..
            } => {
                let preview: String = body.chars().take(120).collect();
                println!("[body   ] {hash} {subject:?}\n    {preview:?}");
                // 继续跑到 deadline（观察重连等后续事件）
            }
            _ => {}
        }
    }

    service.stop();
    println!("[headless] 完成");
}
