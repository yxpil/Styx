//! # styx-server — TCP 服务
//!
//! 把 [`styx_core::Kernel`] 暴露成一个常驻的多会话 TCP 服务，
//! 让同一个角色可以被终端、GUI、手机端同时使用（各自独立的会话）。
//!
//! ## 协议
//!
//! 一行一个 JSON（`\n` 定界），请求与响应都是一行：
//!
//! ```text
//! → {"op":"hello","session":"林夏线"}
//! ← {"ok":true,"op":"hello","session":"林夏线","card":"林夏","turn":0,...}
//!
//! → {"op":"say","text":"我想看看那本旧相册。"}
//! ← {"ok":true,"op":"say","turn":1,"reply":{"speech":["不卖。"],...},...}
//!
//! → {"op":"quit"}
//! ← {"ok":true,"op":"quit","bye":true}
//! ```
//!
//! 支持的 op：`ping` `hello` `say` `state` `scene` `set_scene` `events`
//! `tools` `call` `status` `remember` `recall` `associate` `reset` `quit`。
//!
//! 用 JSON-lines 而不是二进制帧，是为了能用 `nc` 直接调试——
//! 调试角色行为时，"肉眼能不能看懂"比"字节效率"重要得多。
//!
//! ```ignore
//! use std::sync::Arc;
//! use styx_server::Server;
//! # fn make_kernel(_s: &str) -> styx_core::Result<styx_core::Kernel> { unimplemented!() }
//!
//! let server = Arc::new(Server::new(Arc::new(|s: &str| make_kernel(s))));
//! server.bind_and_run("127.0.0.1:7879")?;
//! # Ok::<(), styx_core::StyxError>(())
//! ```

pub mod protocol;
pub mod server;

pub use protocol::{Request, Response};
pub use server::{reply_json, ConnectionState, KernelFactory, Server};
