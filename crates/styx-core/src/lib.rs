//! # styx-core — 角色扮演内核
//!
//! Styx 的内核只做一件事：**把一个"角色"在"场景"中随时间演化的状态，
//! 与外部能力（语言模型、长期记忆、联想、共享记忆池、工具）编排成一个回合闭环**。
//!
//! 内核本身**不依赖任何具体后端**，所有外部能力都以端口（trait）形式声明，
//! 由上层按需注入。这带来两个好处：
//!
//! 1. 同一份内核可以跑在 Nebula / MightBe / MemoryPool 组成的完整集群上，
//!    也可以只跑内存兜底实现，完全离线；
//! 2. 换后端不动业务代码，与 Nebula、MightBe 自身的"模块化单体 + trait 边界"
//!    设计哲学保持一致。
//!
//! ## 回合闭环
//!
//! ```text
//!   用户输入
//!      │
//!      ▼
//!  ① 记忆召回 (MemoryPort)       ← Nebula: BM25 / 标签 / 重要度
//!      │
//!      ▼
//!  ② 联想发散 (AssocPort)        ← MightBe: 多跳关联 + 证据 + 置信度
//!      │
//!      ▼
//!  ③ 提示词装配 (PromptBuilder)  ← 角色卡 + 场景 + 状态 + 记忆 + 联想 + 历史
//!      │
//!      ▼
//!  ④ 生成     (LlmPort)          ← OpenAI 兼容 + 多端点加权调度
//!      │
//!      ▼
//!  ⑤ 结构化解析 (Reply)          ← [说]/[做]/[想]/[情]/[关系]/[忆]
//!      │
//!      ▼
//!  ⑥ 状态结算 (DynamicState)     ← 心情/好感/信任/紧张/意图 演化与衰减
//!      │
//!      ▼
//!  ⑦ 落库     (MemoryPort + PoolPort) ← 事件与长期记忆写回
//!      │
//!      ▼
//!   回复 + 回合报告
//! ```

pub mod character;
pub mod error;
pub mod event;
pub mod kernel;
pub mod ports;
pub mod prompt;
pub mod reply;
pub mod scene;
pub mod session;
pub mod state;
pub mod text;

pub use character::{CharacterCard, LoreEntry, Relation};
pub use error::{Result, StyxError};
pub use event::{Event, EventKind};
pub use kernel::{Kernel, KernelConfig, KernelStatus, TurnOutcome};
pub use ports::{
    AssocPort, Association, ChatMessage, Completion, LlmOptions, LlmPort, MemoryNote, MemoryPort,
    PoolPort, PoolStats, Recalled, ToolPort, ToolSpec,
};
pub use prompt::{PromptBudget, PromptBuilder, PromptPlan, PromptReport};
pub use reply::Reply;
pub use scene::Scene;
pub use session::{Audit, Guard, Session};
pub use state::{DynamicState, Mood, StateDelta};
pub use text::{estimate_tokens, keywords, summarize, truncate_to_tokens};

/// 内核版本。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
