//! # styx-memory — 长期记忆端口
//!
//! 实现 [`styx_core::ports::MemoryPort`]，把角色的"记得住、想得起"落到真实的存储层。
//!
//! ## 两个实现，一条端口
//!
//! | 实现 | 用途 | 存储 |
//! |---|---|---|
//! | [`NebulaMemory`] | 生产：对接 [Nebula](https://github.com/yxpil/Nebula) 服务 | 加密单文件 `.ndb`，BM25 + `RELATED` |
//! | [`InMemoryMemory`] | 兜底 / 测试：零依赖，真 BM25 + 共现联想 | 进程内存，退出即失 |
//!
//! 上层不需要区分两者——它们满足同一个 trait。这正是"协议适配器优先"
//! 策略的价值：Nebula 没启动时，角色依然能演，只是记性短一点。
//!
//! ## 与 Nebula 的互通程度
//!
//! [`wire`] 模块复刻了 Nebula 的 v2 有线协议（`NEBULA2` 握手、
//! Argon2id + HKDF + HMAC 挑战应答、ChaCha20-Poly1305 加密帧），
//! [`crypto`] 与之逐位对齐。因此 Styx 是 Nebula 服务的一个**原生客户端**，
//! 不是"另起一套存储"。
//!
//! ```ignore
//! use styx_memory::{InMemoryMemory, MemoryPort};
//! use styx_core::ports::MemoryNote;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mem = InMemoryMemory::new();
//! mem.remember(&MemoryNote::new("母亲留下了一张旧照片").with_importance(0.9))?;
//! let hits = mem.recall("照片", 3)?;
//! assert_eq!(hits.len(), 1);
//! # Ok(())
//! # }
//! ```
//!
//! ## 环境变量
//!
//! 与 Nebula 官方 CLI 保持一致，便于复用同一套凭据：
//!
//! | 变量 | 含义 |
//! |---|---|
//! | `NEBULA_ADDR` | 服务地址，默认 `127.0.0.1:7878` |
//! | `NEBULA_USER` | 用户名，默认 `admin` |
//! | `NEBULA_PASSWORD` | 密码 |

pub mod error;
pub mod mem;
pub mod schema;

#[cfg(feature = "nebula")]
pub mod crypto;
#[cfg(feature = "nebula")]
pub mod nebula;
#[cfg(feature = "nebula")]
pub mod wire;

pub use error::{MemoryError, Result};
pub use mem::{Bm25Params, InMemoryMemory};
pub use schema::{
    delete_sql, insert_sql, keyword_sql, recent_sql, related_sql, rows_to_recalled, search_sql,
    DEFAULT_DB,
};
/// 兜底实现与共现图共用的分词器（定义在 `styx-core`）。
pub use styx_core::text::tokenize;

#[cfg(feature = "nebula")]
pub use crypto::{
    derive_master_key, hkdf_sha256, hmac_sha256, open, seal, KEY_LEN, NONCE_LEN, SALT_LEN,
};
#[cfg(feature = "nebula")]
pub use nebula::{parse_inserted_id, NebulaConfig, NebulaMemory};
#[cfg(feature = "nebula")]
pub use wire::{NebulaClient, Request, Response};
