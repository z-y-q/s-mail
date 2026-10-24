//! 存储层错误类型
//!
//! 统一 SQLite 操作、FTS5 全文搜索、数据库迁移等错误。
//!
//! ## 错误分类
//!
//! - **数据库级**：连接失败、SQL 语法错误、索引损坏、磁盘满
//! - **业务级**：记录不存在、重试穷尽、文件夹不存在
//!
//! 调用方可根据错误类型区分处理策略（重试 vs 用户提示 vs 回退）。

use thiserror::Error;

/// 存储层错误
#[derive(Error, Debug)]
pub enum StoreError {
    /// 数据库连接失败
    #[error("无法连接数据库：{0}")]
    ConnectionFailed(String),

    /// 数据库初始化失败
    #[error("数据库初始化失败：{0}")]
    InitializationFailed(String),

    /// SQL 执行错误
    #[error("SQL 执行失败：{0}")]
    SqlExecution(String),

    /// 事务错误
    #[error("事务操作失败：{0}")]
    TransactionFailed(String),

    /// FTS5 全文搜索错误
    #[error("全文搜索失败：{0}")]
    FullTextSearchFailed(String),

    /// 数据序列化/反序列化错误
    #[error("数据转换失败：{0}")]
    SerializationFailed(String),

    /// 索引损坏
    #[error("索引文件损坏，建议重建：{0}")]
    CorruptedIndex(String),

    /// 磁盘空间不足
    #[error("磁盘空间不足，无法写入数据")]
    DiskFull,

    /// 通用 I/O 错误
    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),

    // --- 业务逻辑错误 ---
    /// 邮件记录不存在（查询返回空，通常是 hash 错误或已删除）
    #[error("邮件不存在：hash={0}")]
    MessageNotFound(i64),

    /// 草稿不存在（草稿 ID 无效或已删除）
    #[error("草稿不存在：id={0}")]
    DraftNotFound(i64),

    /// 发件箱邮件不存在
    #[error("发件箱邮件不存在：id={0}")]
    OutboxNotFound(i64),

    /// 发件重试次数用尽（已达 20 次上限，标记为 failed）
    #[error("发件重试次数用尽：id={0}（已尝试 {1} 次）")]
    OutboxExhausted(i64, i32),

    /// 文件夹不存在（操作目标文件夹无邮件）
    #[error("文件夹不存在或为空：{0}")]
    FolderNotFound(String),

    /// 联系人不存在
    #[error("联系人不存在：id={0}")]
    ContactNotFound(i64),
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        match err {
            rusqlite::Error::SqliteFailure(ref e, _) if e.code == rusqlite::ErrorCode::DiskFull => {
                StoreError::DiskFull
            }
            rusqlite::Error::SqliteFailure(ref e, _)
                if e.code == rusqlite::ErrorCode::DatabaseCorrupt =>
            {
                StoreError::CorruptedIndex(err.to_string())
            }
            _ => StoreError::SqlExecution(err.to_string()),
        }
    }
}

/// 存储层 Result 类型别名
pub type Result<T> = std::result::Result<T, StoreError>;
