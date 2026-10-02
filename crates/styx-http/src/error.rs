//! HTTP 错误。

use thiserror::Error;

/// HTTP 层错误。
#[derive(Debug, Error)]
pub enum HttpError {
    #[error("URL 非法：{0}")]
    InvalidUrl(String),

    #[error("网络 IO 错误：{0}")]
    Io(String),

    #[error("请求超时：{0}")]
    Timeout(String),

    #[error("TLS 错误：{0}")]
    Tls(String),

    #[error("服务端返回 {code}：{body}")]
    Status { code: u16, body: String },

    #[error("响应解析失败：{0}")]
    Decode(String),

    #[error("该传输不支持 HTTPS（{0}）：请以 `--features tls` 重新构建，或改用 http:// 端点")]
    NoTls(String),

    #[error("{0}")]
    Other(String),
}

/// 结果类型。
pub type Result<T> = std::result::Result<T, HttpError>;
