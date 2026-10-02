//! # styx-llm — 语言模型端口
//!
//! 实现 [`styx_core::ports::LlmPort`]，负责"把提示词变成一段文本"。
//!
//! ## 三层结构
//!
//! ```text
//!   LlmPort  ← 内核只看见这个
//!      │
//!   EndpointPool   加权轮询 + 熔断 + 半开试探 + 故障转移（纯逻辑，可单测）
//!      │
//!   ChatBackend    真正的传输：OpenAiBackend（HTTP）/ MockBackend（离线）
//! ```
//!
//! 把**调度**与**传输**分开，是为了让调度逻辑能被彻底测试——
//! 注入一个假后端就能精确验证"哪个端点在什么时刻被选中、熔断何时打开、
//! 恢复后是否回归"，全程不联网、不 `sleep`。
//!
//! ## 为什么只实现 OpenAI 兼容协议
//!
//! 因为它已经事实上是通用语：官方 OpenAI、DeepSeek、通义千问 compatible-mode、
//! Moonshot、Groq、本机 Ollama / LM Studio / vLLM / one-api 全都说这一套。
//! 端点之间的差异（地址、密钥、模型名、权重）都在 [`Endpoint`] 里，不在协议里。
//!
//! ```ignore
//! use std::sync::Arc;
//! use styx_llm::{Endpoint, EndpointPool, OpenAiBackend};
//!
//! let pool = EndpointPool::new(
//!     vec![
//!         Endpoint::openai("gpt-4o-mini", "sk-…").with_weight(3),
//!         Endpoint::ollama("qwen2.5:14b").with_weight(1),
//!     ],
//!     Arc::new(OpenAiBackend::new()),
//! )?;
//! let stats = pool.stats();
//! for s in stats {
//!     println!("{}", s.render());
//! }
//! # Ok::<(), styx_core::StyxError>(())
//! ```

pub mod backend;
pub mod endpoint;
pub mod error;
pub mod mock;
pub mod pool;
pub mod scheduler;

#[cfg(feature = "http")]
pub mod openai;

pub use backend::ChatBackend;
pub use endpoint::{from_env, Endpoint};
pub use error::{LlmError, Result};
pub use mock::{offline_llm, offline_pool, MockBackend};
pub use pool::EndpointPool;
pub use scheduler::{EndpointStat, Scheduler, SchedulerSummary};

#[cfg(feature = "http")]
pub use openai::{build_body, extract_error_message, parse_chat_response, OpenAiBackend};
