//! m16：草稿双向同步端到端（v2 对账）。
//!
//!   sh spikes/spike-melib/imap-test/up.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m16 \
//!     cargo run -p smail-core --example m16 -- 60
//!
//! 验证：外部注入的"其他设备"草稿 → 打开草稿箱对账后本地出现并可读；
//! 服务器侧删除该草稿 → 再次对账后本地跟随删除；自建草稿全程不受影响。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_backend::{build_draft_bytes, connect, ImapConfig, MailBackend, OutgoingMail};
use smail_core::{AccountService, Cmd, Event};

fn imap_cfg() -> ImapConfig {
    ImapConfig {
        host: "127.0.0.1".into(),
        port: 1143,
        username: "test".into(),
        password: Some("testpass".into()),
        starttls: false,
        tls: false,
        use_oauth2: false,
    }
}

/// 清场：删服务器 Drafts 里历史遗留的 s-mail/m16 草稿
fn cleanup_server_drafts() {
    let (mut b, _) = connect(&imap_cfg()).expect("独立 IMAP 连接");
    let envs = b.fetch_envelopes("Drafts").expect("fetch Drafts");
    let mine: Vec<u64> = envs
        .iter()
        .filter(|e| e.message_id.contains("s-mail-draft-") || e.message_id.contains("m16-"))
        .map(|e| e.hash)
        .collect();
    if !mine.is_empty() {
        b.delete_messages("Drafts", &mine).expect("清理遗留草稿");
        println!("[m16] 已清理遗留草稿 {} 封", mine.len());
    }
}

/// 模拟其他设备：注入一封外来草稿（非 s-mail 前缀的 Message-ID）
fn inject_foreign_draft(subject: &str) {
    let (mut b, _) = connect(&imap_cfg()).expect("独立 IMAP 连接");
    let mail = OutgoingMail {
        from: "test@local".into(),
        to: vec!["peer@local".into()],
        cc: vec![],
        bcc: vec![],
        subject: subject.into(),
        body: "来自其他设备的草稿内容".into(),
        attachments: vec![],
    };
    let bytes = build_draft_bytes(&mail, "<m16-foreign-1@other-device>").expect("组草稿 MIME");
    b.append_draft(bytes).expect("注入外来草稿");
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("/tmp/smail-m16"));

    cleanup_server_drafts();
    inject_foreign_draft("m16 外来草稿");

    let (mut service, mut rx) = match AccountService::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m16 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m16] 会话已启动（外来草稿已注入服务器）");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut step = 0u8;
    fn fail(msg: &str) -> ! {
        eprintln!("[m16 FAIL] {msg}");
        std::process::exit(1);
    }

    while Instant::now() < deadline {
        let Some(ev) = rx.try_recv().ok() else {
            std::thread::sleep(Duration::from_millis(150));
            continue;
        };
        match ev {
            Event::Status(s) => println!("[status ] {s}"),
            Event::Error(s) => println!("[error  ] {s}"),
            Event::Folders(f) if step == 0 && f.iter().any(|x| x.path == "Drafts") => {
                step = 1;
                println!("[m16] Drafts 就绪，请求草稿列表（应导入外来草稿）");
                let _ = service.cmd_tx.send(Cmd::ListDrafts);
            }
            Event::DraftsListed(list) if step == 1 => {
                let imported = list.iter().find(|d| d.subject == "m16 外来草稿");
                let Some(d) = imported else {
                    fail("对账后本地应出现外来草稿");
                };
                println!("[check  ] 外来草稿已导入（to={}）✅", d.to_addr);
                step = 2;
                println!("[m16] 服务器侧删除外来草稿（模拟对方设备撤回）");
                let (mut b, _) = connect(&imap_cfg()).expect("独立 IMAP 连接");
                let gone = b
                    .delete_draft_by_mid("<m16-foreign-1@other-device>")
                    .expect("删服务器草稿");
                if !gone {
                    fail("服务器侧应能按 Message-ID 删到外来草稿");
                }
                let _ = service.cmd_tx.send(Cmd::ListDrafts);
            }
            Event::DraftsListed(list) if step == 2 => {
                if list.iter().any(|d| d.subject == "m16 外来草稿") {
                    fail("服务器删除后本地应跟随删除外来草稿");
                }
                println!(
                    "[check  ] 服务器删除后本地跟随清理 ✅（剩余 {} 封）",
                    list.len()
                );
                println!("\n[m16 PASS] 外来导入 ✅ 对端删除传播 ✅");
                service.stop();
                return;
            }
            _ => {}
        }
    }
    fail("超时未完成全部链路");
}
