//! 核心层错误类型
//!
//! 统一配置加载、会话管理、PGP 加密、OAuth 认证等核心功能错误。

use thiserror::Error;

/// 核心层错误
#[derive(Error, Debug)]
pub enum CoreError {
    /// 配置文件不存在
    #[error("配置文件不存在：{0}")]
    ConfigMissing(std::path::PathBuf),

    /// 配置解析失败
    #[error("配置文件格式错误：{0}")]
    ConfigParseFailed(String),

    /// 配置验证失败
    #[error("配置验证失败：{0}")]
    ConfigInvalid(String),

    /// 钥匙串访问失败
    #[error("钥匙串访问失败：{0}")]
    KeychainFailed(String),

    /// OAuth 令牌无效
    #[error("OAuth 令牌无效或已过期")]
    OAuthTokenInvalid,

    /// OAuth 刷新失败
    #[error("OAuth 令牌刷新失败：{0}")]
    OAuthRefreshFailed(String),

    /// OAuth 设备流超时
    #[error("OAuth 授权超时，请重试")]
    OAuthTimeout,

    /// PGP 密钥错误
    #[error("PGP 密钥错误：{0}")]
    PgpKeyError(String),

    /// PGP 加密失败
    #[error("加密失败：{0}")]
    PgpEncryptionFailed(String),

    /// PGP 解密失败
    #[error("解密失败：{0}")]
    PgpDecryptionFailed(String),

    /// PGP 签名失败
    #[error("签名失败：{0}")]
    PgpSignatureFailed(String),

    /// PGP 签名验证失败
    #[error("签名验证失败：{0}")]
    PgpVerificationFailed(String),

    /// WKD 公钥发现失败
    #[error("无法通过 WKD 查找 {0} 的公钥")]
    WkdLookupFailed(String),

    /// HTML 转换失败
    #[error("HTML 转换失败：{0}")]
    HtmlConversionFailed(String),

    /// 邮件解析失败
    #[error("邮件解析失败：{0}")]
    MailParseFailed(String),

    /// 会话线程错误
    #[error("会话线程错误：{0}")]
    SessionThreadFailed(String),

    /// 后端错误传播
    #[error(transparent)]
    Backend(#[from] smail_backend::BackendError),

    /// 存储错误传播
    #[error(transparent)]
    Store(#[from] smail_store::StoreError),

    /// I/O 错误
    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),

    /// 通用错误
    #[error("{0}")]
    Other(String),
}

/// 核心层 Result 类型别名
pub type Result<T> = std::result::Result<T, CoreError>;

/// 启动错误（保留兼容性，映射到 CoreError）
#[derive(Error, Debug)]
pub enum StartError {
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl From<std::io::Error> for StartError {
    fn from(e: std::io::Error) -> Self {
        StartError::Core(CoreError::Io(e))
    }
}
