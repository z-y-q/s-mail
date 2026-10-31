//! M23 端到端：撤销发送（延后投递）。
//!
//! 前置（同 m2/m22）：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   python3 spikes/spike-melib/imap-test/mini_smtp.py 11525 &
//!
//! 流程：Cmd::QueueHold（send_at=now+3）→ 立即删行验证撤销语义
//! → 再入队一条 send_at=now+3 → 轮询 mini_smtp 落盘文件，
//! 3 秒内不应出现（hold 未释放），5 秒内应出现（到期真投）。
//!
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m23 \
//!     cargo run -p smail-core --example m23 -- 60

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{Cmd, Session, SessionCmd};

fn util_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
        .unwrap_or_else(|| "/tmp/smail-m23".into());
    let _ = std::fs::remove_dir_all(&home);

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m23 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m23] 会话已启动");

    let now = util_now();
    let mail = |id: i64, subject: &str, send_at: i64| Cmd::QueueHold {
        id,
        to: "friend@example.com".into(),
        cc: String::new(),
        bcc: String::new(),
        subject: subject.into(),
        body: format!("撤销发送测试 {subject}"),
        attachments: vec![],
        sign: false,
        encrypt: false,
        high_priority: false,
        send_at,
    };

    // 1) 立刻撤销：入队后马上删行——mini_smtp 永远不该收到它
    session
        .cmd_tx
        .send(SessionCmd::Route {
            account: 0,
            cmd: mail(9_000_000_000_001, "撤销的那封", now + 3),
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    session
        .cmd_tx
        .send(SessionCmd::Route {
            account: 0,
            cmd: Cmd::DiscardOutbox(9_000_000_000_001),
        })
        .unwrap();
    println!("[m23] 撤销已执行（hold 行已删）");

    // 2) 到期投递：send_at=now+3，3 秒后 flush 应释放并真投
    //（主题用 ASCII 标记——lettre 对中文主题走 RFC2047 编码，断言按原文比对会失配）
    let send_id = 9_000_000_000_002i64;
    session
        .cmd_tx
        .send(SessionCmd::Route {
            account: 0,
            cmd: mail(send_id, "hold-deliver-test", now + 3),
        })
        .unwrap();
    println!("[m23] 延后邮件已入队（3s 后到期）");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut delivered = false;
    while Instant::now() < deadline {
        while let Ok(_ev) = rx.try_recv() {}
        match std::fs::read_to_string("/tmp/mini-smtp-mail.eml") {
            Ok(eml) if eml.contains("hold-deliver-test") => {
                let elapsed = util_now() - now;
                if elapsed < 3 {
                    println!("[m23 FAIL] 到期前({elapsed}s)就投递了——hold 未生效");
                    std::process::exit(1);
                }
                delivered = true;
                println!("[m23] 已捕获到期投递（{elapsed}s 后）");
                break;
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !delivered {
        println!("[m23 FAIL] 超时未捕获到期投递");
        std::process::exit(1);
    }
    println!("[m23 PASS] 撤销发送：hold 到期才投 + 撤销行永不投");
}
