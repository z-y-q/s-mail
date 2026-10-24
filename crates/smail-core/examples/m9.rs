//! M5-c 端到端：PGP 收信侧（解密 PGP/MIME + 验证明文签名）。
//!
//! 前置：dovecot up.sh（m9 自己生成密钥、自己造加密/签名邮件写进 Maildir）
//!   SMAIL_CONFIG=spikes/spike-melib/config.pgp.toml SMAIL_HOME=/tmp/smail-m9 \
//!     cargo run -p smail-core --example m9 -- 50
//!
//! 通过标准：加密邮件打开后正文含机密内容且 pgp 状态含"已解密"+"签名有效"；
//! 明文签名邮件正文可读且状态含"签名有效" → 退出码 0。

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sequoia_openpgp::serialize::Marshal as _;

use smail_core::pgp;
use smail_core::{Cmd, Event, Session, SessionCmd, TaggedEvent};

fn b64(s: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    // ---- 1. 生成测试密钥 + 密钥环落盘（config.pgp.toml 指向这里） ----
    let keydir = PathBuf::from("/tmp/smail-m9-pgp");
    let _ = std::fs::remove_dir_all(&keydir);
    std::fs::create_dir_all(&keydir).unwrap();
    let cert = pgp::generate_cert("m9 测试 <m9@local>").expect("生成密钥");
    let mut f = std::fs::File::create(keydir.join("secret.asc")).unwrap();
    cert.as_tsk().armored().serialize(&mut f).unwrap();
    let mut f = std::fs::File::create(keydir.join("pub.asc")).unwrap();
    cert.armored().serialize(&mut f).unwrap();
    println!("[m9] 密钥已生成并落盘");

    // 每轮唯一后缀：避免命中上一轮遗留的旧密钥邮件（Maildir 不去重）
    let run_tag = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // ---- 2. 造两封 PGP 邮件写进 dovecot Maildir ----
    // 2a. 加密+内嵌签名（PGP/MIME multipart/encrypted）
    let inner =
        "Content-Type: text/plain; charset=utf-8\r\n\r\n机密正文：会议密码 9527，勿外传。\r\n";
    let armored = pgp::encrypt_and_sign(std::slice::from_ref(&cert), &cert, inner.as_bytes())
        .expect("加密签名");
    let mut enc_mail = Vec::new();
    write!(
        enc_mail,
        "From: =?UTF-8?B?{}?= <m9@local>\r\nTo: =?UTF-8?B?{}?= <test@localhost>\r\nSubject: =?UTF-8?B?{}?=\r\nDate: Sat, 18 Oct 2026 13:10:00 +0800\r\nMessage-ID: <m9-enc-{run_tag}@seed>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"m9enc\"\r\n\r\nThis is an OpenPGP/MIME encrypted message (RFC 4880)\r\n\r\n--m9enc\r\nContent-Type: application/pgp-encrypted\r\n\r\nVersion: 1\r\n\r\n--m9enc\r\nContent-Type: application/octet-stream; name=\"encrypted.asc\"\r\nContent-Disposition: inline; filename=\"encrypted.asc\"\r\n\r\n",
        b64("m9"), b64("我"), b64(&format!("PGP加密：机密通知{run_tag}")),
    )
    .unwrap();
    enc_mail.extend_from_slice(&armored);
    enc_mail.extend_from_slice(b"\r\n--m9enc--\r\n");

    // 2b. 明文签名（multipart/signed，detached 签名覆盖精确的 content 字节）
    let signed_content =
        "Content-Type: text/plain; charset=utf-8\r\n\r\n这是明文签名邮件的正文，防篡改。会议照常。\r\n";
    let sig = pgp::sign_detached(&cert, signed_content.as_bytes()).expect("detached 签名");
    let mut sig_mail = Vec::new();
    write!(
        sig_mail,
        "From: =?UTF-8?B?{}?= <m9@local>\r\nTo: =?UTF-8?B?{}?= <test@localhost>\r\nSubject: =?UTF-8?B?{}?=\r\nDate: Sat, 18 Oct 2026 13:11:00 +0800\r\nMessage-ID: <m9-sig-{run_tag}@seed>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/signed; protocol=\"application/pgp-signature\"; micalg=pgp-sha256; boundary=\"m9sig\"\r\n\r\n--m9sig\r\n",
        b64("m9"), b64("我"), b64(&format!("PGP签名：明文公告{run_tag}")),
    )
    .unwrap();
    sig_mail.extend_from_slice(signed_content.as_bytes());
    sig_mail.extend_from_slice(b"\r\n--m9sig\r\nContent-Type: application/pgp-signature; name=\"signature.asc\"\r\nContent-Disposition: inline; filename=\"signature.asc\"\r\n\r\n");
    sig_mail.extend_from_slice(&sig);
    sig_mail.extend_from_slice(b"\r\n--m9sig--\r\n");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let maildir = PathBuf::from("spikes/spike-melib/imap-test/mail/test/Maildir/new");
    std::fs::write(maildir.join(format!("{now}.M40P1.local")), &enc_mail).unwrap();
    std::fs::write(maildir.join(format!("{now}.M41P1.local")), &sig_mail).unwrap();
    println!("[m9] 两封 PGP 邮件已写入 Maildir");

    // ---- 3. Session 同步 + 打开断言 ----
    let cfg: PathBuf = std::env::var_os("SMAIL_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| PathBuf::from("spikes/spike-melib/config.pgp.toml"));
    let home: PathBuf = std::env::var_os("SMAIL_HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp/smail-m9".into());

    let (session, mut rx) = match Session::start(&cfg, &home) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[m9 启动失败] {e}");
            std::process::exit(1);
        }
    };
    println!("[m9] Session 已启动（PGP 账号）");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut enc_hash: Option<i64> = None;
    let mut sig_hash: Option<i64> = None;
    let mut enc_ok = false;
    let mut sig_ok = false;
    let mut requested = Vec::new();

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
                        println!("[loaded {account}] INBOX: {} 封", rows.len());
                        for r in &rows {
                            if r.subject.contains(&format!("PGP加密：机密通知{run_tag}")) {
                                enc_hash = Some(r.envelope_hash);
                            } else if r.subject.contains(&format!("PGP签名：明文公告{run_tag}"))
                            {
                                sig_hash = Some(r.envelope_hash);
                            }
                        }
                    }
                }
                Event::BodyLoaded {
                    hash,
                    ref body,
                    ref pgp,
                    ..
                } => {
                    let status = pgp.clone().unwrap_or_default();
                    if Some(hash) == enc_hash {
                        enc_ok = body.contains("9527")
                            && status.contains("已解密")
                            && status.contains("签名有效");
                        println!(
                            "[m9] 加密邮件：正文含机密={} 状态={status:?}",
                            body.contains("9527")
                        );
                    } else if Some(hash) == sig_hash {
                        sig_ok = body.contains("防篡改") && status.contains("签名有效");
                        println!(
                            "[m9] 签名邮件：正文可读={} 状态={status:?}",
                            body.contains("防篡改")
                        );
                    }
                }
                _ => {}
            }
        }

        // 依次打开两封
        if let Some(h) = enc_hash {
            if !requested.contains(&h) {
                requested.push(h);
                let _ = session.cmd_tx.send(SessionCmd::Route {
                    account: 0,
                    cmd: Cmd::OpenMessage(h),
                });
            }
        }
        if let Some(h) = sig_hash {
            if !requested.contains(&h) && enc_ok {
                requested.push(h);
                let _ = session.cmd_tx.send(SessionCmd::Route {
                    account: 0,
                    cmd: Cmd::OpenMessage(h),
                });
            }
        }

        if enc_ok && sig_ok {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = session.cmd_tx.send(SessionCmd::Shutdown);

    if enc_ok && sig_ok {
        println!("\n[m9 PASS] PGP/MIME 解密 ✅ 内嵌签名验证 ✅ 明文签名验证 ✅ 正文管线复用 ✅");
        std::process::exit(0);
    }
    println!("\n[m9 FAIL] enc={enc_ok} sig={sig_ok}");
    std::process::exit(1);
}
