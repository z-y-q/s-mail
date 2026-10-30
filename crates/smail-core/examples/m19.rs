//! M19 端到端：清空废纸篓真实链路（用户报障复现：清空后同步又回来）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m19 \
//!     cargo run -p smail-core --example m19 -- 90
//!
//! 流程（全走 Cmd→服务线程→dovecot 真实链路）：
//!   1. INBOX 同步 → 取一封靶子
//!   2. DeleteMessage(靶子)（普通文件夹 → 移废纸篓）
//!   3. 废纸篓文件夹出现 → SelectFolder(废纸篓) → FolderLoaded 有靶子
//!   4. EmptyTrash(废纸篓) → batch_delete → 服务器 EXPUNGE
//!   5. 再次 SelectFolder(废纸篓) 强制重同步 → FolderLoaded 必须 0 封
//!   （若服务器上没删掉，重同步会把邮件拉回来——这正是用户报障现象）
//!
//! 新账号（干净 SMAIL_HOME）+ 新 dovecot 容器里，UidStore 无缓存，
//! 该用例验证「同连接 FETCH 后删除」路径；用户报障是「重连/缓存回放后
//! hash→UID 映射缺失」路径——两条路径都必须删得掉才算修复。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

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
        .unwrap_or_else(|| "/tmp/smail-m19".into());
    let _ = std::fs::remove_dir_all(&home);

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m19 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m19] 会话已启动");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut target_mid = String::new();
    let mut trash_folder: Option<String> = None;
    let mut moved_to_trash = false;
    let mut empty_sent = false;
    let mut trash_empty_after_sync = false;
    let mut synced_trash_after_empty = false;
    // 探测短路观测（M19_PROBE_CHECK=1 且配 TTL=2 的 config.probe.toml）：
    // 清空验证后等 TTL 过期再切文件夹，应收到「无新变化（跳过整包拉取）」
    let probe_check = std::env::var("M19_PROBE_CHECK").is_ok();
    let mut shortcircuit_seen = false;
    let mut probe_wait_sent = false;

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::Folders(folders) => {
                    if trash_folder.is_none() {
                        trash_folder = folders
                            .iter()
                            .find(|f| f.kind == smail_backend::FolderKind::Trash)
                            .map(|f| f.path.clone());
                        if let Some(t) = trash_folder.clone() {
                            println!("[m19] 废纸篓文件夹：{t}");
                            if moved_to_trash {
                                let _ = session.cmd_tx.send(SessionCmd::Route {
                                    account,
                                    cmd: Cmd::SelectFolder(t),
                                });
                            }
                        }
                    }
                }
                Event::FolderLoaded { folder, rows } => {
                    let tf = trash_folder.clone().unwrap_or_default();
                    if folder.eq_ignore_ascii_case("INBOX")
                        && !rows.is_empty()
                        && target_mid.is_empty()
                    {
                        target_mid = rows[0].message_id.clone();
                        println!("[m19] INBOX {} 封；靶子：{}", rows.len(), rows[0].subject);
                        let _ = session.cmd_tx.send(SessionCmd::Route {
                            account,
                            cmd: Cmd::DeleteMessage(rows[0].envelope_hash),
                        });
                    }
                    if folder == tf && !tf.is_empty() {
                        if !moved_to_trash {
                            // 移入废纸篓后的首次同步：应看到靶子
                            //（melib 的 EnvelopeHash=hash(UID+路径)，MOVE 后副本 hash
                            // 必变——跨文件夹识别同一封邮件只能用 Message-ID）
                            if rows.iter().any(|r| r.message_id == target_mid) {
                                moved_to_trash = true;
                                println!("[m19] 靶子已进废纸篓（{} 封）→ 清空", rows.len());
                                let _ = session.cmd_tx.send(SessionCmd::Route {
                                    account,
                                    cmd: Cmd::EmptyTrash(tf.clone()),
                                });
                            }
                        } else if empty_sent && !synced_trash_after_empty {
                            synced_trash_after_empty = true;
                            let 残留 = rows.iter().any(|r| r.message_id == target_mid);
                            trash_empty_after_sync = rows.is_empty();
                            println!(
                                "[m19] 清空后重同步：废纸篓 {} 封，靶子{} → {}",
                                rows.len(),
                                if 残留 { "还在" } else { "已不在" },
                                if trash_empty_after_sync {
                                    "✅ 已删净"
                                } else {
                                    "❌ 邮件回来了"
                                }
                            );
                            for r in &rows {
                                println!("      残留：{}", r.subject);
                            }
                        }
                    }
                }
                Event::Status(s) => {
                    println!("[m19] {s}");
                    // 探测短路命中（TTL 过期后的重拉被 SELECT 探测拦下）
                    if s.contains("无新变化") {
                        shortcircuit_seen = true;
                        println!("[m19] ✅ 探测短路生效（整包 FETCH 已跳过）");
                    }
                    // 移动完成后（废纸篓文件夹已知）→ 选中废纸篓拉列表
                    if s.contains("已移动") && !moved_to_trash {
                        if let Some(t) = trash_folder.clone() {
                            let _ = session.cmd_tx.send(SessionCmd::Route {
                                account,
                                cmd: Cmd::SelectFolder(t),
                            });
                        }
                    }
                    // EmptyTrash 完成状态 → 强制重同步废纸篓验证
                    if (s.contains("已清空废纸篓") || s.contains("已删除") || s.contains("Deleted"))
                        && !empty_sent
                    {
                        empty_sent = true;
                        if let Some(t) = trash_folder.clone() {
                            // 清 TTL 由服务端 EmptyTrash 分支处理；这里再显式刷一次
                            let _ = session.cmd_tx.send(SessionCmd::Route {
                                account,
                                cmd: Cmd::ForceRefresh(t.clone()),
                            });
                            let _ = session.cmd_tx.send(SessionCmd::Route {
                                account,
                                cmd: Cmd::SelectFolder(t),
                            });
                        }
                    }
                }
                Event::Error(s) => println!("[m19][error ] {s}"),
                _ => {}
            }
        }

        // 探测短路段：TTL 过期后再切一次废纸篓，应走短路而非全量
        if trash_empty_after_sync && probe_check && !shortcircuit_seen && !probe_wait_sent {
            probe_wait_sent = true;
            println!("[m19] 等 TTL（2s）过期后触发探测短路观测…");
            std::thread::sleep(Duration::from_secs(3));
            if let Some(t) = trash_folder.clone() {
                let _ = session.cmd_tx.send(SessionCmd::Route {
                    account: 0,
                    cmd: Cmd::SelectFolder(t),
                });
            }
        }

        if trash_empty_after_sync && (!probe_check || shortcircuit_seen) {
            let _ = session.cmd_tx.send(SessionCmd::Shutdown);
            if probe_check {
                println!(
                    "\n[m19 PASS] 移入废纸篓 ✅ 清空 ✅ 重同步后 0 封 ✅ 探测短路 ✅（全链路省流量验证）"
                );
            } else {
                println!(
                    "\n[m19 PASS] 移入废纸篓 ✅ 清空 ✅ 重同步后废纸篓 0 封 ✅（服务器 EXPUNGE 真实生效）"
                );
            }
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);
    eprintln!(
        "\n[m19 FAIL] 靶子={:?} 废纸篓={:?} 移入={moved_to_trash} \
         清空命令={empty_sent} 清空后空={trash_empty_after_sync}",
        !target_mid.is_empty(),
        trash_folder.is_some(),
    );
    std::process::exit(1);
}
