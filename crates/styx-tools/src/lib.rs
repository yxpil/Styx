//! # styx-tools — 工具端口
//!
//! 让角色能"作用于世界"，而不只是说话。
//!
//! ## 三层来源
//!
//! | 来源 | 模块 | 说明 |
//! |---|---|---|
//! | 内置 | [`builtin`] | 时钟、骰子、随机挑选、记忆检索、联想、写共享记忆 |
//! | MCP | [`mcp::McpToolPort`] | Streamable HTTP + JSON-RPC 2.0 |
//! | BIT Remote | [`mcp::BitRemoteToolPort`] | `{"params": {...}}` 信封 |
//!
//! 多个来源用 [`ChainedTools`] 串成一个 [`ToolPort`] 交给内核——
//! 内核完全不需要知道某个工具来自哪里。
//!
//! ## 一条设计原则
//!
//! [`builtin::register_builtins`] 只会注册**当前真的能工作**的工具：
//! 没有记忆后端时不注册 `recall`。让模型看见一个必然失败的工具，
//! 比看不见它更糟——它会尝试、会失败、然后在后续回合里反复尝试。
//!
//! ```ignore
//! use styx_tools::{register_builtins, ToolRegistry};
//!
//! let mut registry = ToolRegistry::new();
//! register_builtins(&mut registry, None, None, None);
//! let port = registry.into_port();
//! println!("{}", port.status());
//! ```

pub mod builtin;
pub mod registry;

#[cfg(feature = "http")]
pub mod mcp;

pub use builtin::{
    register_builtins, now_millis, AssociateTool, ClockTool, DiceTool, NoteTool, PickTool,
    RecallTool, SmallRng,
};
pub use registry::{ChainedTools, Tool, ToolRegistry};

#[cfg(feature = "http")]
pub use mcp::{BitRemoteToolPort, McpClient, McpToolPort};
