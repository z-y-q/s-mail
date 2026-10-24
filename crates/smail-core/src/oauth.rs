//! OAuth 2.0 设备流认证
//!
//! ## 职责
//!
//! - **设备流授权**（RFC 8628）：Google/Microsoft/自定义端点
//! - **令牌管理**：access_token 自动刷新 + 持久化存储
//! - **IMAP/SMTP 集成**：生成 XOAUTH2 SASL 认证串
//!
//! ## 存储策略
//!
//! - 位置：`<数据目录>/oauth.json`（Unix 0600 权限）
//! - 性质：可重建状态（类似 SQLite 缓存，丢失重新授权即可）
//! - 内容：access_token + refresh_token + 过期时间
//!
//! ## 协议适配
//!
//! ### IMAP（melib）
//! ```toml
//! use_oauth2 = true
//! server_password = "<base64(user=...\x01auth=Bearer ...)>"
//! ```
//! melib 解码后作为 `AUTHENTICATE XOAUTH2` 的初始响应
//!
//! ### SMTP（lettre）
//! ```text
//! Mechanism::Xoauth2
//! secret = access_token  // 裸令牌，lettre 自己拼 SASL
//! ```
//!
//! ## 刷新策略
//!
//! - **触发条件**：令牌剩余寿命 < [`REFRESH_MARGIN_SECS`]（120秒）
//! - **主动刷新**：Session 主循环在临期前重建连接触发
//! - **透明更新**：刷新成功后更新 oauth.json，下次启动直接可用

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// access token 剩余寿命低于该秒数时触发刷新
pub const REFRESH_MARGIN_SECS: i64 = 120;

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct OAuthCfg {
    /// google | microsoft | custom
    pub provider: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// microsoft 租户（consumers/organizations/common），默认 common
    #[serde(default)]
    pub tenant: Option<String>,
    /// custom：设备码端点
    #[serde(default)]
    pub device_endpoint: Option<String>,
    /// custom：token 端点
    #[serde(default)]
    pub token_endpoint: Option<String>,
}

impl OAuthCfg {
    /// (device 端点, token 端点)
    pub fn endpoints(&self) -> Result<(String, String), String> {
        match self.provider.as_str() {
            "google" => Ok((
                "https://oauth2.googleapis.com/device/code".into(),
                "https://oauth2.googleapis.com/token".into(),
            )),
            "microsoft" => {
                let t = self.tenant.as_deref().unwrap_or("common");
                Ok((
                    format!("https://login.microsoftonline.com/{t}/oauth2/v2.0/devicecode"),
                    format!("https://login.microsoftonline.com/{t}/oauth2/v2.0/token"),
                ))
            }
            "custom" => {
                let d = self
                    .device_endpoint
                    .clone()
                    .ok_or("custom provider 需要 device_endpoint")?;
                let t = self
                    .token_endpoint
                    .clone()
                    .ok_or("custom provider 需要 token_endpoint")?;
                Ok((d, t))
            }
            other => Err(format!(
                "未知 oauth provider {other:?}（支持 google/microsoft/custom）"
            )),
        }
    }

    /// 微软 v2 端点必须显式带 offline_access 才发 refresh_token
    fn effective_scopes(&self) -> String {
        let mut s = self.scopes.join(" ");
        if self.provider == "microsoft" && !s.contains("offline_access") {
            s = format!("offline_access {s}");
        }
        s
    }

    fn form_common(&self) -> Vec<(String, String)> {
        let mut v = vec![("client_id".to_string(), self.client_id.clone())];
        if let Some(sec) = &self.client_secret {
            v.push(("client_secret".to_string(), sec.clone()));
        }
        v
    }
}

// ---------------------------------------------------------------------------
// token 存取
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// unix 秒
    pub expires_at: i64,
}

pub fn token_path(dir: &Path) -> PathBuf {
    dir.join("oauth.json")
}

pub fn load_tokens(dir: &Path) -> Option<TokenSet> {
    let s = std::fs::read_to_string(token_path(dir)).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save_tokens(dir: &Path, ts: &TokenSet) -> Result<(), String> {
    let p = token_path(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("建 token 目录失败：{e}"))?;
    // 创建即 0600（与 config.toml 同策略；File::create 默认 0644 会让 refresh_token
    // 在 chmod 前有短暂 world-readable 窗口）
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&p)
            .map_err(|e| format!("写 token 失败：{e}"))?
    };
    #[cfg(not(unix))]
    let mut f = std::fs::File::create(&p).map_err(|e| format!("写 token 失败：{e}"))?;
    let body = serde_json::to_string_pretty(ts).map_err(|e| format!("序列化 token 失败：{e}"))?;
    f.write_all(body.as_bytes())
        .map_err(|e| format!("写 token 失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 兑底：收紧历史版本以默认权限创建过的文件（OpenOptions 的 mode 只在新建时生效）
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// SASL
// ---------------------------------------------------------------------------

/// melib 侧 IMAP 认证串：base64("user=..\x01auth=Bearer ..\x01\x01")
pub fn sasl_b64(username: &str, token: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .encode(format!("user={username}\x01auth=Bearer {token}\x01\x01"))
}

// ---------------------------------------------------------------------------
// 设备流
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DeviceAuth {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval: u64,
    pub expires_at: i64,
}

#[derive(Debug)]
pub enum PollOutcome {
    Granted(TokenSet),
    Pending,
    SlowDown,
    Denied(String),
}

#[derive(serde::Deserialize)]
struct DevResp {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_uri")]
    verification_url: String,
    #[serde(default = "dflt_interval")]
    interval: u64,
    #[serde(default = "dflt_expires")]
    expires_in: i64,
}
fn dflt_interval() -> u64 {
    5
}
fn dflt_expires() -> i64 {
    1800
}

pub fn device_start(cfg: &OAuthCfg) -> Result<DeviceAuth, String> {
    let (dev_url, _) = cfg.endpoints()?;
    let mut form = cfg.form_common();
    form.push(("scope".into(), cfg.effective_scopes()));
    let form_refs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let resp = ureq::post(&dev_url)
        .timeout(Duration::from_secs(15))
        .send_form(&form_refs)
        .map_err(|e| format!("设备流启动失败: {e}"))?;
    let r: DevResp = resp
        .into_json()
        .map_err(|e| format!("设备流响应解析失败: {e}"))?;
    Ok(DeviceAuth {
        device_code: r.device_code,
        user_code: r.user_code,
        verification_url: r.verification_url,
        interval: r.interval.max(1),
        expires_at: unix_now() + r.expires_in,
    })
}

fn parse_tokens(v: serde_json::Value) -> Result<TokenSet, String> {
    let access_token = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .ok_or("token 响应缺 access_token")?
        .to_string();
    let refresh_token = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let expires_in = v.get("expires_in").and_then(|x| x.as_i64()).unwrap_or(3600);
    // 留 30s 时钟偏差余量
    Ok(TokenSet {
        access_token,
        refresh_token,
        expires_at: unix_now() + expires_in - 30,
    })
}

/// 设备码轮询一次
pub fn device_poll(cfg: &OAuthCfg, device_code: &str) -> Result<PollOutcome, String> {
    let (_, token_url) = cfg.endpoints()?;
    let mut form = cfg.form_common();
    form.push((
        "grant_type".into(),
        "urn:ietf:params:oauth:grant-type:device_code".into(),
    ));
    form.push(("device_code".into(), device_code.to_string()));
    poll_or_refresh(cfg, &token_url, form)
}

/// refresh_token 换新 access token
pub fn refresh(cfg: &OAuthCfg, refresh_token: &str) -> Result<TokenSet, String> {
    let (_, token_url) = cfg.endpoints()?;
    let mut form = cfg.form_common();
    form.push(("grant_type".into(), "refresh_token".into()));
    form.push(("refresh_token".into(), refresh_token.to_string()));
    match poll_or_refresh(cfg, &token_url, form)? {
        PollOutcome::Granted(ts) => Ok(ts),
        PollOutcome::Denied(e) => Err(format!("刷新被拒: {e}")),
        _ => unreachable!("refresh grant 不会返回 pending/slow_down"),
    }
}

fn poll_or_refresh(
    _cfg: &OAuthCfg,
    url: &str,
    form: Vec<(String, String)>,
) -> Result<PollOutcome, String> {
    let form_refs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let resp = match ureq::post(url)
        .timeout(Duration::from_secs(15))
        .send_form(&form_refs)
    {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r, // 4xx 带 JSON error 体，读出来分类
        Err(e) => return Err(format!("token 端点请求失败: {e}")),
    };
    let ok = resp.status() == 200;
    let v: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("token 响应解析失败: {e}"))?;
    if ok {
        return Ok(PollOutcome::Granted(parse_tokens(v)?));
    }
    match v.get("error").and_then(|e| e.as_str()).unwrap_or("") {
        "authorization_pending" => Ok(PollOutcome::Pending),
        "slow_down" => Ok(PollOutcome::SlowDown),
        other => Ok(PollOutcome::Denied(other.to_string())),
    }
}

// ---------------------------------------------------------------------------
// 取 token（带刷新）
// ---------------------------------------------------------------------------

/// 取有效 access token；临期自动刷新并落盘。未授权时提示先跑 `s-mail auth`。
pub fn ensure_token(cfg: &OAuthCfg, dir: &Path) -> Result<String, String> {
    let mut ts = load_tokens(dir)
        .ok_or_else(|| "尚未授权：请先在终端运行 `s-mail auth <账号名>`".to_string())?;
    if ts.expires_at - unix_now() < REFRESH_MARGIN_SECS {
        let rt = ts
            .refresh_token
            .clone()
            .ok_or_else(|| "token 已过期且没有 refresh_token，请重新授权".to_string())?;
        ts = refresh(cfg, &rt).map_err(|e| format!("刷新 token 失败: {e}"))?;
        save_tokens(dir, &ts)?;
    }
    Ok(ts.access_token)
}

/// 启动时快速检查：仅验证 token 存在，不刷新（避免阻塞启动）。
/// 返回当前 token（可能已过期），连接失败时由重连机制触发真正的刷新。
pub fn try_token_no_refresh(dir: &Path) -> Result<String, String> {
    let ts = load_tokens(dir)
        .ok_or_else(|| "尚未授权：请先在终端运行 `s-mail auth <账号名>`".to_string())?;
    Ok(ts.access_token)
}

// ---------------------------------------------------------------------------
// 交互式授权（CLI / e2e）
// ---------------------------------------------------------------------------

/// 走完整个设备流并落盘。`open_browser` 仅 CLI 用（e2e 关掉）。
pub fn authorize(cfg: &OAuthCfg, dir: &Path, open_browser: bool) -> Result<TokenSet, String> {
    authorize_with(cfg, dir, open_browser, &mut |da| {
        println!("── 浏览器打开: {}", da.verification_url);
        println!("── 输入代码: {}", da.user_code);
    })
}

/// 设备流全程：`show` 在拿到 URL/验证码时回调（GUI 发事件、CLI 打印），
/// 随后阻塞轮询直至授权/失败（账号专用线程里跑，无碍其他账号）
pub fn authorize_with(
    cfg: &OAuthCfg,
    dir: &Path,
    open_browser: bool,
    show: &mut dyn FnMut(&DeviceAuth),
) -> Result<TokenSet, String> {
    authorize_check(cfg, dir, open_browser, show, &|| false)
}

/// 同上，但每轮轮询前检查中断标志（服务线程 shutdown 时置位，避免 Session
/// 重建/退出时 join 卡到设备码过期——Google 真实有效期 15 分钟）
pub fn authorize_check(
    cfg: &OAuthCfg,
    dir: &Path,
    open_browser: bool,
    show: &mut dyn FnMut(&DeviceAuth),
    should_stop: &dyn Fn() -> bool,
) -> Result<TokenSet, String> {
    let da = device_start(cfg)?;
    show(&da);
    if open_browser {
        open_url(&da.verification_url);
    }
    let deadline =
        SystemTime::now() + Duration::from_secs((da.expires_at - unix_now()).max(1) as u64);
    let mut interval = da.interval;
    loop {
        // 中断粒度 = 一个轮询间隔（mock 1s / Google 5s，join 等待可接受）
        for _ in 0..interval {
            if should_stop() {
                return Err("授权已取消（会话关闭）".into());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        match device_poll(cfg, &da.device_code)? {
            PollOutcome::Granted(ts) => {
                save_tokens(dir, &ts)?;
                return Ok(ts);
            }
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval += 5,
            PollOutcome::Denied(e) => return Err(format!("授权被拒: {e}")),
        }
        if SystemTime::now() > deadline {
            return Err("设备码已过期，请重试".into());
        }
    }
}

pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("smail-oauth-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sasl_b64_wire_format() {
        use base64::Engine as _;
        let b = sasl_b64("user@example.com", "TOKEN123");
        let raw = base64::engine::general_purpose::STANDARD.decode(b).unwrap();
        assert_eq!(
            raw,
            b"user=user@example.com\x01auth=Bearer TOKEN123\x01\x01"
        );
    }

    #[test]
    fn endpoints_three_providers() {
        let mut c = OAuthCfg {
            provider: "google".into(),
            client_id: "x".into(),
            client_secret: None,
            scopes: vec![],
            tenant: None,
            device_endpoint: None,
            token_endpoint: None,
        };
        assert!(c.endpoints().unwrap().0.contains("googleapis.com"));
        c.provider = "microsoft".into();
        let (d, t) = c.endpoints().unwrap();
        assert!(d.contains("common") && t.contains("common"));
        c.tenant = Some("consumers".into());
        assert!(c.endpoints().unwrap().0.contains("consumers"));
        c.provider = "custom".into();
        assert!(c.endpoints().is_err());
        c.device_endpoint = Some("http://127.0.0.1:1/devicecode".into());
        assert!(c.endpoints().is_err());
        c.token_endpoint = Some("http://127.0.0.1:1/token".into());
        assert!(c.endpoints().is_ok());
    }

    #[test]
    fn microsoft_scope_injects_offline_access() {
        let c = OAuthCfg {
            provider: "microsoft".into(),
            client_id: "x".into(),
            client_secret: None,
            scopes: vec!["https://outlook.office365.com/IMAP.AccessAsUser.All".into()],
            tenant: None,
            device_endpoint: None,
            token_endpoint: None,
        };
        assert!(c.effective_scopes().starts_with("offline_access "));
        let g = OAuthCfg {
            provider: "google".into(),
            ..c
        };
        assert!(!g.effective_scopes().contains("offline_access"));
    }

    #[test]
    fn token_store_roundtrip_and_permissions() {
        let d = tmpdir("store");
        assert!(load_tokens(&d).is_none());
        let ts = TokenSet {
            access_token: "a1".into(),
            refresh_token: Some("r1".into()),
            expires_at: unix_now() + 3600,
        };
        save_tokens(&d, &ts).unwrap();
        let back = load_tokens(&d).unwrap();
        assert_eq!(back.access_token, "a1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_path(&d))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ensure_token_needs_store_and_skips_fresh() {
        let d = tmpdir("ensure");
        let c = OAuthCfg {
            provider: "custom".into(),
            client_id: "x".into(),
            client_secret: None,
            scopes: vec![],
            tenant: None,
            device_endpoint: Some("http://127.0.0.1:1/d".into()),
            token_endpoint: Some("http://127.0.0.1:1/t".into()),
        };
        let err = ensure_token(&c, &d).unwrap_err();
        assert!(err.contains("s-mail auth"), "{err}");
        // 未过期：直接返回，不碰网络（端点不可达也必须成功）
        save_tokens(
            &d,
            &TokenSet {
                access_token: "fresh".into(),
                refresh_token: Some("r".into()),
                expires_at: unix_now() + 3600,
            },
        )
        .unwrap();
        assert_eq!(ensure_token(&c, &d).unwrap(), "fresh");
        let _ = std::fs::remove_dir_all(&d);
    }
}
