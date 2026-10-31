//! M21 端到端：无 MOVE 能力服务器（QQ 类）的移动回退链路。
//!
//! 前置（不能用常规 up.sh——它起的 dovecot 会广告 MOVE）：
//!   sh spikes/spike-melib/imap-test/up-nomove.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m21 \
//!     cargo run -p smail-core --example m21 -- 60
//!
//! 验证目标：服务器不广告 MOVE 时，melib 回退 COPY + STORE \Deleted，
//! 副本留在源文件夹直到 EXPUNGE——后端 move_messages 必须在回退后
//! 主动清源，否则同一封邮件两处同现（源文件夹删不掉的幽灵）。
//!
//! 流程（全走 Cmd→服务线程→真实 IMAP）：
//!   1. INBOX 同步 → 取一封靶子 → CreateFolder("m21box")
//!   2. Folders 出现 m21box → MoveMessage(靶子 → m21box)
//!   3. Status 已移动 → SelectFolder("m21box") → FolderLoaded 含靶子（落地 ✅）
//!   4. SelectFolder("INBOX") 重同步 → 靶子不在（回退清源 ✅）
//!   5. DeleteFolder 收尾

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

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
        .unwrap_or_else(|| "/tmp/smail-m21".into());
    let _ = std::fs::remove_dir_all(&home);

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m21 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m21] 会话已启动（服务器应为无 MOVE 仿真）");

    let boxname = "m21box".to_string();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut moved_hash: Option<i64> = None;
    let mut moved_mid = String::new();
    let mut inbox_loaded = false;
    let mut folder_created = false;
    let mut move_sent = false;
    let mut move_status = false;
    let mut in_dest = false;
    let mut gone_from_inbox = false;
    let mut cleanup_sent = false;

    let send = |session: &Session, account: usize, cmd: Cmd| {
        let _ = session.cmd_tx.send(SessionCmd::Route { account, cmd });
    };

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::Folders(f) => {
                    let names: Vec<&str> = f.iter().map(|x| x.path.as_str()).collect();
                    if names.contains(&boxname.as_str()) && !folder_created {
                        folder_created = true;
                        println!("[m21] 文件夹已创建：{boxname}");
                        if let Some(h) = moved_hash {
                            move_sent = true;
                            send(
                                &session,
                                account,
                                Cmd::MoveMessage {
                                    hash: h,
                                    dest: boxname.clone(),
                                },
                            );
                        }
                    }
                }
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") && !rows.is_empty() {
                        if moved_hash.is_none() {
                            moved_hash = Some(rows[0].envelope_hash);
                            // COPY 副本在目标文件夹获得新 UID → melib envelope
                            // hash（folder+UID）必然不同——落地断言按 message_id
                            moved_mid = rows[0].message_id.clone();
                            inbox_loaded = true;
                            println!("[m21] INBOX {} 封；靶子：{}", rows.len(), rows[0].subject);
                            send(&session, account, Cmd::CreateFolder(boxname.clone()));
                        } else if move_status && in_dest && !gone_from_inbox {
                            // 移动后的 INBOX 重同步：回退清源断言（核心）。
                            // 修复前：COPY 副本无 EXPUNGE 挂在源文件夹，靶子仍在 → 失败。
                            // （move_messages_cmd 的乐观删行只删本地——INBOX 重同步
                            // 以服务器真值回灌，真没删干净会在这里现形）
                            let gone = rows.iter().all(|r| Some(r.envelope_hash) != moved_hash);
                            gone_from_inbox = gone;
                            println!(
                                "[m21] INBOX 重同步：靶子{}（{}）",
                                if gone { "已不在" } else { "仍在！！" },
                                if gone {
                                    "回退清源 ✅"
                                } else {
                                    "COPY 副本未清 ❌"
                                }
                            );
                            if gone && !cleanup_sent {
                                cleanup_sent = true;
                                send(&session, account, Cmd::DeleteFolder(boxname.clone()));
                            }
                        }
                    }
                    if folder == boxname && move_sent && move_status && !in_dest {
                        // SelectFolder(m21box) 的装载回包：靶子必须在目标
                        //（按 message_id——COPY 副本 hash 因新 UID 必然不同）
                        in_dest = rows.iter().any(|r| r.message_id == moved_mid);
                        println!(
                            "[m21] m21box {} 封；靶子落地：{}",
                            rows.len(),
                            if in_dest { "✅" } else { "❌ 未找到" }
                        );
                        if in_dest {
                            send(&session, account, Cmd::SelectFolder("INBOX".into()));
                        }
                    }
                }
                Event::Status(s) if s.contains("已移动") && !move_status => {
                    move_status = true;
                    println!("[m21] {s}");
                    send(&session, account, Cmd::SelectFolder(boxname.clone()));
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(120));
    }

    let pass = inbox_loaded && folder_created && move_status && in_dest && gone_from_inbox;
    if pass {
        println!("[m21 PASS] 无 MOVE 服务器：COPY 落地 ✅ 回退清源 ✅");
    } else {
        println!(
            "[m21 FAIL] inbox_loaded={inbox_loaded} folder_created={folder_created} \
             move_sent={move_sent} move_status={move_status} in_dest={in_dest} \
             gone_from_inbox={gone_from_inbox}"
        );
        std::process::exit(1);
    }
}
