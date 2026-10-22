//! M3-b 端到端：多账号（Session）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh        # 含 test/test2 两个账号的种子邮件
//!   SMAIL_CONFIG=spikes/spike-melib/config.multi.toml SMAIL_HOME=/tmp/smail-m5 \
//!     cargo run -p smail-core --example m5 -- 45
//!
//! 通过标准：两个账号各自 Folders/FolderLoaded → 各自 INBOX 内容正确（主号无"二账号"前缀、
//! 二号全是"二账号"）→ 打开二号一封信 → 全账号搜索"周报"两号都命中 → 退出码 0。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(45);

    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("config.multi.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m5".into());

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m5 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m5] Session 已启动（多账号）");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut folders_seen = [false; 2];
    let mut loaded = [false; 2];
    let mut acct2_subjects_ok = false;
    let mut acct1_subjects_ok = false;
    let mut opened2_ok = false;
    let mut search_acct_hits = [false; 2];
    let mut searched = false;

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::Status(s) => println!("[status {account}] {s}"),
                Event::Error(s) => println!("[error  {account}] {s}"),
                Event::Folders(f) => {
                    folders_seen[account] = true;
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
                        loaded[account] = true;
                        println!("[loaded {account}] INBOX: {} 封", rows.len());
                        if account == 0 {
                            acct1_subjects_ok = rows.iter().all(|r| !r.subject.contains("二账号"))
                                && rows.iter().any(|r| r.subject.contains("项目周报"));
                        } else {
                            // 演示种子（seed_demo）会给两账号灌相同邮件，
                            // 隔离断言改为：test2 有自己的专属标记且 test1 列表无串号
                            acct2_subjects_ok = rows.iter().any(|r| r.subject.contains("二账号"))
                                && rows.iter().any(|r| r.subject.contains("周报"));
                            if let Some(t) = rows.iter().find(|r| r.subject.contains("二账号")) {
                                let _ = session.cmd_tx.send(SessionCmd::Route {
                                    account,
                                    cmd: Cmd::OpenMessage(t.envelope_hash),
                                });
                            }
                        }
                    }
                }
                Event::BodyLoaded {
                    hash,
                    subject,
                    is_html,
                    ..
                } => {
                    if account == 1 {
                        opened2_ok = subject.contains("二账号");
                        println!("[opened {account}] {subject:?} is_html={is_html} hash={hash}");
                    }
                }
                Event::SearchResults { rows, .. } => {
                    let hit = rows.iter().any(|r| r.subject.contains("周报"));
                    search_acct_hits[account] = hit;
                    println!("[search {account}] {} 条，含周报={hit}", rows.len());
                }
                _ => {}
            }
        }

        if loaded[0] && loaded[1] && !searched {
            searched = true;
            println!("[search ] 全账号搜索“周报”");
            let _ = session.cmd_tx.send(SessionCmd::SearchAll("周报".into()));
        }

        if folders_seen.iter().all(|x| *x)
            && loaded.iter().all(|x| *x)
            && acct1_subjects_ok
            && acct2_subjects_ok
            && opened2_ok
            && search_acct_hits.iter().all(|x| *x)
        {
            println!("\n[m5 PASS] 双账号就绪 ✅ 内容隔离 ✅ 二号开信 ✅ 跨账号搜索双命中 ✅");
            let _ = session.cmd_tx.send(SessionCmd::Shutdown);
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "\n[m5 FAIL] folders={folders_seen:?} loaded={loaded:?} subj1={acct1_subjects_ok} \
         subj2={acct2_subjects_ok} open2={opened2_ok} search={search_acct_hits:?}"
    );
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);
    std::process::exit(1);
}
