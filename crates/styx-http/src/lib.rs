//! # styx-http — 极小 HTTP 客户端
//!
//! 给 Styx 的 LLM 客户端与 MemoryPool 客户端提供统一、无异步的运行时的 HTTP 能力。
//!
//! ## 设计取舍
//!
//! | 取舍 | 原因 |
//! |---|---|
//! | 不用 reqwest / hyper / tokio | 内核是同步的，异步运行时在这里只增加体积与构建时间 |
//! | 默认不带 gzip | 不发送 `Accept-Encoding`，免掉解压依赖 |
//! | HTTPS 走 `tls` feature | 只用本机 `http://` 模型服务的人不必编译 TLS 栈 |
//! | 自己写 URL 解析 | 只需要 `http(s)://host[:port][/path]` 这一小撮语法 |
//!
//! ```ignore
//! use styx_http::{default_transport, HttpRequest};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let t = default_transport();
//! let resp = t.send(&HttpRequest::get("http://127.0.0.1:8751/health"))?;
//! assert!(resp.is_success());
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod plain;
pub mod request;
#[cfg(feature = "tls")]
pub mod tls;
pub mod url;

pub use error::{HttpError, Result};
pub use plain::PlainHttp;
pub use request::{default_transport, HttpRequest, HttpResponse, HttpTransport};
pub use url::Url;

#[cfg(feature = "tls")]
pub use tls::AutoHttp;
