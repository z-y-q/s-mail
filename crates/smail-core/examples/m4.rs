//! M4 端到端：HTML 邮件 → 消毒 → markdown 提取。
//!
//! 前置：先注入一封 multipart/alternative 的 HTML 测试邮件再启动本例：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   python3 spikes/spike-melib/imap-test/inject_html_mail.py   # 容器内写 Maildir 触发 IDLE
//!   SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME=/tmp/smail-m4 \
//!     cargo run -p smail-core --example m4 -- 45

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{AccountService, Cmd, Event};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(45);

    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m4".into());

    let subject = "M4-HTML渲染测试";
    let (mut service, mut rx) = match AccountService::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m4 启动失败] {e}");
            std::process::exit(1);
        }
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut target_hash: Option<i64> = None;
    let mut opened = false;

    while Instant::now() < deadline {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Status(s) => println!("[status] {s}"),
                Event::Error(s) => println!("[error ] {s}"),
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") {
                        if let Some(r) = rows.iter().find(|r| r.subject == subject) {
                            if target_hash.is_none() {
                                target_hash = Some(r.envelope_hash);
                                println!("[found  ] INBOX 里有目标邮件，请求打开");
                                let _ = service.cmd_tx.send(Cmd::OpenMessage(r.envelope_hash));
                            }
                        }
                    }
                }
                Event::BodyLoaded {
                    hash,
                    body,
                    is_html,
                    attachments,
                    ..
                } => {
                    if Some(hash) == target_hash {
                        opened = true;
                        println!("[opened ] is_html={is_html} 附件 {} 个", attachments.len());
                        println!("--- 提取的 markdown ---");
                        for line in body.lines().take(20) {
                            println!("  {line}");
                        }
                        println!("--- 断言 ---");
                        let ok_heading = body.contains("## 订单通知");
                        let ok_bold = body.contains("**SO-M4**");
                        let ok_link = body.contains("[物流跟踪](https://example.com/m4)");
                        let ok_list = body.contains("- 顺丰速运");
                        let ok_no_script = !body.contains("alert");
                        let ok_no_img = !body.contains("pixel.gif");
                        println!(
                            "标题{ok_heading} 加粗{ok_bold} 链接{ok_link} 列表{ok_list} 无脚本{ok_no_script} 无图片{ok_no_img}"
                        );
                        if is_html
                            && ok_heading
                            && ok_bold
                            && ok_link
                            && ok_list
                            && ok_no_script
                            && ok_no_img
                        {
                            println!("\n[m4 PASS] HTML 管线全绿");
                            service.stop();
                            std::process::exit(0);
                        }
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("\n[m4 FAIL] opened={opened}");
    service.stop();
    std::process::exit(1);
}
