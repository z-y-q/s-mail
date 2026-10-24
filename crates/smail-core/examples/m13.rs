//! M13 端到端：服务端删除同步（他处/网页版删信 → 本地自动清理）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m13 \
//!     cargo run -p smail-core --example m13 -- 60
//!
//! 流程：播种唯一邮件 → 首同步含靶子 → 另一 IMAP 客户端删靶子 →
//! 容器内注入触发信驱动 IDLE 重同步 → 本地靶子消失、计数守恒（+触发 -靶子）。

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

fn b64(s: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("spikes/spike-melib/config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m13".into());
    let _ = std::fs::remove_dir_all(&home);

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mid = format!("m13-target-{ts}@seed");
    let subject = format!("m13删除靶子{ts}");

    // ---- 1. 播种唯一靶子（host 侧写 maildir；SELECT 时 dovecot 现扫目录） ----
    let raw = format!(
        "From: =?UTF-8?B?{}?= <m13@local>\r\nTo: test@localhost\r\nSubject: =?UTF-8?B?{}?=\r\nDate: Sat, 18 Oct 2026 15:30:00 +0800\r\nMessage-ID: <{mid}>\r\n\r\nbody\r\n",
        b64("m13"),
        b64(&subject)
    );
    let maildir = "spikes/spike-melib/imap-test/mail/test/Maildir/new";
    std::fs::write(format!("{maildir}/{ts}.M99P1.local"), raw).unwrap();
    println!("[m13] 靶子已播种 mid={mid} subject={subject:?}");

    // ---- 2. Session 首同步 ----
    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m13 启动失败] {e}");
            std::process::exit(1);
        }
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut initial: Option<usize> = None;
    let mut target: Option<i64> = None;
    let mut injected = false;
    let mut trig_subject = String::new();
    let mut after: Option<(usize, bool)> = None; // (count, target_gone)
                                                 // 事件静默期判定：触发信快照后继续泵事件，新快照刷新计时器（最后快照
                                                 // 为准），静默满 STABLE 后才认定同步稳定——替代旧版「睡 1200ms 赌稳定」
                                                 // （慢 CI 下 1200ms 不够会误判，快环境下又白等）
    const STABLE: Duration = Duration::from_millis(1500);
    let mut after_at: Option<Instant> = None;

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::Status(s) => {
                    if s.contains("本地同步清理") {
                        println!("[m13] {s}");
                    }
                }
                Event::Error(s) => println!("[error ] {s}"),
                Event::Folders(f) => {
                    println!(
                        "[folders] {:?}",
                        f.iter().map(|x| x.path.as_str()).collect::<Vec<_>>()
                    );
                    let _ = session.cmd_tx.send(SessionCmd::Route {
                        account,
                        cmd: Cmd::SelectFolder("INBOX".into()),
                    });
                }
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") {
                        if !injected {
                            if let Some(r) = rows.iter().find(|r| r.subject == subject) {
                                if target.is_none() {
                                    target = Some(r.envelope_hash);
                                    initial = Some(rows.len());
                                    println!(
                                        "[m13] 首同步 {} 封，靶子 hash={}",
                                        rows.len(),
                                        r.envelope_hash
                                    );
                                }
                            }
                        } else {
                            // 只在本轮触发信可见的快照上判定：避免两类旧视图抢先——
                            // 1) SelectFolder 驱动的同步与删除并发（含靶子）；
                            // 2) 历轮残留的 m13-trigger 信让条件恒真
                            if rows.iter().any(|r| r.subject == trig_subject) {
                                let gone = rows.iter().all(|r| Some(r.envelope_hash) != target);
                                after = Some((rows.len(), gone));
                                after_at = Some(Instant::now());
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // ---- 3. 他处删除 + 容器内触发信 ----
        if target.is_some() && !injected {
            injected = true;
            println!("[m13] 另一客户端删除靶子…");
            let out = std::process::Command::new("python3")
                .args(["spikes/spike-melib/imap-test/imap_delete.py", &mid])
                .output()
                .expect("python3");
            if !out.status.success() {
                eprintln!(
                    "[m13 FAIL] 删除失败：{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                std::process::exit(1);
            }
            // 容器内写触发信（virtiofs 不传 inotify，必须容器内写才驱动 IDLE）
            let t2 = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            trig_subject = format!("m13-trigger-{t2}");
            let trig = format!(
                "From: t@local\r\nTo: test@localhost\r\nSubject: m13-trigger-{t2}\r\nDate: Sat, 18 Oct 2026 15:31:00 +0800\r\nMessage-ID: <m13-trigger-{t2}@t>\r\n\r\nx\r\n"
            );
            let script = format!(
                "printf '%s' '{}' > /var/mail/test/Maildir/new/m13t.{t2}",
                trig
            );
            let _ = std::process::Command::new("docker")
                .args(["exec", "smail-imap", "sh", "-c", &script])
                .status();
            println!("[m13] 已删 + 注入触发信，等 IDLE 重同步…");
        }

        if after_at.is_some_and(|t| t.elapsed() >= STABLE) {
            break; // 静默期内无新快照：最后一份即稳定结论
        }
        std::thread::sleep(Duration::from_millis(100)); // 静默计时轮询（事件在上方 try_recv 泵）
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);

    let (n0, n1, gone) = (
        initial.unwrap_or(0),
        after.map(|a| a.0).unwrap_or(0),
        after.map(|a| a.1).unwrap_or(false),
    );
    if n0 > 0 && gone && n1 == n0 {
        println!("\n[m13 PASS] 他处删除 → 本地清理 ✅（{n0} → {n1}：+触发信 −靶子）");
        std::process::exit(0);
    }
    println!("\n[m13 FAIL] n0={n0} n1={n1} 靶子消失={gone}（期望 n1=n0 且消失）");
    std::process::exit(1);
}
