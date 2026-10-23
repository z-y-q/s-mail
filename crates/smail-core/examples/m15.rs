//! m15：草稿云同步端到端（B2）。
//!
//!   sh spikes/spike-melib/imap-test/up.sh
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m15 \
//!     cargo run -p smail-core --example m15 -- 60
//!
//! 验证：保存草稿→服务器 Drafts 出现 1 封；编辑→仍 1 封（换版不叠加）；
//! 删除→服务器 0 封。服务器侧断言用独立 IMAP 连接（不经被测服务线程）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_backend::{connect, ImapConfig, MailBackend};
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

/// 清场：删掉服务器 Drafts 里历史遗留的 s-mail 草稿（断言要求精确计数）
fn cleanup_server_drafts() {
    let (b, _) = connect(&imap_cfg()).expect("独立 IMAP 连接");
    let envs = b.fetch_envelopes("Drafts").expect("fetch Drafts");
    let mine: Vec<u64> = envs
        .iter()
        .filter(|e| e.message_id.contains("s-mail-draft-"))
        .map(|e| e.hash)
        .collect();
    if !mine.is_empty() {
        b.delete_messages("Drafts", &mine).expect("清理遗留草稿");
        println!("[m15] 已清理遗留草稿 {} 封", mine.len());
    }
}

/// 独立连接数服务器 Drafts 里 s-mail 草稿的封数
fn server_draft_count() -> usize {
    let (b, _) = connect(&imap_cfg()).expect("独立 IMAP 连接");
    b.fetch_envelopes("Drafts")
        .expect("fetch Drafts")
        .iter()
        .filter(|e| e.message_id.contains("s-mail-draft-"))
        .count()
}

fn wait_server_drafts(n: usize, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if server_draft_count() == n {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    server_draft_count() == n
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
        .unwrap_or_else(|| PathBuf::from("/tmp/smail-m15"));

    // 先清服务器遗留（前次运行残留会让精确计数断言误报）
    cleanup_server_drafts();

    let (mut service, mut rx) = match AccountService::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m15 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m15] 会话已启动");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut step = 0u8;
    let mut draft_id: Option<i64> = None;
    fn fail(msg: &str) -> ! {
        eprintln!("[m15 FAIL] {msg}");
        std::process::exit(1);
    }

    while Instant::now() < deadline {
        let Some(ev) = rx.try_recv().ok() else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        match ev {
            Event::Status(s) => println!("[status ] {s}"),
            Event::Error(s) => println!("[error  ] {s}"),
            Event::DraftSaved { id } => {
                println!("[draft  ] 已保存 #{id}");
                draft_id = Some(id);
            }
            Event::Folders(f) => {
                // 首次文件夹广播 = 连接就绪，开始链路
                if step == 0 && f.iter().any(|x| x.path == "Drafts") {
                    step = 1;
                    println!("[m15] Drafts 就绪，保存草稿 v1");
                    let _ = service.cmd_tx.send(Cmd::SaveDraft {
                        id: None,
                        to: "peer@local".into(),
                        cc: String::new(),
                        bcc: String::new(),
                        subject: "m15 草稿验证 v1".into(),
                        body: "v1 内容".into(),
                    });
                }
            }
            Event::DraftsListed(list) if step == 1 => {
                // DraftSaved 已到（list 非空即保存成功）；等服务上传完成
                if list.is_empty() || draft_id.is_none() {
                    continue;
                }
                if !wait_server_drafts(1, 15) {
                    fail("保存后服务器 Drafts 应恰有 1 封");
                }
                println!("[check  ] 服务器 Drafts = 1 ✅");
                step = 2;
                println!("[m15] 编辑为 v2（换版应替换不叠加）");
                let _ = service.cmd_tx.send(Cmd::SaveDraft {
                    id: draft_id,
                    to: "peer@local".into(),
                    cc: String::new(),
                    bcc: String::new(),
                    subject: "m15 草稿验证 v2".into(),
                    body: "v2 内容".into(),
                });
            }
            Event::DraftsListed(_) if step == 2 => {
                if !wait_server_drafts(1, 15) {
                    fail("编辑后服务器 Drafts 应仍为 1 封（旧删新上）");
                }
                println!("[check  ] 换版后服务器 Drafts = 1 ✅");
                step = 3;
                println!("[m15] 删除草稿");
                let _ = service.cmd_tx.send(Cmd::DeleteDraft(draft_id.unwrap()));
            }
            Event::DraftDeleted(_) if step == 3 => {
                if !wait_server_drafts(0, 15) {
                    fail("删除后服务器 Drafts 应为 0 封");
                }
                println!("[check  ] 删除后服务器 Drafts = 0 ✅");
                println!("\n[m15 PASS] 上传 ✅ 换版替换 ✅ 删除清理 ✅");
                service.stop();
                return;
            }
            _ => {}
        }
    }
    fail("超时未完成全部链路");
}
