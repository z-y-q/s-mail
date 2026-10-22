//! M12 端到端：设备流可中断性（会话关闭不挂死）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh   # 无所谓，本测试不连 IMAP 成功路径
//!   python3 spikes/spike-melib/imap-test/mock_oauth_never.py 18889 &
//!   SMAIL_HOME=/tmp/smail-m12 cargo run -p smail-core --example m12 -- 30
//!
//! 通过标准：OAuth 账号无 token 起 Session（服务线程进入永不授权的设备流）→
//! 收到 OAuthPrompt → drop Session 耗时 < 10s（修复前会卡到设备码过期=1 小时）→
//! 退出码 0。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smail_core::{Event, Session, TaggedEvent};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m12".into());
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    // 配置：oauth 账号指向 never-grant mock（IMAP 随便填，走不到）
    let cfg_path = home.join("config.toml");
    std::fs::write(
        &cfg_path,
        r#"
[[accounts]]
name = "NeverAuth"
[accounts.imap]
host = "127.0.0.1"
port = 1143
username = "test"
tls = false
[accounts.oauth]
provider = "custom"
device_endpoint = "http://127.0.0.1:18889/devicecode"
token_endpoint = "http://127.0.0.1:18889/token"
client_id = "x"
scopes = ["mail"]
"#,
    )
    .unwrap();

    let (session, mut rx) = match Session::start(&cfg_path, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m12 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m12] Session 已启动（服务线程应已进入设备流轮询）");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut got_prompt = false;
    while Instant::now() < deadline {
        while let Ok(TaggedEvent { event, .. }) = rx.try_recv() {
            if let Event::OAuthPrompt { url, code } = event {
                got_prompt = true;
                println!("[m12] OAuthPrompt {url} / {code}（永不授权，线程挂在轮询）");
            }
        }
        if got_prompt {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !got_prompt {
        eprintln!("[m12 FAIL] 未收到 OAuthPrompt");
        std::process::exit(1);
    }
    std::thread::sleep(Duration::from_secs(3)); // 确保已处于轮询等待中

    // 关键测量：drop（触发 Shutdown + stop 旗 + join）
    let t0 = Instant::now();
    drop(session);
    let elapsed = t0.elapsed();
    println!("[m12] Session drop 耗时 {:.2}s", elapsed.as_secs_f64());

    if elapsed < Duration::from_secs(10) {
        println!("\n[m12 PASS] 设备流轮询被 stop 旗中断，会话干净退出 ✅");
        std::process::exit(0);
    }
    println!("\n[m12 FAIL] drop 卡了 {elapsed:?}（stop 旗未生效？）");
    std::process::exit(1);
}
