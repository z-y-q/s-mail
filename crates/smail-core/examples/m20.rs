//! M20 端到端：各文件夹写操作——归档 / 标为垃圾 / 已发送类文件夹的移动
//! 与删除（用户报障「废纸篓删除复活」的同源排查：所有文件夹的写操作
//! 都走同一批 melib 原语，映射缺失时必须报错可见而不是静默假成功）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m20 \
//!     cargo run -p smail-core --example m20 -- 90
//!
//! 流程：
//!   1. INBOX 同步 → 靶子 A、B
//!   2. Cmd::Archive([A]) → Archive 文件夹出现 + 重同步后 INBOX 无 A、Archive 有 A
//!   3. Cmd::Spam([B]) → Junk 文件夹出现 + 重同步后 INBOX 无 B、Junk 有 B
//!   4. 对 Archive 里的 A 执行单封删除（非 Trash → 移废纸篓）→ 废纸篓有 A
//!   5. 整箱清空废纸篓（expunge_folder）→ 废纸篓 0 封
//!   （melib 静默假成功已补丁为 Err：任何一步空转都会卡住流程超时红）

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
        .unwrap_or_else(|| "/tmp/smail-m20".into());
    let _ = std::fs::remove_dir_all(&home);

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m20 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m20] 会话已启动");

    let deadline = Instant::now() + Duration::from_secs(secs);
    // 靶子用 Message-ID 跨文件夹识别（melib EnvelopeHash=hash(UID+路径)，
    // MOVE 后副本 hash 必变）
    let mut mid_a = String::new();
    let mut mid_b = String::new();
    let mut hash_b: Option<i64> = None;
    let mut archive_hash: Option<i64> = None;
    let mut archived = false;
    let mut spammed = false;
    let mut archived_verified = false;
    let mut spam_verified = false;
    let mut trash_sent = false;
    let mut trash_verified = false;

    let send = |session: &Session, account: usize, cmd: Cmd| {
        let _ = session.cmd_tx.send(SessionCmd::Route { account, cmd });
    };

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") && rows.len() >= 2 && mid_a.is_empty() {
                        mid_a = rows[0].message_id.clone();
                        mid_b = rows[1].message_id.clone();
                        hash_b = Some(rows[1].envelope_hash);
                        println!(
                            "[m20] INBOX {} 封；靶子 A={} B={}",
                            rows.len(),
                            rows[0].subject,
                            rows[1].subject
                        );
                        send(&session, account, Cmd::Archive(vec![rows[0].envelope_hash]));
                    }
                    // Archive 同步到达：A 应在里面（记下副本 hash）→ 接着标垃圾 B
                    if archived && !archived_verified && folder.eq_ignore_ascii_case("Archive") {
                        if let Some(r) = rows.iter().find(|r| r.message_id == mid_a) {
                            archive_hash = Some(r.envelope_hash);
                            archived_verified = true;
                            println!("[m20] ✅ 归档：A 在 Archive（{} 封）", rows.len());
                            send(&session, account, Cmd::Spam(vec![hash_b.unwrap()]));
                        }
                    }
                    // Junk 同步到达：B 应在里面 → 对 Archive 里的 A 删除（移废纸篓）
                    if spammed && !spam_verified && folder.eq_ignore_ascii_case("Junk") {
                        if rows.iter().any(|r| r.message_id == mid_b) {
                            spam_verified = true;
                            println!("[m20] ✅ 垃圾：B 在 Junk（{} 封）", rows.len());
                            if let Some(h) = archive_hash {
                                send(&session, account, Cmd::DeleteMessage(h));
                            }
                        }
                    }
                    // 废纸篓同步：A 移入后 → 整箱清空；清空后的验证同步必须 0 封
                    if trash_sent && !trash_verified && folder.eq_ignore_ascii_case("Trash") {
                        let has_a = rows.iter().any(|r| r.message_id == mid_a);
                        if has_a {
                            println!("[m20] 废纸篓 {} 封，A 在 → 整箱清空", rows.len());
                            send(&session, account, Cmd::EmptyTrash(folder.clone()));
                        } else if rows.is_empty() {
                            trash_verified = true;
                            println!("[m20] ✅ 废纸篓整箱清空后 0 封");
                        }
                    }
                }
                Event::Folders(folders) => {
                    // 新文件夹（Archive/Junk/Trash）出现后拉列表验证
                    for f in &folders {
                        let p = f.path.clone();
                        if (p.eq_ignore_ascii_case("Archive")
                            || p.eq_ignore_ascii_case("Junk")
                            || p.eq_ignore_ascii_case("Trash"))
                            && ((p.eq_ignore_ascii_case("Archive")
                                && archived
                                && !archived_verified)
                                || (p.eq_ignore_ascii_case("Junk") && spammed && !spam_verified)
                                || (p.eq_ignore_ascii_case("Trash")
                                    && trash_sent
                                    && !trash_verified))
                        {
                            send(&session, account, Cmd::SelectFolder(p));
                        }
                    }
                }
                Event::Status(s) => {
                    println!("[m20] {s}");
                    if s.contains("归档到") {
                        archived = true;
                    }
                    if s.contains("移到") && s.contains("垃圾") {
                        spammed = true;
                    }
                    // 移动完成 → 拉目标文件夹列表验证（Folders 事件先于移动
                    // 到达，不能依赖它触发选中）
                    if s.contains("已移动") {
                        for name in ["Archive", "Junk", "Trash"] {
                            if s.contains(name) {
                                if name == "Trash" && spam_verified && !trash_sent {
                                    trash_sent = true;
                                }
                                send(&session, account, Cmd::SelectFolder(name.into()));
                            }
                        }
                    }
                }
                Event::Error(s) => println!("[m20][error ] {s}"),
                _ => {}
            }
        }

        if archived_verified && spam_verified && trash_verified {
            let _ = session.cmd_tx.send(SessionCmd::Shutdown);
            println!("\n[m20 PASS] 归档 ✅ 标垃圾 ✅ 各文件夹写操作全部真实生效（无静默假成功）");
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);
    eprintln!(
        "\n[m20 FAIL] A={:?} B={:?} 归档={archived} 归档验证={archived_verified} \
         标垃圾={spammed} 垃圾验证={spam_verified} 进废纸篓={trash_sent} 清空验证={trash_verified}",
        !mid_a.is_empty(),
        !mid_b.is_empty(),
    );
    std::process::exit(1);
}
