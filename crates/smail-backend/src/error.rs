//! 后端错误类型
//!
//! 统一 IMAP/SMTP 协议错误、网络连接错误、认证失败等。

use thiserror::Error;

/// 后端错误
#[derive(Error, Debug)]
pub enum BackendError {
    /// 网络连接失败
    #[error("无法连接到服务器 {host}：{reason}")]
    ConnectionFailed { host: String, reason: String },

    /// 认证失败
    #[error("认证失败：{0}")]
    AuthenticationFailed(String),

    /// TLS/SSL 错误
    #[error("安全连接失败：{0}")]
    TlsFailed(String),

    /// IMAP 协议错误
    #[error("IMAP 协议错误：{0}")]
    ImapProtocol(String),

    /// SMTP 协议错误
    #[error("SMTP 协议错误：{0}")]
    SmtpProtocol(String),

    /// 邮件格式错误
    #[error("邮件格式错误：{0}")]
    InvalidMailFormat(String),

    /// 文件夹不存在
    #[error("文件夹不存在：{0}")]
    FolderNotFound(String),

    /// 邮件不存在
    #[error("邮件不存在：UID {0}")]
    MessageNotFound(u32),

    /// 操作超时
    #[error("操作超时（{0}秒）")]
    Timeout(u64),

    /// 服务器容量限制
    #[error("超出服务器限制：{0}")]
    QuotaExceeded(String),

    /// 配置错误
    #[error("配置错误：{0}")]
    ConfigError(String),

    /// 通用后端错误
    #[error("{0}")]
    Other(String),
}

/// 后端 Result 类型别名
pub type Result<T> = std::result::Result<T, BackendError>;

impl From<melib::Error> for BackendError {
    fn from(err: melib::Error) -> Self {
        use melib::ErrorKind;
        match err.kind {
            ErrorKind::Authentication => BackendError::AuthenticationFailed(err.to_string()),
            ErrorKind::Bug => BackendError::Other(format!("melib bug: {err}")),
            ErrorKind::Network(_) => BackendError::ConnectionFailed {
                host: "未知".to_string(),
                reason: err.to_string(),
            },
            ErrorKind::TimedOut => BackendError::Timeout(30),
            ErrorKind::Configuration => BackendError::ConfigError(err.to_string()),
            _ => BackendError::Other(err.to_string()),
        }
    }
}
