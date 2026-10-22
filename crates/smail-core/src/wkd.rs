//! WKD（Web Key Directory）公钥自动发现。
//!
//! <https://datatracker.ietf.org/doc/html/draft-koch-openpgp-webkey-service>：
//! 按收件人邮箱向其域名查询公开托管的 OpenPGP 证书。advanced 方法
//! `openpgpkey.<domain>` 优先（防篡改），退回 direct 方法。
//!
//! 隐私说明：查询会把收件人邮箱暴露给对方域名服务器——这是 WKD 的固有
//! 特性（GnuPG 同样如此）；默认关闭（`[accounts.pgp] wkd = true` 显式开启），
//! 符合本项目“宁可不发不错发”的保守姿态。

use sequoia_openpgp::cert::prelude::*;
use sequoia_openpgp::parse::Parse;

/// user-id 哈希：SHA-1(小写 local part) 的 z-base-32（WKD 规范约定）。
/// sequoia 2.x 无公开哈希接口，用独立 sha1 实现
pub fn wkd_hash(local: &str) -> Result<String, String> {
    use sha1::{Digest as _, Sha1};
    let digest = Sha1::digest(local.to_lowercase().as_bytes());
    Ok(zbase32(&digest))
}

/// z-base-32（RFC 6189 字母表，MSB-first，无填充）。
/// 实现已对照真实 WKD 服务验证（protonmail test → iffe93qcsgp4c8ncbb378rxjo6cn9q6u）
fn zbase32(data: &[u8]) -> String {
    const ALPH: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8 + 1);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in data {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(ALPH[((acc >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(ALPH[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn wkd_urls(email: &str) -> Result<Vec<String>, String> {
    let (local, domain) = email
        .split_once('@')
        .ok_or_else(|| format!("邮箱 {email:?} 无 @，无法做 WKD 查询"))?;
    let domain = domain.to_lowercase(); // 域名大小写不敏感，URL 统一小写
    let hu = wkd_hash(local)?;
    let l = urlencoding_lite(local);
    Ok(vec![
        // advanced（专用子域，DNS 成为信任边界的一部分）
        format!("https://openpgpkey.{domain}/.well-known/openpgpkey/{domain}/hu/{hu}?l={l}"),
        // direct
        format!("https://{domain}/.well-known/openpgpkey/hu/{hu}?l={l}"),
    ])
}

/// 极简百分号编码（query 的 l 参数只需处理非保留字符外的情况）
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// best-effort 发现：任一 URL 命中且能解析为证书即返回；全部未命中/失败返回 None
pub fn discover_cert(email: &str) -> Option<Cert> {
    for url in wkd_urls(email).ok()? {
        let resp = match ureq::get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .call()
        {
            Ok(r) if r.status() == 200 => r,
            _ => continue,
        };
        let mut bytes = Vec::with_capacity(2048);
        use std::io::Read as _;
        if resp
            .into_reader()
            .take(256 * 1024) // 上限 256KB：防御异常大响应
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        if let Ok(cert) = Cert::from_bytes(&bytes) {
            return Some(cert);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zbase32_known_vectors() {
        // 由独立实现（python hashlib + RFC 6189 字母表）生成，非手写记忆值
        let cases: &[(&str, &str)] = &[
            ("joe", "n4w4kuq9ejc3kmthngg8ccja7y5j8i97"),
            ("me@example.com", "qgm9qsh1nqduatu3z9f3okoy1o9hb97u"),
            ("a", "o556ep94wsu93ak7dzqmu4zk7e5zc37a"),
            ("test", "iffe93qcsgp4c8ncbb378rxjo6cn9q6u"),
        ];
        for (input, want) in cases {
            let got = wkd_hash(input).unwrap();
            assert_eq!(&got, want, "local={input:?}");
        }
    }

    #[test]
    fn url_construction() {
        let urls = wkd_urls("Joe@Example.COM").unwrap();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].starts_with("https://openpgpkey.example.com/"));
        assert!(
            urls[0].contains("/hu/n4w4kuq9ejc3kmthngg8ccja7y5j8i97?l=Joe"),
            "{}",
            urls[0]
        );
        assert!(urls[1].starts_with("https://example.com/.well-known/openpgpkey/hu/"));
        // 哈希用小写 local；l 参数保留原样
        assert!(urls[0].contains("l=Joe"));
    }

    /// 真实网络集成（CI 跳过）：SMAIL_WKD_TEST=1 cargo test -- --ignored
    #[test]
    #[ignore]
    fn discover_proton_test_key() {
        let cert = discover_cert("test@protonmail.com").expect("protonmail WKD 应命中");
        assert!(
            cert.userids()
                .any(|u| { u.userid().to_string().contains("protonmail.com") }),
            "返回证书应含 protonmail.com userid"
        );
    }
}
