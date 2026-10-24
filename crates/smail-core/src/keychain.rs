//! 系统钥匙串凭据存储
//!
//! ## 职责
//!
//! - **密码安全存储**：密码不落 config.toml 明文
//! - **系统集成**：使用系统钥匙串（macOS Keychain / Linux secret-service）
//! - **透明读取**：启动时自动从钥匙串读取密码替换标记
//!
//! ## 存储方案
//!
//! config.toml 中存标记：
//! ```toml
//! [account.imap]
//! password = "keychain:me@x.com/imap"
//! ```
//!
//! 真实密码存系统钥匙串：
//! - **macOS**：Keychain（`security` CLI）
//! - **Linux**：secret-service（`secret-tool`）
//!
//! ## 运行时流程
//!
//! ```text
//! AccountService::start_for()
//!   ↓
//! 检测 password 是否以 "keychain:" 开头
//!   ↓
//! 调用系统钥匙串 API 获取真实密码
//!   ↓
//! 内存替换（仅服务线程持有，不回写配置文件）
//! ```
//!
//! ## 安全权衡
//!
//! - **macOS 限制**：`security add-generic-password -w` 经参数传密码，写入瞬间可在 `ps` 短暂可见（毫秒级）
//! - **Linux 安全**：`secret-tool` 走 stdin，无此问题
//! - **明文永不回写**：`save_accounts` 原样保留标记

const SERVICE: &str = "s-mail";

/// config 里密码字段的前缀标记
pub const MARKER: &str = "keychain:";

/// 钥匙串账号键：<用户名>/imap 或 <用户名>/smtp（用户名稳定唯一，
/// 不能用 slug——load_accounts 的 slug 去重与索引相关，不稳定）
pub fn account_key(username: &str, kind: &str) -> String {
    format!("{username}/{kind}")
}

pub fn is_marker(s: &str) -> bool {
    s.starts_with(MARKER)
}

/// 密码统一 hex 封装：macOS `security` 对非 ASCII 密码的 `-w` 输出会转十六进制
/// （实测中文密码读回 "73336372..."），任意字符密码必须封装才能稳定往返。
/// 解码失败回退原文（兼容用户手动添加的明文条目）
fn hex_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() * 2 + 4);
    out.push_str("hex:"); // 前缀区分封装条目与手动明文条目，消除纯 hex 密码歧义
    for b in s.as_bytes() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn hex_decode(s: &str) -> Option<String> {
    let s = s.strip_prefix("hex:")?;
    if s.is_empty() {
        return None;
    }
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect::<Option<_>>()?;
    String::from_utf8(bytes).ok()
}

/// 由标记构造密码值（写 config 用）
pub fn marker_for(username: &str, kind: &str) -> String {
    format!("{MARKER}{}", account_key(username, kind))
}

fn run(cmd: &mut std::process::Command) -> Result<std::process::Output, String> {
    cmd.output()
        .map_err(|e| format!("钥匙串工具启动失败（未安装或不在 PATH）：{e}"))
}

/// 存（已存在则更新）。macOS：security；Linux：secret-tool（stdin 传密码）
pub fn store(account: &str, password: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let out = run(std::process::Command::new("security").args([
            "add-generic-password",
            "-U",
            "-s",
            SERVICE,
            "-a",
            account,
            "-w",
            &hex_encode(password),
        ]))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "security 写入失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::io::Write as _;
        let mut child = std::process::Command::new("secret-tool")
            .args([
                "store", "--label", SERVICE, "service", SERVICE, "account", account,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("secret-tool 启动失败（未安装？）：{e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(password.as_bytes());
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("secret-tool 执行失败：{e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "secret-tool 写入失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (account, password);
        Err("该平台暂不支持系统钥匙串".into())
    }
}

/// 读：返回密码。找不到/平台不支持给人话错误（账号将无法连接）
pub fn resolve(marker: &str) -> Result<String, String> {
    let account = marker.strip_prefix(MARKER).unwrap_or(marker);
    #[cfg(target_os = "macos")]
    {
        let out = run(std::process::Command::new("security").args([
            "find-generic-password",
            "-s",
            SERVICE,
            "-a",
            account,
            "-w",
        ]))?;
        if out.status.success() {
            let raw = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            let pw = hex_decode(&raw).unwrap_or(raw);
            if pw.is_empty() {
                Err(format!("钥匙串条目 {account:?} 密码为空"))
            } else {
                Ok(pw)
            }
        } else {
            Err(format!(
                "钥匙串读取失败（条目 {account:?} 不存在或被拒绝）：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let out = run(std::process::Command::new("secret-tool")
            .args(["lookup", "service", SERVICE, "account", account]))?;
        let pw = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        if out.status.success() && !pw.is_empty() {
            Ok(pw)
        } else {
            Err(format!("钥匙串读取失败（条目 {account:?} 不存在）"))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = account;
        Err("该平台暂不支持系统钥匙串".into())
    }
}

/// 删（删账号时清理，防孤儿凭据；不存在不算错）
pub fn delete(account: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let out = run(std::process::Command::new("security").args([
            "delete-generic-password",
            "-s",
            SERVICE,
            "-a",
            account,
        ]))?;
        // 条目不存在（44=item not found）视为已清理
        if out.status.success() || out.status.code() == Some(44) {
            Ok(())
        } else {
            Err(format!(
                "钥匙串条目删除失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let out = run(std::process::Command::new("secret-tool")
            .args(["clear", "service", SERVICE, "account", account]))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("钥匙串条目删除失败"))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = account;
        Err("该平台暂不支持系统钥匙串".into())
    }
}

/// 解析单个密码字段：标记 → 钥匙串读取；明文原样返回
pub fn resolve_password(pw: Option<String>, what: &str) -> Result<Option<String>, String> {
    match pw {
        None => Ok(None),
        Some(s) if is_marker(&s) => Ok(Some(resolve(&s).map_err(|e| format!("{what}：{e}"))?)),
        Some(s) => Ok(Some(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_roundtrip_format() {
        let m = marker_for("me@x.com", "imap");
        assert_eq!(m, "keychain:me@x.com/imap");
        assert!(is_marker(&m));
        assert!(!is_marker("plainpass"));
        assert!(!is_marker(""));
    }

    #[test]
    fn hex_wrap_roundtrip() {
        // 中文/控制字符/纯 hex 样式密码都能稳定往返，且不会歧义解码
        for pw in ["s3cret-密码", "abcd", "6162", "p@ss \u{1}", ""] {
            assert_eq!(
                hex_decode(&hex_encode(pw)).unwrap_or_default(),
                pw,
                "{pw:?}"
            );
        }
        // 无前缀（手动明文条目）不解码，原样返回
        assert_eq!(hex_decode("6162"), None);
    }

    /// 真实钥匙串往返（需交互环境；CI 跳过）：SMAIL_KEYCHAIN_TEST=1 cargo test
    #[test]
    #[ignore]
    fn keychain_store_resolve_delete() {
        let acct = account_key("smail-test@local", "imap");
        store(&acct, "s3cret-密码").expect("store");
        assert_eq!(
            resolve(&marker_for("smail-test@local", "imap")).unwrap(),
            "s3cret-密码"
        );
        delete(&acct).expect("delete");
        assert!(
            resolve(&format!("{MARKER}{acct}")).is_err(),
            "删除后应读不到"
        );
    }
}
