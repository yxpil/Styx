//! # styx-guard — 服务层边界防护
//!
//! 三件事，都是"会被人从外面连"才需要、自己一个人用不需要的：
//!
//! - [`Gate`]：连接准入。一连接一线程的模型下，**没有这道门就是一个
//!   `for i in $(seq 1 10000); do curl …; done` 就能把线程表打满**。
//! - [`Shutdown`]：优雅关闭。默认的 Ctrl+C 是直接杀进程，正在跑的回合
//!   会连半句台词都留不下。
//! - [`Limits`]：配额。**默认全部为"不限"**，个人模式下的行为与从前
//!   完全一致；[`Limits::production`] 给一份可以直接照用的保守档。
//! - [`Token`] / [`Exposure`]：谁能连。前者是常量时间比对的访问令牌，
//!   后者把监听地址分成"本机 / 内网 / 公网"三档——安全上最常见的失误
//!   不是认证写错，是**根本没意识到这个端口是开放的**。
//! - [`fsutil::atomic_write`]：写文件要么是完整的旧值，要么是完整的新值。
//!   `fs::write` 做不到这一点，而 checkpoint 写坏比没写更糟。
//!
//! ## 为什么是独立 crate 而不是塞进 `styx-server`
//!
//! 这三样东西 **TCP 服务和 HTTP 服务都要用**（`styx-web` 复用
//! `styx-server` 的同一个门和同一个关闭信号，这样两个入口的配额
//! 才是"这一个进程"的配额，而不是各算各的）。放哪边都会让另一边
//! 产生反向依赖。
//!
//! ## 为什么不用 `tokio` 的 `Semaphore`
//!
//! 整个网络层是同步的（见 `styx-server` 的线程模型说明）。为了一个
//! 信号量把异步运行时拖进来，是拿一吨钢材去换一颗螺丝。

pub mod auth;
pub mod fsutil;
pub mod gate;
pub mod limits;
pub mod shutdown;

pub use auth::{Exposure, Token};
pub use gate::{Gate, Permit};
pub use limits::Limits;
pub use shutdown::{Shutdown, ShutdownGuard};
