//! PGP 加密与签名（M5-c）
//!
//! ## 职责
//!
//! - **解密 PGP/MIME**：multipart/encrypted（RFC 3156）邮件解密
//! - **签名验证**：内嵌签名（one-pass）+ 分离签名（multipart/signed）
//! - **发信加密**：PGP 加密 + 签名（基于 sequoia-openpgp）
//!
//! ## 密钥约定
//!
//! 账号数据目录下：
//! - **私钥**（解密用）：`config.toml` 中 `[accounts.pgp].key_path` 指向 armored 私钥文件
//! - **公钥环**（验签用）：`[accounts.pgp].pubring_path`，armored 格式，可含多个证书
//!
//! ## 技术栈
//!
//! - **sequoia-openpgp**：纯 Rust 实现（crypto-rust 后端）
//! - **算法支持**：Cv25519/RSA/AES（常见算法够用）
//!
//! ## 设计取舍
//!
//! - **索引策略**：解密结果不进 FTS5 全文索引（安全默认：加密邮件只有信封可搜）
//! - **验签精度**：multipart/signed 按 RFC 3156 手工切 boundary 取精确字节验签
//!   - 避免 MIME 解析器规范化破坏签名
//! - **密码输入**：首次使用时提示输入私钥密码，内存缓存（不落盘）
//!
//! ## 工作流
//!
//! ### 收信解密
//! ```text
//! multipart/encrypted
//!   ├─ application/pgp-encrypted (version)
//!   └─ application/octet-stream (PGP message)
//!        ↓ DecryptorBuilder
//!      明文邮件
//! ```
//!
//! ### 发信加密
//! ```text
//! 明文邮件 ──Encryptor──> PGP message ──Armorer──> ASCII armor
//! ```

use std::io::Read as _;
use std::io::Write as _;

use mail_parser::MimeHeaders;
use openpgp::cert::prelude::*;
use openpgp::crypto::Password;
use openpgp::parse::stream::{DecryptorBuilder, DetachedVerifierBuilder};
use openpgp::parse::{stream::*, Parse};
use openpgp::policy::StandardPolicy;
use openpgp::serialize::stream::{Armorer, Encryptor, LiteralWriter, Message, Recipient, Signer};
use sequoia_openpgp as openpgp;

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PgpCfg {
    /// 私钥文件（armored，含证书+私钥；解密用）
    pub key_path: String,
    /// 私钥口令（无私钥口令的证书可省）
    #[serde(default)]
    pub passphrase: Option<String>,
    /// 公钥环（armored，可含多个证书；验签用。缺省=能解密但不验签）
    #[serde(default)]
    pub pubring_path: Option<String>,
    /// WKD 自动发现（收件人缺公钥时向其域名查询；隐私考量默认关）
    #[serde(default)]
    pub wkd: bool,
}

// ---------------------------------------------------------------------------
// 运行时
// ---------------------------------------------------------------------------

/// 展开 ~/ 前缀（README 示例路径写法；PgpRuntime::load 历来不展开导致直接读失败）
fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home)
                .join(rest)
                .display()
                .to_string();
        }
    }
    p.to_string()
}

pub struct PgpRuntime {
    secret: Option<Cert>,
    passphrase: Option<Password>,
    pubring: Vec<Cert>,
    policy: StandardPolicy<'static>,
}

/// 验签结论
#[derive(Clone)]
enum VerifyOutcome {
    /// uid
    Good(String),
    NoKey,
    Bad,
}

/// stream Helper：解密 + 验签共用的状态机
struct Helper {
    secret: Option<Cert>,
    passphrase: Option<Password>,
    pubring: Vec<Cert>,
    policy: StandardPolicy<'static>,
    verified: Option<VerifyOutcome>,
}

impl VerificationHelper for Helper {
    fn get_certs(&mut self, _handles: &[openpgp::KeyHandle]) -> openpgp::Result<Vec<Cert>> {
        // 全量返回，check() 里按签发者精确匹配
        Ok(self.pubring.clone())
    }

    fn check(&mut self, structure: MessageStructure) -> openpgp::Result<()> {
        for layer in structure {
            if let MessageLayer::SignatureGroup { results } = layer {
                for r in results {
                    match r {
                        Ok(good) => {
                            let cert = good.ka.cert();
                            // 无 userid 时用指纹（valid cert 必有指纹）
                            let uid = cert
                                .userids()
                                .next()
                                .map(|u| u.userid().to_string())
                                .unwrap_or_else(|| cert.fingerprint().to_string());
                            self.verified = Some(VerifyOutcome::Good(uid));
                        }
                        Err(openpgp::parse::stream::VerificationError::MissingKey { .. }) => {
                            self.verified = Some(VerifyOutcome::NoKey);
                            return Err(anyhow::anyhow!("没有匹配的公钥"));
                        }
                        Err(e) => {
                            self.verified = Some(VerifyOutcome::Bad);
                            return Err(anyhow::anyhow!("签名无效: {e}"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl DecryptionHelper for Helper {
    fn decrypt(
        &mut self,
        pkesks: &[openpgp::packet::PKESK],
        skesks: &[openpgp::packet::SKESK],
        sym_algo: Option<openpgp::types::SymmetricAlgorithm>,
        decrypt: &mut dyn FnMut(
            Option<openpgp::types::SymmetricAlgorithm>,
            &openpgp::crypto::SessionKey,
        ) -> bool,
    ) -> openpgp::Result<Option<Cert>> {
        let Some(secret) = self.secret.clone() else {
            return Err(anyhow::anyhow!("未配置 PGP 私钥"));
        };
        // 先试公钥加密会话（PKESK）
        for pkesk in pkesks {
            for ka in secret.keys().secret().with_policy(&self.policy, None) {
                let key = ka.key().clone();
                // 未加密私钥直接成对；加密私钥用口令解锁
                let pair = key.clone().into_keypair().or_else(|_| {
                    self.passphrase
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("私钥有口令但未配置 passphrase"))
                        .and_then(|p| key.decrypt_secret(&p))
                        .and_then(|k| k.into_keypair())
                });
                let Ok(mut pair) = pair else {
                    continue;
                };
                if let Some((algo, sk)) = pkesk.decrypt(&mut pair, sym_algo) {
                    if decrypt(algo, &sk) {
                        return Ok(Some(secret));
                    }
                }
            }
        }
        // 再试口令加密会话（SKESK）
        if let Some(p) = &self.passphrase {
            for skesk in skesks {
                if let Ok((algo, sk)) = skesk.decrypt(p) {
                    if decrypt(algo, &sk) {
                        return Ok(None);
                    }
                }
            }
        }
        Err(anyhow::anyhow!("解密失败：私钥不匹配或口令错误"))
    }
}

impl PgpRuntime {
    pub fn load(cfg: &PgpCfg) -> Result<Self, String> {
        let policy = StandardPolicy::new();
        let key_path = expand_tilde(&cfg.key_path);
        let secret = std::fs::read(&key_path)
            .map_err(|e| format!("读 PGP 私钥 {key_path} 失败: {e}"))
            .and_then(|b| Cert::from_bytes(&b).map_err(|e| format!("解析私钥失败: {e}")))?;
        let pubring = match &cfg.pubring_path {
            Some(p) => {
                let p = expand_tilde(p);
                Cert::from_bytes(&std::fs::read(&p).map_err(|e| format!("读公钥环 {p} 失败: {e}"))?)
                    .map(|c| vec![c])
                    .map_err(|e| format!("解析公钥环失败: {e}"))?
            }
            None => vec![],
        };
        Ok(Self {
            secret: Some(secret),
            passphrase: cfg.passphrase.clone().map(Password::from),
            pubring,
            policy,
        })
    }

    /// 私钥证书（发信签名用）
    pub fn secret_cert(&self) -> Result<Cert, String> {
        self.secret
            .clone()
            .ok_or_else(|| "未配置 PGP 私钥".to_string())
    }

    /// 按邮箱地址在公钥环里找证书（uid 子串匹配，大小写不敏感）
    pub fn find_cert_by_email(&self, email: &str) -> Option<Cert> {
        let e = email.to_lowercase();
        self.pubring
            .iter()
            .find(|c| {
                c.userids()
                    .any(|u| u.userid().to_string().to_lowercase().contains(&e))
            })
            .cloned()
    }

    /// 解密 armored/binary OpenPGP 消息；返回 (明文, 状态行)
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<(Vec<u8>, String), String> {
        let helper = Helper {
            secret: self.secret.clone(),
            passphrase: self.passphrase.clone(),
            pubring: self.pubring.clone(),
            policy: StandardPolicy::new(),
            verified: None,
        };
        let mut reader = DecryptorBuilder::from_bytes(ciphertext)
            .map_err(|e| format!("解析密文失败: {e}"))?
            .with_policy(&self.policy, None, helper)
            .map_err(|e| format!("解密失败: {e}"))?;
        let mut plain = Vec::new();
        reader
            .read_to_end(&mut plain)
            .map_err(|e| format!("读解密流失败: {e}"))?;
        let helper = reader.into_helper();
        let sig_part = match helper.verified {
            Some(VerifyOutcome::Good(uid)) => format!(" · ✓ 签名有效（{uid}）"),
            Some(VerifyOutcome::NoKey) => " · ⚠ 有签名但无匹配公钥".to_string(),
            Some(VerifyOutcome::Bad) => " · ✗ 签名无效".to_string(),
            None => String::new(),
        };
        Ok((plain, format!("🔓 已解密{sig_part}")))
    }

    /// 验证 detached 签名（RFC 3156 multipart/signed）。返回状态行（不 Err，失败也是状态）
    pub fn verify_detached(&self, content: &[u8], sig: &[u8]) -> String {
        if self.pubring.is_empty() {
            return "⚠ 签名未验证（未配置公钥环）".to_string();
        }
        let helper = Helper {
            secret: None,
            passphrase: None,
            pubring: self.pubring.clone(),
            policy: StandardPolicy::new(),
            verified: None,
        };
        let result = DetachedVerifierBuilder::from_bytes(sig)
            .and_then(|b| b.with_policy(&self.policy, None, helper))
            .and_then(|mut v| {
                v.verify_bytes(content)?;
                Ok(v.into_helper())
            });
        let verified = result.as_ref().ok().and_then(|h| h.verified.clone());
        match (result.is_ok(), verified) {
            (true, Some(VerifyOutcome::Good(uid))) => format!("✓ 签名有效（{uid}）"),
            (_, Some(VerifyOutcome::NoKey)) => "⚠ 有签名但无匹配公钥".to_string(),
            _ => "✗ 签名无效".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// MIME 探测与拆解
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PgpKind {
    /// multipart/encrypted + protocol=application/pgp-encrypted
    Encrypted,
    /// multipart/signed + protocol=application/pgp-signature
    Signed,
}

/// 看顶层 Content-Type 头判断 PGP/MIME 类型（不做完整解析）
pub fn detect_pgp_kind(raw: &[u8]) -> Option<PgpKind> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n"))?;
    let head = String::from_utf8_lossy(&raw[..header_end]).to_lowercase();
    // 折叠续行后找 content-type
    let mut ct = String::new();
    for line in head.lines() {
        if line.starts_with("content-type:") {
            ct.push_str(line.trim_start_matches("content-type:").trim());
        } else if line.starts_with(' ') || line.starts_with('\t') {
            ct.push_str(line.trim());
        }
    }
    if ct.contains("multipart/encrypted") && ct.contains("application/pgp-encrypted") {
        Some(PgpKind::Encrypted)
    } else if ct.contains("multipart/signed") && ct.contains("application/pgp-signature") {
        Some(PgpKind::Signed)
    } else {
        None
    }
}

/// 取顶层 Content-Type 头的原始值（不改变大小写——boundary 是大小写敏感的）
fn top_content_type(raw: &[u8]) -> Option<String> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n"))?;
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let mut ct = String::new();
    for line in head.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("content-type:") {
            ct.push_str(line[13..].trim());
        } else if line.starts_with(' ') || line.starts_with('\t') {
            ct.push_str(line.trim());
        }
    }
    if ct.is_empty() {
        None
    } else {
        Some(ct)
    }
}

/// 取 boundary 参数
fn boundary_of(raw: &[u8]) -> Option<String> {
    let ct = top_content_type(raw)?;
    for param in ct.split(';') {
        let param = param.trim();
        if let Some(b) = param.strip_prefix("boundary=") {
            let b = b.trim().trim_matches('"');
            if !b.is_empty() {
                return Some(b.to_string());
            }
        }
    }
    None
}

/// multipart/signed 拆 (签名的原文部分, 签名部分)。原文按 RFC 2046 语义：
/// 第一个分隔行之后的 CRLF 起、到下一个分隔行之前的 CRLF 止（不含属于分隔符的 CRLF）。
pub fn signed_mime_parts(raw: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let boundary = boundary_of(raw)?;
    let body_start = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|p| p + 2))?;
    let body = &raw[body_start..];
    // 第一个 "--boundary" 行（返回长度已含行尾 CRLF）
    let (_, l1) = find_line_start(body, &boundary)?;
    let rest = &body[l1..];
    // 下一个 "--boundary" 行：其前的 CRLF 属于分隔符，剥掉即得签名原文
    let (p2, l2) = find_line_start(rest, &boundary)?;
    let content = rest[..trim_one_eol(&rest[..p2])].to_vec();
    // 第二部分（签名）：到 "--boundary--" 或结尾
    let after2 = &rest[p2 + l2..];
    let sig_end = find_line_start(after2, &format!("{boundary}--"))
        .map(|(p, _)| p)
        .unwrap_or(after2.len());
    let sig = strip_part_headers(&after2[..sig_end])?;
    Some((content, sig))
}

/// 找以 "--{boundary}" 开头的行的 (位置, 行长含换行)
fn find_line_start(hay: &[u8], boundary: &str) -> Option<(usize, usize)> {
    find_line_start_at(hay, 0, boundary)
}

fn find_line_start_at(hay: &[u8], skip: usize, boundary: &str) -> Option<(usize, usize)> {
    let needle = format!("--{boundary}");
    let mut found = 0;
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..].starts_with(needle.as_bytes())
            && (i == 0 || hay[i - 1] == b'\n')
            && (i + needle.len() == hay.len()
                || hay[i + needle.len()] == b'\r'
                || hay[i + needle.len()] == b'\n')
        {
            if found == skip {
                let mut end = i + needle.len();
                if end < hay.len() && hay[end] == b'\r' {
                    end += 1;
                }
                if end < hay.len() && hay[end] == b'\n' {
                    end += 1;
                }
                return Some((i, end - i));
            }
            found += 1;
        }
        i += 1;
    }
    None
}

/// 去掉结尾一个行尾（属于下一个分隔符的 CRLF）
fn trim_one_eol(mut b: &[u8]) -> usize {
    if b.ends_with(b"\n") {
        b = &b[..b.len() - 1];
    }
    if b.ends_with(b"\r") {
        b = &b[..b.len() - 1];
    }
    b.len()
}

/// 剥离 MIME part 头（到第一个空行）
fn strip_part_headers(part: &[u8]) -> Option<Vec<u8>> {
    let pos = part
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .or_else(|| part.windows(2).position(|w| w == b"\n\n").map(|p| p + 2))?;
    Some(part[pos..].to_vec())
}

/// 用 mail-parser 取指定 content-type 的顶层子 part 内容（解密密文部分用）
pub fn extract_pgp_ciphertext(raw: &[u8]) -> Option<Vec<u8>> {
    let msg = mail_parser::MessageParser::default().parse(raw)?;
    for part in msg.attachments() {
        let ct = part.content_type()?;
        if ct.ctype().eq_ignore_ascii_case("application")
            && ct
                .subtype()
                .map(|s| s.eq_ignore_ascii_case("octet-stream"))
                .unwrap_or(false)
        {
            return Some(part.contents().to_vec());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 写侧（e2e 造数据 + 后续发信用）
// ---------------------------------------------------------------------------

/// 生成测试证书（e2e 用；无口令私钥）
pub fn generate_cert(uid: &str) -> Result<Cert, String> {
    // 注意：2.4 的 CertBuilder 默认只生成 Certify 主键，必须显式加子键
    let (cert, _rev) = CertBuilder::new()
        .add_userid(uid)
        .set_cipher_suite(CipherSuite::Cv25519)
        .add_signing_subkey()
        .add_transport_encryption_subkey()
        .add_storage_encryption_subkey()
        .generate()
        .map_err(|e| format!("生成密钥失败: {e}"))?;
    Ok(cert)
}

/// 签名并加密（sign-then-encrypt，armored 输出；加密给全部收件人证书）
pub fn encrypt_and_sign(
    recipients: &[Cert],
    signer: &Cert,
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let policy = StandardPolicy::new();
    let mut sink = Vec::new();
    let msg = Message::new(&mut sink);
    let writer = Armorer::new(msg)
        .build()
        .map_err(|e| format!("armor 失败: {e}"))?;
    let recipients: Vec<Recipient> = recipients
        .iter()
        .flat_map(|c| {
            c.keys()
                .with_policy(&policy, None)
                .for_transport_encryption()
                .map(|ka| -> Recipient { ka.into() })
                .collect::<Vec<_>>()
        })
        .collect();
    let writer = Encryptor::for_recipients(writer, recipients)
        .build()
        .map_err(|e| format!("加密器失败: {e}"))?;
    let signing = signer
        .keys()
        .secret()
        .with_policy(&policy, None)
        .for_signing()
        .next()
        .and_then(|ka| ka.key().clone().into_keypair().ok())
        .ok_or_else(|| "签名密钥不可用".to_string())?;
    let writer = Signer::new(writer, signing)
        .map_err(|e| format!("签名器失败: {e}"))?
        .build()
        .map_err(|e| format!("签名器失败: {e}"))?;
    let mut literal = LiteralWriter::new(writer)
        .build()
        .map_err(|e| format!("literal 失败: {e}"))?;
    literal
        .write_all(plaintext)
        .map_err(|e| format!("写入明文失败: {e}"))?;
    literal
        .finalize()
        .map_err(|e| format!("finalize 失败: {e}"))?;
    Ok(sink)
}

/// detached 签名（armored；RFC 3156 multipart/signed 用）
pub fn sign_detached(signer: &Cert, data: &[u8]) -> Result<Vec<u8>, String> {
    let policy = StandardPolicy::new();
    let mut sink = Vec::new();
    let msg = Message::new(&mut sink);
    let writer = Armorer::new(msg)
        .kind(openpgp::armor::Kind::Signature)
        .build()
        .map_err(|e| format!("armor 失败: {e}"))?;
    let signing = signer
        .keys()
        .secret()
        .with_policy(&policy, None)
        .for_signing()
        .next()
        .and_then(|ka| ka.key().clone().into_keypair().ok())
        .ok_or_else(|| "签名密钥不可用".to_string())?;
    let mut signer_w = Signer::new(writer, signing)
        .map_err(|e| format!("签名器失败: {e}"))?
        .detached()
        .build()
        .map_err(|e| format!("签名器失败: {e}"))?;
    signer_w
        .write_all(data)
        .map_err(|e| format!("写入数据失败: {e}"))?;
    signer_w
        .finalize()
        .map_err(|e| format!("finalize 失败: {e}"))?;
    Ok(sink)
}

// ---------------------------------------------------------------------------
// 发信构造（M5-d）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PgpSendMode {
    /// 仅签名（明文可读，detached）
    Sign,
    /// 加密 + 签名（收件人公钥 + 自己，防篡改）
    EncryptSign,
}

/// PGP 发信参数（上层已解析好地址与密钥）
pub struct OutgoingPgp<'a> {
    pub from: &'a str,
    pub to: &'a [String],
    pub cc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub attachments: &'a [(String, Vec<u8>)],
    pub mode: PgpSendMode,
    /// 签名用私钥证书
    pub signer: &'a Cert,
    /// 收件人公钥（不含自己；加密时会自动加上 self_cert）
    pub recipient_certs: &'a [Cert],
    /// 自己的证书（encrypt-to-self，Sent 可解）
    pub self_cert: &'a Cert,
    pub message_id: &'a str,
}

/// 构造完整 PGP/MIME RFC822 邮件字节
pub fn build_outgoing(m: &OutgoingPgp) -> Result<Vec<u8>, String> {
    // ---- 内层 MIME 实体（签名/加密的覆盖对象；全 base64 CTE，字节精确） ----
    let mut inner: Vec<u8> = Vec::new();
    let text_ct =
        "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n";
    let body_b64 = b64_wrap(&base64_encode(m.body.as_bytes()));
    if m.attachments.is_empty() {
        inner.extend_from_slice(text_ct.as_bytes());
        inner.extend_from_slice(body_b64.as_bytes());
    } else {
        let b = "s1mailmix";
        inner.extend_from_slice(
            format!("Content-Type: multipart/mixed; boundary=\"{b}\"\r\n\r\n").as_bytes(),
        );
        inner.extend_from_slice(format!("--{b}\r\n").as_bytes());
        inner.extend_from_slice(text_ct.as_bytes());
        inner.extend_from_slice(body_b64.as_bytes());
        inner.extend_from_slice(b"\r\n");
        for (name, data) in m.attachments {
            let mime = mime_guess::from_path(name)
                .first_or_octet_stream()
                .to_string();
            inner.extend_from_slice(
                format!(
                    "--{b}\r\nContent-Type: {mime}; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n"
                )
                .as_bytes(),
            );
            inner.extend_from_slice(b64_wrap(&base64_encode(data)).as_bytes());
            inner.extend_from_slice(b"\r\n");
        }
        inner.extend_from_slice(format!("--{b}--\r\n").as_bytes());
    }

    // ---- 顶层包装 ----
    let mut top: Vec<u8> = Vec::new();
    match m.mode {
        PgpSendMode::Sign => {
            let sig = sign_detached(m.signer, &inner)?;
            let b = "s1mailsig";
            top.extend_from_slice(
                format!(
                    "Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; micalg=pgp-sha256; boundary=\"{b}\"\r\n\r\n--{b}\r\n"
                )
                .as_bytes(),
            );
            top.extend_from_slice(&inner);
            top.extend_from_slice(
                format!(
                    "\r\n--{b}\r\nContent-Type: application/pgp-signature; name=\"signature.asc\"\r\nContent-Disposition: inline; filename=\"signature.asc\"\r\n\r\n"
                )
                .as_bytes(),
            );
            top.extend_from_slice(&sig);
            top.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
        }
        PgpSendMode::EncryptSign => {
            let mut rcpts: Vec<Cert> = m.recipient_certs.to_vec();
            rcpts.push(m.self_cert.clone());
            let armored = encrypt_and_sign(&rcpts, m.signer, &inner)?;
            let b = "s1mailenc";
            top.extend_from_slice(
                format!(
                    "Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"{b}\"\r\n\r\nThis is an OpenPGP/MIME encrypted message (RFC 4880)\r\n\r\n--{b}\r\nContent-Type: application/pgp-encrypted\r\n\r\nVersion: 1\r\n\r\n--{b}\r\nContent-Type: application/octet-stream; name=\"encrypted.asc\"\r\nContent-Disposition: inline; filename=\"encrypted.asc\"\r\n\r\n"
                )
                .as_bytes(),
            );
            top.extend_from_slice(&armored);
            top.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
        }
    }

    // ---- 传输头 + 拼装 ----
    let mut mail: Vec<u8> = Vec::new();
    mail.extend_from_slice(format!("From: {}\r\n", m.from).as_bytes());
    mail.extend_from_slice(format!("To: {}\r\n", m.to.join(", ")).as_bytes());
    if !m.cc.is_empty() {
        mail.extend_from_slice(format!("Cc: {}\r\n", m.cc.join(", ")).as_bytes());
    }
    mail.extend_from_slice(
        format!(
            "Subject: =?UTF-8?B?{}?=\r\n",
            base64_encode(m.subject.as_bytes())
        )
        .as_bytes(),
    );
    mail.extend_from_slice(format!("Date: {}\r\n", rfc2822_now()).as_bytes());
    mail.extend_from_slice(format!("Message-ID: {}\r\n", m.message_id).as_bytes());
    mail.extend_from_slice(b"MIME-Version: 1.0\r\n");
    // top 自带顶层 Content-Type 头 + 空行 + multipart 体
    mail.extend_from_slice(&top);
    Ok(mail)
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// base64 按 76 列折行（MIME 惯例，行尾统一 CRLF）
fn b64_wrap(s: &str) -> String {
    let mut out = String::new();
    for chunk in s.as_bytes().chunks(76) {
        // base64 输出总是 ASCII，from_utf8 理论上不会失败；
        // 保留 unwrap_or 作为防御，即使失败也只是截断该块而非崩溃
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push_str("\r\n");
    }
    out
}

/// 当前时间的 RFC 2822 日期（UTC，+0000）
fn rfc2822_now() -> String {
    // SystemTime 早于 UNIX_EPOCH 只在时钟回拨时发生，回退到 epoch 0（1970-01-01）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (h, mi, s2) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // Howard Hinnant civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let mon = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(mth - 1) as usize];
    format!("{wd}, {d:02} {mon} {y:04} {h:02}:{mi:02}:{s2:02} +0000")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_paths() {
        assert_eq!(expand_tilde("/abs/path.asc"), "/abs/path.asc");
        assert_eq!(expand_tilde("relative.asc"), "relative.asc");
        let h = std::env::var("HOME").unwrap_or_default();
        let expanded = expand_tilde("~/keys.asc");
        assert!(
            expanded.starts_with(&h) && expanded.ends_with("keys.asc"),
            "HOME={h} expanded={expanded}"
        );
    }
    use sequoia_openpgp::serialize::Marshal;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("smail-pgp-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_encrypt_sign_decrypt_verify() {
        let cert = generate_cert("测试 <t@local>").unwrap();
        let plain = b"Content-Type: text/plain; charset=utf-8\r\n\r\n\xE6\x9C\xBA\xE5\xAF\x86\xE6\xAD\xA3\xE6\x96\x87\r\n";
        let armored = encrypt_and_sign(std::slice::from_ref(&cert), &cert, plain).unwrap();
        assert!(String::from_utf8_lossy(&armored).contains("BEGIN PGP MESSAGE"));

        let rt = PgpRuntime {
            secret: Some(cert.clone()),
            passphrase: None,
            pubring: vec![cert.clone()],
            policy: StandardPolicy::new(),
        };
        let (out, status) = rt.decrypt(&armored).unwrap();
        assert_eq!(out, plain);
        assert!(status.contains("已解密"), "{status}");
        assert!(status.contains("签名有效"), "{status}");
    }

    #[test]
    fn detached_sign_verify_and_tamper() {
        let cert = generate_cert("签名者 <s@local>").unwrap();
        let content = b"Content-Type: text/plain; charset=utf-8\r\n\r\n\xE8\xA6\x81\xE7\xAD\xBE\xE5\x90\x8D\xE7\x9A\x84\xE5\x86\x85\xE5\xAE\xB9\r\n";
        let sig = sign_detached(&cert, content).unwrap();
        assert!(String::from_utf8_lossy(&sig).contains("BEGIN PGP SIGNATURE"));

        let rt = PgpRuntime {
            secret: None,
            passphrase: None,
            pubring: vec![cert],
            policy: StandardPolicy::new(),
        };
        assert!(rt.verify_detached(content, &sig).contains("签名有效"));
        // 篡改后必须失败
        let bad = b"Content-Type: text/plain; charset=utf-8\r\n\r\nTAMPERED\r\n";
        assert!(rt.verify_detached(bad, &sig).contains("签名无效"));
    }

    #[test]
    fn detect_and_signed_mime_split() {
        let mail = b"From: a@b\r\nTo: c@d\r\nSubject: s\r\nMIME-Version: 1.0\r\nContent-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=\"XY\"\r\n\r\n--XY\r\nContent-Type: text/plain\r\n\r\nhello world\r\n\r\n--XY\r\nContent-Type: application/pgp-signature\r\n\r\nSIGDATA\r\n--XY--\r\n";
        assert_eq!(detect_pgp_kind(mail), Some(PgpKind::Signed));
        let (content, sig) = signed_mime_parts(mail).unwrap();
        assert_eq!(content, b"Content-Type: text/plain\r\n\r\nhello world\r\n");
        assert_eq!(sig, b"SIGDATA\r\n");

        let enc = b"Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"Z\"\r\n\r\n--Z\r\nContent-Type: application/pgp-encrypted\r\n\r\nVersion: 1\r\n\r\n--Z\r\nContent-Type: application/octet-stream\r\n\r\nCIPHER\r\n--Z--\r\n";
        assert_eq!(detect_pgp_kind(enc), Some(PgpKind::Encrypted));
        assert!(detect_pgp_kind(b"Content-Type: text/plain\r\n\r\nx").is_none());
    }

    #[test]
    fn runtime_load_from_files() {
        let d = tmpdir("load");
        let cert = generate_cert("载入 <l@local>").unwrap();
        let sec = d.join("secret.asc");
        let mut f = std::fs::File::create(&sec).unwrap();
        cert.as_tsk().armored().serialize(&mut f).unwrap();
        let pubf = d.join("pub.asc");
        let mut f = std::fs::File::create(&pubf).unwrap();
        cert.armored().serialize(&mut f).unwrap();

        let cfg = PgpCfg {
            key_path: sec.to_string_lossy().to_string(),
            passphrase: None,
            pubring_path: Some(pubf.to_string_lossy().to_string()),
            wkd: false,
        };
        let rt = PgpRuntime::load(&cfg).unwrap();
        let plain = b"data";
        let armored = encrypt_and_sign(std::slice::from_ref(&cert), &cert, plain).unwrap();
        let (out, st) = rt.decrypt(&armored).unwrap();
        assert_eq!(out, b"data");
        assert!(st.contains("签名有效"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn outgoing_roundtrip_both_modes() {
        let cert = generate_cert("往返 <r@local>").unwrap();
        let to = vec!["r@local".to_string()];
        let cc: Vec<String> = vec![];
        let mid = "<rt@t>";
        // 加密+签名
        let m = OutgoingPgp {
            from: "r@local",
            to: &to,
            cc: &cc,
            subject: "加密往返",
            body: "正文（机密）9527",
            attachments: &[],
            mode: PgpSendMode::EncryptSign,
            signer: &cert,
            recipient_certs: &[],
            self_cert: &cert,
            message_id: mid,
        };
        let mail = build_outgoing(&m).unwrap();
        assert_eq!(detect_pgp_kind(&mail), Some(PgpKind::Encrypted));
        let rt = PgpRuntime {
            secret: Some(cert.clone()),
            passphrase: None,
            pubring: vec![cert.clone()],
            policy: StandardPolicy::new(),
        };
        let ct = extract_pgp_ciphertext(&mail).unwrap();
        let (plain, st) = rt.decrypt(&ct).unwrap();
        assert!(st.contains("已解密") && st.contains("签名有效"));
        let plain_str = String::from_utf8_lossy(&plain);
        assert!(plain_str.contains("Content-Type: text/plain"));
        assert!(plain_str.contains(&base64_encode("正文（机密）9527".as_bytes())));
        // 仅签名
        let to2 = vec!["r@local".to_string()];
        let m2 = OutgoingPgp {
            from: "r@local",
            to: &to2,
            cc: &cc,
            subject: "签名往返",
            body: "明文正文",
            attachments: &[],
            mode: PgpSendMode::Sign,
            signer: &cert,
            recipient_certs: &[],
            self_cert: &cert,
            message_id: mid,
        };
        let mail2 = build_outgoing(&m2).unwrap();
        assert_eq!(detect_pgp_kind(&mail2), Some(PgpKind::Signed));
        let (content, sig) = signed_mime_parts(&mail2).unwrap();
        assert!(rt.verify_detached(&content, &sig).contains("签名有效"));
    }
}
