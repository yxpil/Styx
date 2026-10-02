//! # styx-pool — 共享记忆池端口
//!
//! 实现 [`styx_core::ports::PoolPort`]，给**多个 agent 之间**提供一块共享黑板。
//!
//! | 实现 | 用途 | 存储 |
//! |---|---|---|
//! | [`MemoryPool`] | 生产：对接 [MemoryPool](https://github.com/yxpil/MemoryPool) | JSONL（`~/.memorypool/memories.jsonl`），HTTP / BIT Remote / MCP |
//! | [`LocalPool`] | 兜底 / 测试 | 进程内，BM25 检索 |
//!
//! ## 为什么它和长期记忆是两个端口
//!
//! 因为它们的**生命周期与可见性不同**：
//!
//! - 长期记忆属于**一个角色**，随角色成长而累积，可加密、可私有；
//! - 共享记忆池属于**一个协作现场**，多个 agent（BIT、Styx、以及任何实现了
//!   BIT Remote 协议的程序）读写同一份事实。
//!
//! 把二者混为一谈，最后一定会出现"某个角色的私密往事被别的 agent 读到了"。
//!
//! ```ignore
//! use styx_pool::{LocalPool, PoolConfig, MemoryPool};
//! use styx_core::ports::{MemoryNote, PoolPort};
//!
//! // 联网版
//! let pool = MemoryPool::new(PoolConfig::default());
//! if pool.health() {
//!     pool.remember(&MemoryNote::new("陈默今晚在城南"))?;
//! }
//!
//! // 离线兜底
//! let local = LocalPool::new();
//! local.remember(&MemoryNote::new("陈默今晚在城南"))?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod error;
pub mod local;

#[cfg(feature = "http")]
pub mod client;

pub use error::{PoolError, Result};
pub use local::LocalPool;

#[cfg(feature = "http")]
pub use client::{json_to_recalled_list, percent_encode, MemoryPool, PoolConfig};
