//! 共享记忆池错误。

use styx_core::error::StyxError;
use thiserror::Error;

/// 记忆池错误。
#[derive(Debug, Error)]
pub enum PoolError {
    #[error("HTTP 错误：{0}")]
    Http(String),

    #[error("响应解析失败：{0}")]
    Decode(String),

    #[error("服务端返回 {status}：{body}")]
    Status { status: u16, body: String },

    #[error("配置非法：{0}")]
    Config(String),

    #[error("后端不可用：{0}")]
    Offline(String),

    #[error("{0}")]
    Other(String),
}

/// 结果类型。
pub type Result<T> = std::result::Result<T, PoolError>;

/// 把 [`PoolError`] 转成内核统一错误。
impl From<PoolError> for StyxError {
    fn from(e: PoolError) -> Self {
        match e {
            PoolError::Offline(m) => StyxError::PortUnavailable {
                port: "pool",
                reason: m,
            },
            other => StyxError::Pool(other.to_string()),
        }
    }
}
