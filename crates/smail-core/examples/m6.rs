//! M4-b 端到端：OAuth 设备流 + XOAUTH2 IMAP/SMTP 全链路（本地 mock）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh                          # dovecot(1143)
//!   python3 spikes/spike-melib/imap-test/mock_oauth.py 18888 /tmp/mock-oauth.json &
//!   python3 spikes/spike-melib/imap-test/mock_xoauth2_proxy.py 11143 1143 /tmp/xoauth2-sasl.log &
//!   python3 spikes/spike-melib/imap-test/mini_smtp.py 11525 &
//!   SMAIL_CONFIG=spikes/spike-melib/config.oauth.toml SMAIL_HOME=/tmp/smail-m6 \
//!     cargo run -p smail-core --example m6 -- 45
//!
//! 通过标准：设备流（2 次 pending 后授权）→ token=testpass → XOAUTH2 IMAP 登录
//! （代理日志含标准 SASL 串）→ INBOX 同步到种子邮件 → 主动刷新得新 token →
//! SMTP AUTH XOAUTH2（mini_smtp 日志含 Bearer）→ 退出码 0。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{oauth, Cmd, Event, Session, SessionCmd, TaggedEvent};

const SASL_LOG: &str = "/tmp/xoauth2-sasl.log";
const SMTP_AUTH_LOG: &str = "/tmp/mini-smtp-auth.log";
const OAUTH_STATE: &str = "/tmp/mock-oauth.json";

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(45);

    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("spikes/spike-melib/config.oauth.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m6".into());
    let _ = std::fs::remove_dir_all(&home);

    // ---- 1. 设备流（不开浏览器；mock 前 2 次 pending 后自动授权） ----
    let mut accounts = smail_core::load_accounts(&cfg).expect("配置解析失败");
    let acct = accounts.swap_remove(0);
    let oc = acct.oauth.as_ref().expect("配置需要 [accounts.oauth]");
    let dir = home.join(&acct.slug);
    let ts = oauth::authorize(oc, &dir, false).expect("设备流授权失败");
    assert_eq!(ts.access_token, "testpass", "mock 授权应发 testpass");
    assert!(ts.refresh_token.is_some(), "应带回 refresh_token");
    println!(
        "[m6] 设备流完成 access_token={}（已含 2 次 pending 轮询）",
        ts.access_token
    );

    // ---- 2. 未过期：ensure_token 不碰网络直接返回 ----
    let t = oauth::ensure_token(oc, &dir).expect("ensure_token");
    assert_eq!(t, "testpass");

    // ---- 3. Session：IMAP 走 XOAUTH2（经翻译代理登录 dovecot） ----
    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m6 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m6] Session 已启动（oauth 账号）");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut loaded_ok = false;
    let mut mail_sent = false;
    let mut sent_request = false;

    while Instant::now() < deadline {
        while let Ok(TaggedEvent { account, event }) = rx.try_recv() {
            match event {
                Event::Status(s) => println!("[status {account}] {s}"),
                Event::Error(s) => println!("[error  {account}] {s}"),
                Event::Folders(f) => {
                    println!(
                        "[folders {account}] {:?}",
                        f.iter().map(|x| x.path.as_str()).collect::<Vec<_>>()
                    );
                    let _ = session.cmd_tx.send(SessionCmd::Route {
                        account,
                        cmd: Cmd::SelectFolder("INBOX".into()),
                    });
                }
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") {
                        let ok = rows.iter().any(|r| r.subject.contains("项目周报"));
                        println!(
                            "[loaded {account}] INBOX: {} 封（含项目周报={ok}）",
                            rows.len()
                        );
                        loaded_ok = ok;
                    }
                }
                Event::MailSent { subject, .. } => {
                    println!("[sent {account}] {subject:?}");
                    mail_sent = true;
                }
                _ => {}
            }
        }

        if loaded_ok && !sent_request {
            sent_request = true;
            // ---- 4. 强制过期 → ensure_token 走 refresh（mock 换发 mock-access-2） ----
            let mut bad = oauth::load_tokens(&dir).unwrap();
            bad.expires_at = oauth::unix_now() - 1;
            oauth::save_tokens(&dir, &bad).unwrap();
            let t2 = oauth::ensure_token(oc, &dir).expect("刷新失败");
            assert_eq!(t2, "mock-access-2", "刷新后应拿到新 access token");
            println!("[m6] 刷新完成 access_token={t2}");

            // ---- 5. SMTP XOAUTH2（try_send 发信时取 token） ----
            let _ = session.cmd_tx.send(SessionCmd::Route {
                account: 0,
                cmd: Cmd::SendMail {
                    to: "dest@example.com".into(),
                    cc: String::new(),
                    bcc: String::new(),
                    subject: "m6 XOAUTH2 测试".into(),
                    body: "通过 mock XOAUTH2 发出".into(),
                    attachments: vec![],
                    sign: false,
                    encrypt: false,
                },
            });
        }

        if loaded_ok && mail_sent {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);

    // ---- 6. 线上证据断言 ----
    let sasl = std::fs::read(SASL_LOG).unwrap_or_default();
    let expect = b"user=test\x01auth=Bearer testpass\x01\x01\n";
    let sasl_ok = sasl.windows(expect.len()).any(|w| w == expect);
    println!(
        "[m6] IMAP SASL 串断言：{sasl_ok}（日志 {} 字节）",
        sasl.len()
    );

    let smtp_auth = std::fs::read(SMTP_AUTH_LOG).unwrap_or_default();
    let expect_smtp: &[u8] = b"auth=Bearer mock-access-2";
    let smtp_ok = smtp_auth
        .windows(expect_smtp.len())
        .any(|w| w == expect_smtp);
    println!(
        "[m6] SMTP XOAUTH2 断言：{smtp_ok}（日志 {} 字节）",
        smtp_auth.len()
    );

    let state = std::fs::read_to_string(OAUTH_STATE).unwrap_or_default();
    let refresh_calls = state.matches("\"grant_type\": \"refresh_token\"").count();
    // 2 次 pending + 1 次成功 = 3 次 device_code 请求
    let device_polls = state
        .matches("\"grant_type\": \"urn:ietf:params:oauth:grant-type:device_code\"")
        .count();
    println!("[m6] mock 端断言：refresh 调用 {refresh_calls} 次、device_code 轮询 {device_polls} 次（应 3 = 2 pending + 1 授权）");

    if loaded_ok && mail_sent && sasl_ok && smtp_ok && refresh_calls == 1 && device_polls == 3 {
        println!("\n[m6 PASS] 设备流 ✅ XOAUTH2 IMAP（线上 SASL 串）✅ 刷新 ✅ XOAUTH2 SMTP ✅");
        std::process::exit(0);
    }
    println!(
        "\n[m6 FAIL] loaded={loaded_ok} sent={mail_sent} sasl={sasl_ok} smtp={smtp_ok} refresh={refresh_calls} polls={device_polls}"
    );
    std::process::exit(1);
}
