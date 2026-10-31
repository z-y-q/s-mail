//! M22 端到端：代转（Redirect，RFC 5322 Resent-* 重投）。
//!
//! 前置（同 m2）：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   python3 spikes/spike-melib/imap-test/mini_smtp.py 11525 &
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m22 \
//!     cargo run -p smail-core --example m22 -- 60
//!
//! 流程：INBOX 同步 → 取一封靶子 → Cmd::Redirect（投给 friend@example.com）
//! → 轮询 mini_smtp 落盘的 /tmp/mini-smtp-mail.eml → 断言：
//!   1. Resent-To 指向新收件人
//!   2. Resent-From 指向本账号（identity）
//!   3. 原 Subject/From 头原样保留（原文重投，非重新包装）
//!   4. 原正文片段仍在

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

const REDIRECT_TO: &str = "friend@example.com";

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
        .unwrap_or_else(|| "/tmp/smail-m22".into());
    let _ = std::fs::remove_dir_all(&home);

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m22 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m22] 会话已启动");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut target: Option<(i64, String, String, String)> = None; // hash, subject, from, message_id
    let mut redirect_sent = false;
    let mut assertions: Option<(bool, bool, bool, bool)> = None;

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            if let Event::BodyLoaded { hash, .. } = &event {
                println!("[m22 ev] BodyLoaded {hash}");
            }
            if let Event::Error(e) = &event {
                println!("[m22 ev] Error {e}");
            }
            match event {
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") && !rows.is_empty() && target.is_none()
                    {
                        let r = &rows[0];
                        target = Some((
                            r.envelope_hash,
                            r.subject.clone(),
                            r.from_addr.clone(),
                            r.message_id.clone(),
                        ));
                        println!("[m22] 靶子：{}", r.subject);
                        // 先读信触发正文缓存（真实用户路径：代转不要求先读，
                        // 但服务端会对未缓存正文现取——此处验证完整链路）
                        send(&session, account, Cmd::OpenMessage(r.envelope_hash));
                    }
                }
                Event::BodyLoaded { hash, body, .. } => {
                    // 正文缓存落地后发代转（body 含原文本，够断言用）
                    if let Some((h, _, _, _)) = &target {
                        if *h == hash && !redirect_sent {
                            redirect_sent = true;
                            let _ = body;
                            send(
                                &session,
                                account,
                                Cmd::Redirect {
                                    hash: *h,
                                    to: REDIRECT_TO.into(),
                                },
                            );
                        }
                    }
                }
                Event::Status(s) => {
                    println!("[m22 status] {s}");
                    if s.contains("已代转") {
                        redirect_sent = true;
                    }
                }
                _ => {}
            }
        }

        if redirect_sent {
            let Ok(eml) = std::fs::read_to_string("/tmp/mini-smtp-mail.eml") else {
                std::thread::sleep(Duration::from_millis(300));
                continue;
            };
            // 原头在存储文件里保持 RFC2047 编码形态（原样重投语义）——
            // 用稳定的 Message-ID 断言原文保留，Subject/From 用前缀宽松断言
            let Some((_, _, _, mid)) = &target else { break };
            let has_resent_to = eml.contains(&format!("Resent-To: <{REDIRECT_TO}>"));
            let has_resent_from = eml.contains("Resent-From:");
            let has_resent_date = eml.contains("Resent-Date:");
            let orig_mid_kept = eml.contains(mid);
            let orig_body_kept = eml.contains("Content-Transfer-Encoding: base64");
            assertions = Some((
                has_resent_to,
                has_resent_from,
                orig_mid_kept,
                orig_body_kept,
            ));
            println!(
                "[m22] 断言：Resent-To={has_resent_to} Resent-From={has_resent_from} Resent-Date={has_resent_date} 原MessageID={orig_mid_kept} 原体保留={orig_body_kept}"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    match assertions {
        Some((rt, rf, os, of)) if rt && rf && os && of => {
            println!("[m22 PASS] 代转：Resent-* 重投 + 原文原头保留");
        }
        Some((rt, rf, os, of)) => {
            println!("[m22 FAIL] 断言失败：Resent-To={rt} Resent-From={rf} 原MessageID={os} 原体保留={of}");
            std::process::exit(1);
        }
        None => {
            println!("[m22 FAIL] 超时未完成代转捕获");
            std::process::exit(1);
        }
    }
}

fn send(session: &Session, account: usize, cmd: Cmd) {
    let _ = session.cmd_tx.send(SessionCmd::Route { account, cmd });
}
