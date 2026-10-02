//! 联想层错误。

use styx_core::error::StyxError;
use thiserror::Error;

/// 联想后端错误。
#[derive(Debug, Error)]
pub enum AssocError {
    #[error("连接错误：{0}")]
    Io(String),

    #[error("协议错误：{0}")]
    Protocol(String),

    #[error("MightBe 返回错误 {code}：{message}")]
    Server { code: u16, message: String },

    #[error("配置非法：{0}")]
    Config(String),

    #[error("后端不可用：{0}")]
    Offline(String),

    #[error("{0}")]
    Other(String),
}

/// 结果类型。
pub type Result<T> = std::result::Result<T, AssocError>;

/// 把 [`AssocError`] 转成内核统一错误。
///
/// 放在这里而不是 `mightbe.rs`，是为了让不启用 `mightbe` feature 时
/// 内存共现图实现也能映射端口错误。
impl From<AssocError> for StyxError {
    fn from(e: AssocError) -> Self {
        match e {
            AssocError::Offline(m) => StyxError::PortUnavailable {
                port: "assoc",
                reason: m,
            },
            other => StyxError::Assoc(other.to_string()),
        }
    }
}
