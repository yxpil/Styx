//! # styx-assoc — 联想端口
//!
//! 实现 [`styx_core::ports::AssocPort`]，让角色能"从一个词想到另一个词"。
//!
//! ## 两个实现，一条端口
//!
//! | 实现 | 用途 | 依据 |
//! |---|---|---|
//! | [`MightBeAssoc`] | 生产：对接 [MightBe](https://github.com/yxpil/MightBe) | 词向量 + 多跳推理图，带证据与置信度 |
//! | [`InMemoryAssoc`] | 兜底 / 测试 | 共现图 + PMI + 多跳扩展 |
//!
//! 两者语义对齐的三点，也是这个端口存在的意义：
//!
//! 1. **带证据**：每条联想都说得出"从哪儿来的"；
//! 2. **带置信度**：模型和内核都能判断这条联想有多可信；
//! 3. **可弃判（abstain）**：没把握就返回空——角色"想不起来"，
//!    比编造一段似是而非的记忆安全得多。
//!
//! ```ignore
//! use styx_assoc::{InMemoryAssoc, AssocPort};
//!
//! let a = InMemoryAssoc::new();
//! a.observe("母亲留下了一张旧照片");
//! a.observe("那张旧照片被夹在相册里");
//! let hits = a.associate("照片", 3)?;
//! assert!(hits.iter().any(|h| h.word == "相册"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod error;
pub mod graph;

#[cfg(feature = "mightbe")]
pub mod mightbe;
#[cfg(feature = "mightbe")]
pub mod wire;

pub use error::{AssocError, Result};
pub use graph::{GraphParams, InMemoryAssoc};

#[cfg(feature = "mightbe")]
pub use mightbe::{map_rows, MightBeAssoc, MightBeConfig};
#[cfg(feature = "mightbe")]
pub use wire::{read_reply, MightBeClient, Reply};

/// 兜底实现共用的分词器（定义在 `styx-core`）。
pub use styx_core::text::tokenize;
