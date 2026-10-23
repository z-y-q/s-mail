//! M17 e2e：离线优先——重启后不连服务器也能看到文件夹列表与缓存邮件。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m17 \
//!     cargo run -p smail-core --example m17 -- 90
//!
//! 流程：Phase A 正常同步 INBOX（落本地索引）→ 关闭会话 → **停掉 dovecot** →
//! Phase B 同一 SMAIL_HOME 起新会话：在服务器不可达的情况下，
//!   ① 应收到本地广播的 Folders（含 INBOX）；
//!   ② SelectFolder("INBOX") 应回 FolderLoaded 且行数 > 0（本地索引直出）。
//! 通过标准：两条都满足 → exit 0（邮箱客户端的核心承诺：缓存离线可用）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{Cmd, Event, Session, SessionCmd};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);
    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("spikes/spike-melib/config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m17".into());
    let _ = std::fs::remove_dir_all(&home);

    // ---- Phase A：在线首同步，INBOX 落本地索引 ----
    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m17 启动失败] {e}");
            std::process::exit(1);
        }
    };
    let _ = session.cmd_tx.send(SessionCmd::Route {
        account: 0,
        cmd: Cmd::SelectFolder("INBOX".into()),
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut synced_rows = 0usize;
    while Instant::now() < deadline {
        while let Ok(smail_core::TaggedEvent { event, .. }) = rx.try_recv() {
            if let Event::FolderLoaded { rows, .. } = event {
                if !rows.is_empty() {
                    synced_rows = rows.len();
                    break;
                }
            }
        }
        if synced_rows > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(rx);
    drop(session); // Drop：Shutdown + join（数据目录独占，Phase B 可重开）
    if synced_rows == 0 {
        println!("[m17 FAIL] Phase A 未同步到邮件（dovecot 未起？）");
        std::process::exit(1);
    }
    println!("[m17] Phase A：在线同步 {} 封", synced_rows);

    // ---- 停服务器：模拟离线 ----
    let _ = std::process::Command::new("docker")
        .args(["stop", "smail-imap"])
        .output();

    // ---- Phase B：服务器不可达，本地广播先行 ----
    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m17 FAIL] Phase B 会话启动失败：{e}");
            std::process::exit(1);
        }
    };
    let mut folders_local = false;
    let mut loaded_local = 0usize;
    let mut selected = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && (loaded_local == 0 || !folders_local) {
        while let Ok(smail_core::TaggedEvent { event, .. }) = rx.try_recv() {
            match event {
                Event::Folders(infos) => {
                    folders_local = infos.iter().any(|f| f.path == "INBOX");
                    if folders_local {
                        println!("[m17] ① 服务器不可达，本地 Folders 已广播（含 INBOX）");
                        if !selected {
                            selected = true;
                            let _ = session.cmd_tx.send(SessionCmd::Route {
                                account: 0,
                                cmd: Cmd::SelectFolder("INBOX".into()),
                            });
                        }
                    }
                }
                Event::FolderLoaded { rows, .. } => {
                    if !rows.is_empty() {
                        loaded_local = rows.len();
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(rx);
    drop(session);

    // 恢复服务器：CI e2e 矩阵中后续用例（m18 等）仍需 dovecot 存活。
    // （曾在这里 docker rm 容器 + stop 后不恢复，后续用例全部连不上）
    let _ = std::process::Command::new("docker")
        .args(["start", "smail-imap"])
        .output();

    if folders_local && loaded_local > 0 {
        println!(
            "[m17 PASS] 离线 Folders ✅ 离线列表 {} 封 ✅（重启不依赖服务器）",
            loaded_local
        );
        std::process::exit(0);
    }
    println!("[m17 FAIL] folders_local={folders_local} loaded_local={loaded_local}");
    std::process::exit(1);
}
