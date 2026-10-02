//! 语言模型层错误。

use thiserror::Error;

/// LLM 层错误。
#[derive(Debug, Error)]
pub enum LlmError {
    #[error("所有端点都失败了，最后一次错误：{last}")]
    AllEndpointsFailed { last: String },

    #[error("没有可用的端点（{} 个配置端点，全部被熔断或禁用）", total)]
    NoEndpoint { total: usize },

    #[error("端点 `{endpoint}` HTTP 错误：{message}")]
    Http { endpoint: String, message: String },

    #[error("端点 `{endpoint}` 返回 {status}：{body}")]
    Status {
        endpoint: String,
        status: u16,
        body: String,
    },

    #[error("响应解析失败：{0}")]
    Decode(String),

    #[error("模型返回了空内容（端点 {endpoint}）")]
    EmptyCompletion { endpoint: String },

    #[error("配置非法：{0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

/// 结果类型。
pub type Result<T> = std::result::Result<T, LlmError>;
