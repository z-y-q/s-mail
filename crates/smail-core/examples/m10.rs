//! M5-d 端到端：PGP 发信侧（compose 签名/加密 → SMTP raw → 自我互操作验证）。
//!
//! 前置：
//!   sh spikes/spike-melib/imap-test/up.sh
//!   python3 spikes/spike-melib/imap-test/mini_smtp.py 11525 &
//!   SMAIL_CONFIG=spikes/spike-melib/config.pgpsend.toml SMAIL_HOME=/tmp/smail-m10 \
//!     cargo run -p smail-core --example m10 -- 50
//!
//! 通过标准：发出加密邮件（mini_smtp 落盘含 multipart/encrypted + BEGIN PGP MESSAGE，
//! 且能用我们自己的收信侧解密+验签=自我互操作）→ 发出签名邮件（multipart/signed，
//! detached 验签通过）→ 两封都 APPEND 进 Sent（服务器真相）→ 退出码 0。

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sequoia_openpgp::serialize::Marshal as _;

use smail_core::pgp;
use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    // ---- 1. 密钥 ----
    let keydir = PathBuf::from("/tmp/smail-m10-pgp");
    let _ = std::fs::remove_dir_all(&keydir);
    std::fs::create_dir_all(&keydir).unwrap();
    let cert = pgp::generate_cert("m10 <m10@local>").expect("生成密钥");
    let mut f = std::fs::File::create(keydir.join("secret.asc")).unwrap();
    cert.as_tsk().armored().serialize(&mut f).unwrap();
    let mut f = std::fs::File::create(keydir.join("pub.asc")).unwrap();
    cert.armored().serialize(&mut f).unwrap();
    println!("[m10] 密钥已生成并落盘");

    // ---- 2. Session ----
    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("spikes/spike-melib/config.pgpsend.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m10".into());
    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m10 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m10] Session 已启动");

    // ---- 3. 发两封 PGP 邮件 ----
    let send = |subject: &str, body: &str, sign: bool, encrypt: bool| {
        let _ = session.cmd_tx.send(SessionCmd::Route {
            account: 0,
            cmd: Cmd::SendMail {
                to: "m10@local".into(),
                cc: String::new(),
                bcc: String::new(),
                subject: subject.into(),
                body: body.into(),
                attachments: vec![],
                sign,
                encrypt,
            },
        });
    };

    // 首封：加密+签名（收件人 m10@local 在 pubring 里）
    send(
        "PGP发信：加密测试",
        "这是加密邮件正文，含机密数字 8899。",
        false,
        true,
    );
    println!("[m10] 已请求发送加密邮件");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut sent_enc = false;
    let mut sent_sig = false;
    let mut sent_rows = Vec::new();
    let t0 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

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
                    if folder.eq_ignore_ascii_case("Sent") {
                        sent_rows = rows;
                        println!("[loaded {account}] Sent: {} 封", sent_rows.len());
                    }
                }
                Event::MailSent { subject, .. } => {
                    if subject.contains("PGP发信：加密") {
                        sent_enc = true;
                        send(
                            "PGP发信：签名测试",
                            "这是签名邮件正文，明文可读。",
                            true,
                            false,
                        );
                    } else if subject.contains("PGP发信：签名") {
                        sent_sig = true;
                    }
                    println!("[sent {account}] {subject:?}");
                }
                _ => {}
            }
        }

        // 两封都发出后，等 mini_smtp 落盘 + 拉 Sent
        if sent_enc && sent_sig {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);

    // ---- 4. 断言：mini_smtp 落盘 + 自我互操作 ----
    let eml1 = std::fs::read("/tmp/mini-smtp-mail-1.eml").unwrap_or_default();
    let eml2 = std::fs::read("/tmp/mini-smtp-mail-2.eml").unwrap_or_default();
    let structure_enc = String::from_utf8_lossy(&eml1).contains("multipart/encrypted")
        && String::from_utf8_lossy(&eml1).contains("BEGIN PGP MESSAGE");
    let structure_sig = String::from_utf8_lossy(&eml2).contains("multipart/signed")
        && String::from_utf8_lossy(&eml2).contains("BEGIN PGP SIGNATURE");
    println!(
        "[m10] 线上结构：加密邮件={structure_enc}（{} B）、签名邮件={structure_sig}（{} B）",
        eml1.len(),
        eml2.len()
    );

    // 自我互操作：我们的收信侧读自己发的邮件
    let rt = pgp::PgpRuntime::load(&pgp::PgpCfg {
        key_path: keydir.join("secret.asc").to_string_lossy().to_string(),
        passphrase: None,
        pubring_path: Some(keydir.join("pub.asc").to_string_lossy().to_string()),
        wkd: false,
    })
    .expect("载入密钥");
    let interop_enc = (|| -> Option<bool> {
        if pgp::detect_pgp_kind(&eml1) != Some(pgp::PgpKind::Encrypted) {
            return None;
        }
        let ct = pgp::extract_pgp_ciphertext(&eml1)?;
        let (plain, st) = rt.decrypt(&ct).ok()?;
        println!("[m10] 自我互操作（解密）：{}", st);
        println!(
            "[m10]   内层含正文实体：{}",
            String::from_utf8_lossy(&plain).contains("Content-Type: text/plain")
        );
        Some(st.contains("已解密") && st.contains("签名有效"))
    })()
    .unwrap_or(false);
    let interop_sig = (|| -> Option<bool> {
        if pgp::detect_pgp_kind(&eml2) != Some(pgp::PgpKind::Signed) {
            return None;
        }
        let (content, sig) = pgp::signed_mime_parts(&eml2)?;
        let st = rt.verify_detached(&content, &sig);
        println!("[m10] 自我互操作（验签）：{st}");
        Some(st.contains("签名有效"))
    })()
    .unwrap_or(false);

    let in_sent = sent_rows
        .iter()
        .filter(|r| r.subject.contains("PGP发信："))
        .count()
        >= 2;

    let _ = t0;
    if structure_enc
        && structure_sig
        && interop_enc
        && interop_sig
        && in_sent
        && sent_enc
        && sent_sig
    {
        println!("\n[m10 PASS] 加密发送 ✅ 签名发送 ✅ 自我互操作（解密+验签）✅ Sent 归档 ✅");
        std::process::exit(0);
    }
    println!(
        "\n[m10 FAIL] enc={sent_enc} sig={sent_sig} 结构E={structure_enc} 结构S={structure_sig} 互操作E={interop_enc} 互操作S={interop_sig} sent归档={in_sent}"
    );
    std::process::exit(1);
}
