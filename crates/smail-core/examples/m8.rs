//! M5-b 端到端：规则过滤（新邮件自动 标已读 / 星标 / 移动）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   python3 spikes/spike-melib/imap-test/seed_rules.py
//!   SMAIL_CONFIG=spikes/spike-melib/config.rules.toml SMAIL_HOME=/tmp/smail-m8 \
//!     cargo run -p smail-core --example m8 -- 45
//!
//! 通过标准：INBOX 同步后——promo×2 自动已读、周报自动星标、"挪走我"不在 INBOX、
//! 对照组不受影响 → 切到 Sent 看到"挪走我"（服务器端真实移动）→
//! 规则状态事件出现 → 退出码 0。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

fn find<'a>(rows: &'a [smail_store::MessageRow], sub: &str) -> Option<&'a smail_store::MessageRow> {
    rows.iter().find(|r| r.subject.contains(sub))
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(45);

    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("spikes/spike-melib/config.rules.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m8".into());

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m8 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m8] Session 已启动（规则账号）");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut inbox_rows = Vec::new();
    let mut sent_rows = Vec::new();
    let mut rule_status_seen = false;
    let mut requested_sent = false;

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::Status(s) => {
                    if s.starts_with("规则：") {
                        rule_status_seen = true;
                    }
                    println!("[status {account}] {s}");
                }
                Event::Error(s) => println!("[error  {account}] {s}"),
                Event::Folders(f) => {
                    println!(
                        "[folders {account}] {:?}",
                        f.iter().map(|x| x.path.as_str()).collect::<Vec<_>>()
                    );
                    let _ = session.cmd_tx.send(SessionCmd::Route {
                        account,
                        cmd: Cmd::SelectFolder("INBOX".into()),
                    });
                }
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") {
                        inbox_rows = rows;
                        println!("[loaded {account}] INBOX: {} 封", inbox_rows.len());
                    } else if folder.eq_ignore_ascii_case("Sent") {
                        sent_rows = rows;
                        println!("[loaded {account}] Sent: {} 封", sent_rows.len());
                    }
                }
                _ => {}
            }
        }

        if !requested_sent && !inbox_rows.is_empty() {
            requested_sent = true;
            println!("[m8] 切到 Sent 验证移动结果");
            let _ = session.cmd_tx.send(SessionCmd::Route {
                account: 0,
                cmd: Cmd::SelectFolder("Sent".into()),
            });
        }
        if !sent_rows.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);

    // ---- 断言 ----

    let promo_seen_ok = ["限时优惠", "错过再等一年"]
        .iter()
        .all(|s| find(&inbox_rows, s).map(|r| r.seen).unwrap_or(false));
    let boss_flag_ok = find(&inbox_rows, "周报汇总")
        .map(|r| r.flagged)
        .unwrap_or(false);
    let moved_gone = find(&inbox_rows, "挪走我").is_none();
    let neutral_ok = find(&inbox_rows, "对照组")
        .map(|r| !r.seen && !r.flagged)
        .unwrap_or(false);
    let in_sent = find(&sent_rows, "挪走我").is_some();

    println!(
        "[m8] promo已读={promo_seen_ok} 周报星标={boss_flag_ok} INBOX无挪走我={moved_gone} \\
对照组不受影响={neutral_ok} Sent有挪走我={in_sent} 规则状态事件={rule_status_seen}"
    );

    if promo_seen_ok && boss_flag_ok && moved_gone && neutral_ok && in_sent && rule_status_seen {
        println!("\n[m8 PASS] 自动已读 ✅ 自动星标 ✅ 服务器端移动 ✅ 对照组无副作用 ✅");
        std::process::exit(0);
    }
    println!("\n[m8 FAIL]");
    std::process::exit(1);
}
