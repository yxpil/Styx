//! 记忆层错误。

use styx_core::error::StyxError;
use thiserror::Error;

/// 长期记忆后端错误。
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("连接错误：{0}")]
    Io(String),

    #[error("协议错误：{0}")]
    Protocol(String),

    #[error("认证失败：{0}")]
    Auth(String),

    #[error("SQL 执行失败：{0}")]
    Sql(String),

    #[error("配置非法：{0}")]
    Config(String),

    #[error("后端不可用：{0}")]
    Offline(String),

    #[error("JSON 错误：{0}")]
    Json(String),

    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for MemoryError {
    fn from(e: std::io::Error) -> Self {
        MemoryError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for MemoryError {
    fn from(e: serde_json::Error) -> Self {
        MemoryError::Json(e.to_string())
    }
}

/// 结果类型。
pub type Result<T> = std::result::Result<T, MemoryError>;

/// 把 [`MemoryError`] 转成内核统一错误。
///
/// 放在这里而不是 `nebula.rs`，是为了让**不启用 `nebula` feature** 时
/// 内存兜底实现也能把端口错误映射成 [`StyxError`]。
impl From<MemoryError> for StyxError {
    fn from(e: MemoryError) -> Self {
        match e {
            MemoryError::Offline(m) => StyxError::PortUnavailable {
                port: "memory",
                reason: m,
            },
            other => StyxError::Memory(other.to_string()),
        }
    }
}
