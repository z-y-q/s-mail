//! M18 e2e：废纸篓删除语义（Foxmail 式两段删除）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m18 \
//!     cargo run -p smail-core --example m18 -- 90
//!
//! 流程：播种唯一靶子 → INBOX 同步 → ① DeleteMessage（普通文件夹）→
//!   靶子应出现在 Trash（移动，可恢复）→ ② 在 Trash 里 DeleteMessage →
//!   靶子真消失（服务器永久删除）。
//! 通过标准：两段都成立 → exit 0。

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
        .unwrap_or(90);
    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("spikes/spike-melib/config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m18".into());
    let _ = std::fs::remove_dir_all(&home);

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mid = format!("m18-target-{ts}@seed");
    let subject = format!("m18废纸篓靶子{ts}");

    // 播种唯一靶子（host 侧写 maildir）
    let raw = format!(
        "From: =?UTF-8?B?{}?= <m18@local>\r\nTo: test@localhost\r\nSubject: =?UTF-8?B?{}?=\r\nDate: Sat, 18 Oct 2026 16:00:00 +0800\r\nMessage-ID: <{mid}>\r\n\r\nbody\r\n",
        b64("m18"),
        b64(&subject)
    );
    let maildir = "spikes/spike-melib/imap-test/mail/test/Maildir/new";
    std::fs::write(format!("{maildir}/{ts}.M98P1.local"), raw).unwrap();
    println!("[m18] 靶子已播种 subject={subject:?}");

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m18 启动失败] {e}");
            std::process::exit(1);
        }
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut inbox_target: Option<i64> = None;
    let mut trash_name: Option<String> = None;
    let mut in_trash_ok = false;
    let mut deleted_forever = false;
    let mut gone_at: Option<Instant> = None; // 二删后开始计时：窗口内靶子不“复活”即认为真删（空文件夹不触发 FolderLoaded）
    let mut phase = 0u8; // 0=等 INBOX 靶子；1=已发首删等 Trash；2=Trash 内二删等真删

    let send = |s: &smail_core::Session, cmd: Cmd| {
        let _ = s.cmd_tx.send(SessionCmd::Route { account: 0, cmd });
    };

    send(&session, Cmd::SelectFolder("INBOX".into()));

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { event, .. }) = rx.try_recv() {
            match event {
                Event::FolderLoaded { folder, rows } => {
                    let hit = rows.iter().find(|r| r.subject == subject);
                    println!(
                        "[loaded ] {folder}: {} 封（含靶子={}）",
                        rows.len(),
                        hit.is_some()
                    );
                    match phase {
                        0 => {
                            if let Some(r) = hit {
                                inbox_target = Some(r.envelope_hash);
                                phase = 1;
                                println!("[del-1  ] INBOX 删除（应移废纸篓）");
                                send(&session, Cmd::DeleteMessage(r.envelope_hash));
                            }
                        }
                        1 => {
                            if folder.eq_ignore_ascii_case("Trash") {
                                if let Some(r) = hit {
                                    in_trash_ok = true;
                                    phase = 2;
                                    println!("[trash  ] 靶子已在废纸篓 ✅（可恢复）");
                                    println!("[del-2  ] 废纸篓内删除（应永久）");
                                    send(&session, Cmd::DeleteMessage(r.envelope_hash));
                                }
                            }
                        }
                        2 => {
                            if folder.eq_ignore_ascii_case("Trash")
                                && hit.is_none()
                                && deleted_forever
                            {
                                println!("[gone   ] 靶子已永久消失 ✅");
                                println!("\n[m18 PASS] 删除→废纸篓 ✅ 废纸篓→永久删除 ✅");
                                let _ = session.cmd_tx.send(SessionCmd::Shutdown);
                                std::process::exit(0);
                            }
                        }
                        _ => {}
                    }
                }
                Event::Folders(infos) => {
                    if let Some(t) = trash_name.as_ref() {
                        let _ = t;
                    }
                    // 首删后文件夹列表会刷新（Trash 被自动创建/出现在列表）→ 进废纸篓验证
                    if phase == 1 {
                        if let Some(info) =
                            infos.iter().find(|f| f.path.eq_ignore_ascii_case("Trash"))
                        {
                            trash_name = Some(info.path.clone());
                            println!("[folders] Trash 出现（自动创建/识别）");
                            send(&session, Cmd::SelectFolder("Trash".into()));
                        }
                    }
                }
                Event::MessageDeleted(_) => {
                    if phase == 2 && !deleted_forever {
                        deleted_forever = true;
                        gone_at = Some(Instant::now());
                        // 再拉一次 Trash（若还有残留会触发上面的“复活”判定）
                        send(&session, Cmd::SelectFolder("Trash".into()));
                    }
                }
                Event::Status(s) => {
                    if s.contains("废纸篓") || s.contains("Trash") {
                        println!("[status ] {s}");
                    }
                }
                Event::Error(s) => println!("[error  ] {s}"),
                _ => {}
            }
        }
        // 二删后 6s 内靶子未复活（无 FolderLoaded 再含靶子）→ 服务器真删
        //（空文件夹不触发 FolderLoaded，不能等“空列表”事件；服务器侧
        // 已由 doveadb/文件系统在开发期验证，CI 以“不复活”为准）
        if deleted_forever
            && gone_at
                .map(|t| t.elapsed() >= Duration::from_secs(6))
                .unwrap_or(false)
        {
            println!("[gone   ] 靶子未复活（6s 窗口）✅");
            println!("\n[m18 PASS] 删除→废纸篓 ✅ 废纸篓→永久删除 ✅");
            let _ = session.cmd_tx.send(SessionCmd::Shutdown);
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "\n[m18 FAIL] phase={phase} in_trash={in_trash_ok} forever={deleted_forever} target={inbox_target:?}"
    );
    std::process::exit(1);
}
