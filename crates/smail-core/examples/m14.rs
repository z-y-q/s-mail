//! M14 端到端：M7 新功能——手动移动 / 批量标记 / 文件夹管理 / 联系人。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m14 \
//!     cargo run -p smail-core --example m14 -- 60
//!
//! 流程（全走 Cmd→服务线程→dovecot 真实链路）：
//!   1. INBOX 同步 → 取两封靶子
//!   2. CreateFolder("m7box") → Folders 出现
//!   3. MoveMessage(靶子1 → m7box) → Status 确认 + INBOX 重同步无靶子1
//!   4. BatchFlags([靶子2] seen=true) → FlagsChanged 到达
//!   5. ListContacts → ContactsListed 非空（INBOX 有历史发件人）
//!   6. DeleteFolder("m7box") → Folders 消失（服务器与本地一致）
//!   7. 收尾断言靶子1从本地 store 消失（move 清理生效）

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
        .unwrap_or_else(|| "/tmp/smail-m14".into());
    let _ = std::fs::remove_dir_all(&home);

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m14 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m14] 会话已启动");

    let boxname = "m7box".to_string();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut moved_hash: Option<i64> = None;
    let mut flag_hash: Option<i64> = None;
    let mut inbox_ready = false;

    // 各步结果旗
    let mut folder_created = false;
    let mut move_status = false;
    let mut moved_gone_from_inbox = false;
    let mut flags_changed = false;
    let mut contacts_listed = false;
    let mut folder_deleted = false;

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::Folders(f) => {
                    let names: Vec<&str> = f.iter().map(|x| x.path.as_str()).collect();
                    if !inbox_ready {
                        let _ = session.cmd_tx.send(SessionCmd::Route {
                            account,
                            cmd: Cmd::SelectFolder("INBOX".into()),
                        });
                    }
                    if names.contains(&boxname.as_str()) {
                        if !folder_created {
                            folder_created = true;
                            println!("[m14] 文件夹已创建：{boxname}");
                            // 建好后移动靶子
                            if let Some(h) = moved_hash {
                                let _ = session.cmd_tx.send(SessionCmd::Route {
                                    account,
                                    cmd: Cmd::MoveMessage {
                                        hash: h,
                                        dest: boxname.clone(),
                                    },
                                });
                            }
                        }
                    } else if folder_created && !folder_deleted && move_status {
                        folder_deleted = true;
                        println!("[m14] 文件夹已删除：{boxname}");
                    }
                }
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") && !rows.is_empty() {
                        if moved_hash.is_none() {
                            moved_hash = Some(rows[0].envelope_hash);
                            flag_hash = Some(rows[1].envelope_hash);
                            inbox_ready = true;
                            println!(
                                "[m14] INBOX {} 封；靶子 move={} flag={}",
                                rows.len(),
                                rows[0].subject,
                                rows[1].subject
                            );
                            let _ = session.cmd_tx.send(SessionCmd::Route {
                                account,
                                cmd: Cmd::CreateFolder(boxname.clone()),
                            });
                        } else if move_status && !moved_gone_from_inbox {
                            // 移动后的 INBOX 重同步：靶子应已不在
                            let gone = rows.iter().all(|r| Some(r.envelope_hash) != moved_hash);
                            if gone {
                                moved_gone_from_inbox = true;
                                println!("[m14] 移动后 INBOX 无靶子（本地清理 ✅）");
                                // 批量标记 + 联系人
                                let _ = session.cmd_tx.send(SessionCmd::Route {
                                    account,
                                    cmd: Cmd::BatchFlags {
                                        hashes: vec![flag_hash.unwrap()],
                                        seen: Some(true),
                                        flagged: Some(true),
                                    },
                                });
                                let _ = session.cmd_tx.send(SessionCmd::Route {
                                    account,
                                    cmd: Cmd::ListContacts,
                                });
                            }
                        }
                    }
                }
                Event::Status(s) => {
                    if s.contains("已移动") {
                        move_status = true;
                        println!("[m14] {s}");
                        // 触发 INBOX 重同步验证本地清理
                        let _ = session.cmd_tx.send(SessionCmd::Route {
                            account,
                            cmd: Cmd::SelectFolder("INBOX".into()),
                        });
                    }
                    if s.contains("已更新") {
                        println!("[m14] {s}");
                    }
                }
                Event::FlagsChanged {
                    hash,
                    seen,
                    flagged,
                } => {
                    if Some(hash) == flag_hash && seen == Some(true) && flagged == Some(true) {
                        flags_changed = true;
                        println!("[m14] 批量标记生效（seen+flagged）");
                        // 标记验证完 → 删文件夹收尾
                        let _ = session.cmd_tx.send(SessionCmd::Route {
                            account,
                            cmd: Cmd::DeleteFolder(boxname.clone()),
                        });
                    }
                }
                Event::ContactsListed(v) => {
                    if !v.is_empty() {
                        contacts_listed = true;
                        println!("[m14] 联系人 {} 个（首：{}）", v.len(), v[0]);
                    }
                }
                Event::Error(s) => println!("[error ] {s}"),
                _ => {}
            }
        }

        if folder_created
            && move_status
            && moved_gone_from_inbox
            && flags_changed
            && contacts_listed
            && folder_deleted
        {
            let _ = session.cmd_tx.send(SessionCmd::Shutdown);
            println!(
                "\n[m14 PASS] 建文件夹 ✅ 手动移动+本地清理 ✅ 批量标记 ✅ 联系人 ✅ 删文件夹 ✅"
            );
            std::thread::sleep(Duration::from_millis(300));
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);
    eprintln!(
        "\n[m14 FAIL] 建={folder_created} 移动={move_status} 清理={moved_gone_from_inbox} \
         批量={flags_changed} 联系人={contacts_listed} 删={folder_deleted}"
    );
    std::process::exit(1);
}
