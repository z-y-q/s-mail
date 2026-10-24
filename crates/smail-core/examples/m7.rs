//! M5-a 端到端：会话聚合（In-Reply-To 线程化）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   python3 spikes/spike-melib/imap-test/seed_thread.py   # 注入 A←B←C 回信链
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m7 \
//!     cargo run -p smail-core --example m7 -- 45
//!
//! 通过标准：INBOX 同步后 seed 链三封都在且 in_reply_to 落库正确 →
//! group_threads 聚出 3 员工会话（根=A、按时间升序、最新活动排序在顶）→
//! 孤儿邮件（无 irt 或父件缺失）各自成单封会话 → 退出码 0。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::threads::group_threads;
use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

fn norm(s: &str) -> String {
    s.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(45);

    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("spikes/spike-melib/config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m7".into());

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m7 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m7] Session 已启动");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut rows = Vec::new();

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::Status(s) => println!("[status {account}] {s}"),
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
                Event::FolderLoaded { folder, rows: r } if folder.eq_ignore_ascii_case("INBOX") => {
                    rows = r;
                    println!("[loaded {account}] INBOX: {} 封", rows.len());
                }
                _ => {}
            }
        }
        if !rows.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);

    // ---- 1. 种子链落库核验 ----
    let find = |mid: &str| rows.iter().find(|r| norm(&r.message_id) == mid).cloned();
    let (_a, b, c) = match (find("m7-a@seed"), find("m7-b@seed"), find("m7-c@seed")) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => {
            eprintln!("[m7 FAIL] 种子链未同步全（{} 封）", rows.len());
            std::process::exit(1);
        }
    };
    let irt_ok = norm(&b.in_reply_to) == "m7-a@seed" && norm(&c.in_reply_to) == "m7-b@seed";
    println!(
        "[m7] in_reply_to 落库：B→A={} C→B={}（{irt_ok}）",
        b.in_reply_to, c.in_reply_to
    );

    // ---- 2. 聚合断言 ----
    let ts = group_threads(&rows);
    let chain = ts
        .iter()
        .find(|t| t.members.iter().any(|m| norm(&m.message_id) == "m7-a@seed"));
    let mut chain_ok = false;
    let mut order_ok = false;
    // 会话间按最新活动降序（整体有序 + 链的位置在所有更旧会话之前）
    let sorted_desc = ts
        .windows(2)
        .all(|w| w[0].latest().date_unix >= w[1].latest().date_unix);
    let chain_idx = ts
        .iter()
        .position(|t| t.members.iter().any(|m| norm(&m.message_id) == "m7-a@seed"));
    let top_ok = sorted_desc
        && chain_idx.is_some_and(|ci| {
            ts.iter()
                .enumerate()
                .all(|(i, t)| i <= ci || t.latest().date_unix <= ts[ci].latest().date_unix)
        });
    if let Some(t) = chain {
        chain_ok = t.members.len() == 3;
        order_ok = t
            .members
            .iter()
            .map(|m| norm(&m.message_id))
            .collect::<Vec<_>>()
            == vec!["m7-a@seed", "m7-b@seed", "m7-c@seed"];
        println!(
            "[m7] 会话：{} 封、根={}、升序={order_ok}、排序位置正确={top_ok}（第 {} 位）",
            t.members.len(),
            t.root().subject,
            chain_idx.unwrap_or(999)
        );
    }
    let singles = ts.iter().filter(|t| t.members.len() == 1).count();
    let total: usize = ts.iter().map(|t| t.members.len()).sum();
    let no_loss = total == rows.len();
    println!(
        "[m7] 会话数={}（单封 {}）、成员总数 {total}/{} 无丢失（{no_loss}）",
        ts.len(),
        singles,
        rows.len()
    );

    if irt_ok && chain_ok && order_ok && top_ok && no_loss {
        println!(
            "\n[m7 PASS] In-Reply-To 落库 ✅ 三级链聚合 ✅ 时间序 ✅ 最新活动置顶 ✅ 无丢失 ✅"
        );
        std::process::exit(0);
    }
    println!(
        "\n[m7 FAIL] irt={irt_ok} chain={chain_ok} order={order_ok} top={top_ok} no_loss={no_loss}"
    );
    std::process::exit(1);
}
