//! 统一错误类型。

use thiserror::Error;

/// Styx 统一错误。
#[derive(Debug, Error)]
pub enum StyxError {
    #[error("端口 `{port}` 不可用：{reason}")]
    PortUnavailable { port: &'static str, reason: String },

    #[error("语言模型调用失败：{0}")]
    Llm(String),

    #[error("语言模型返回了空内容")]
    EmptyCompletion,

    #[error("长期记忆后端错误：{0}")]
    Memory(String),

    #[error("联想后端错误：{0}")]
    Assoc(String),

    #[error("共享记忆池错误：{0}")]
    Pool(String),

    #[error("工具 `{0}` 执行失败：{1}")]
    Tool(String, String),

    #[error("未知工具：{0}")]
    UnknownTool(String),

    #[error("工具参数非法：{0}")]
    ToolArgs(String),

    #[error("角色卡非法：{0}")]
    InvalidCharacter(String),

    #[error("配置非法：{0}")]
    InvalidConfig(String),

    #[error("网络协议错误：{0}")]
    Protocol(String),

    #[error("认证失败：{0}")]
    Auth(String),

    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 错误：{0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl StyxError {
    /// 便捷构造一个"某端口不可用"错误。
    pub fn unavailable(port: &'static str, reason: impl Into<String>) -> Self {
        StyxError::PortUnavailable {
            port,
            reason: reason.into(),
        }
    }

    /// 是否为"可降级"错误：true 表示上层可以回退到兜底实现继续跑。
    pub fn is_degradable(&self) -> bool {
        matches!(
            self,
            StyxError::PortUnavailable { .. }
                | StyxError::Memory(_)
                | StyxError::Assoc(_)
                | StyxError::Pool(_)
                | StyxError::Io(_)
        )
    }
}

/// 统一结果类型。
pub type Result<T> = std::result::Result<T, StyxError>;
