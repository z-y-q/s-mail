//! M6 端到端：应用内账号配置（写回配置 + OAuth 设备流在服务线程自动完成 + 连接测试）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   python3 spikes/spike-melib/imap-test/mock_oauth.py 18888 /tmp/mock-oauth-m11.json &
//!   SMAIL_HOME=/tmp/smail-m11 cargo run -p smail-core --example m11 -- 60
//!
//! 通过标准：save_accounts 写出 → load_accounts 读回字段一致（往返）→
//! test_connection 连 dovecot 返回文件夹数 →
//! OAuth 账号（无 token）起 Session：收到 OAuthPrompt（URL+验证码，GUI 显示用）
//! → 服务线程自动轮询授权 → 文件夹加载成功 → 退出码 0。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{save_accounts, slug_for, test_connection, Cmd, Event, Session, SessionCmd};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m11".into());
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    // ---- 1. 构造 OAuth 账号（custom provider → mock），走 GUI 同款路径写盘 ----
    let oauth = smail_core::oauth::OAuthCfg {
        provider: "custom".into(),
        client_id: "m11-client".into(),
        client_secret: None,
        scopes: vec!["mail".into()],
        tenant: None,
        device_endpoint: Some("http://127.0.0.1:18888/devicecode".into()),
        token_endpoint: Some("http://127.0.0.1:18888/token".into()),
    };
    let acct = smail_core::AccountConfig {
        name: "M11OAuth".into(),
        slug: slug_for("M11OAuth", "test", 1),
        identity: "test@local".into(),
        signature: None,
        imap: smail_core::ImapConfig {
            // 走 XOAUTH2→LOGIN 翻译代理（真实 Gmail 场景不需要）
            host: "127.0.0.1".into(),
            port: 11143,
            username: "test".into(),
            password: None,
            starttls: false,
            tls: false,
            use_oauth2: false,
        },
        smtp: None,
        oauth: Some(oauth),
        pgp: None,
        rules: vec![],
        sync_config: Default::default(),
    };
    let cfg_path = home.join("config.toml");
    save_accounts(&cfg_path, std::slice::from_ref(&acct)).expect("save_accounts");

    // ---- 2. 往返：load 回来字段一致 ----
    let back = smail_core::load_accounts(&cfg_path).expect("load_accounts");
    assert_eq!(back.len(), 1);
    let a0 = &back[0];
    let roundtrip_ok = a0.name == "M11OAuth"
        && a0.imap.host == "127.0.0.1"
        && a0.imap.port == 11143
        && a0.imap.password.is_none()
        && a0
            .oauth
            .as_ref()
            .map(|o| o.provider == "custom" && o.client_id == "m11-client")
            .unwrap_or(false);
    println!("[m11] 配置往返：{roundtrip_ok}");

    // ---- 3. 连接测试（dovecot plain） ----
    let plain = smail_core::ImapConfig {
        host: "127.0.0.1".into(),
        port: 1143,
        username: "test".into(),
        password: Some("testpass".into()),
        starttls: false,
        tls: false,
        use_oauth2: false,
    };
    let test_res = test_connection(&plain);
    let test_ok = matches!(test_res, Ok(n) if n >= 1);
    println!("[m11] 连接测试：{test_ok}（{test_res:?}）");

    // ---- 4. Session：OAuth 设备流应在服务线程自动完成（无 token 起步） ----
    let (session, mut rx) = match Session::start(&cfg_path, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m11 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m11] Session 已启动（等待设备流横幅 + 自动授权）");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut got_prompt = false;
    let mut got_folders = false;
    let mut selected = false;
    // 事件驱动完成条件：INBOX 实际加载出邮件（旧版此处睡 1500ms「给加载
    // 一点时间」——慢 CI 下不够会误报，快环境下白等，且断言从未真检查过加载）
    let mut inbox_loaded = false;

    while Instant::now() < deadline {
        while let Ok(ev) = rx.try_recv() {
            match ev.event {
                Event::OAuthPrompt { url, code } => {
                    got_prompt = !url.is_empty() && !code.is_empty();
                    println!("[m11] OAuthPrompt url={url} code={code}");
                }
                Event::Status(s) => println!("[status] {s}"),
                Event::Error(s) => println!("[error ] {s}"),
                Event::Folders(f) => {
                    got_folders = !f.is_empty();
                    println!(
                        "[m11] folders: {:?}",
                        f.iter().map(|x| x.path.clone()).collect::<Vec<_>>()
                    );
                }
                Event::FolderLoaded { folder, rows } => {
                    if folder.eq_ignore_ascii_case("INBOX") && !rows.is_empty() {
                        inbox_loaded = true;
                        println!("[m11] INBOX {} 封（设备流自动授权后同步成功）", rows.len());
                    }
                }
                _ => {}
            }
        }
        if got_prompt && got_folders && !selected {
            selected = true;
            let _ = session.cmd_tx.send(SessionCmd::Route {
                account: 0,
                cmd: Cmd::SelectFolder("INBOX".into()),
            });
        }
        if got_prompt && got_folders && selected && inbox_loaded {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);

    if roundtrip_ok && test_ok && got_prompt && got_folders && inbox_loaded {
        println!("\n[m11 PASS] 配置写读往返 ✅ 连接测试 ✅ 设备流横幅（GUI 数据源）✅ 服务线程自动授权+同步（INBOX 已加载）✅");
        std::process::exit(0);
    }
    println!(
        "\n[m11 FAIL] roundtrip={roundtrip_ok} test={test_ok} prompt={got_prompt} folders={got_folders} inbox_loaded={inbox_loaded}"
    );
    std::process::exit(1);
}
