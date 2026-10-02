//! HTTPS 传输（`tls` feature）。
//!
//! 底层用 `ureq`（rustls 后端，纯 Rust，无系统依赖）。
//! 之所以不自己写 TLS——那是另一件事，不该由 Styx 承担；
//! 之所以不把 `ureq` 设成必需依赖——因为只用本机 `http://` 模型服务的人
//! 不应该被迫编译一整个 TLS 栈。

use std::time::Duration;

use crate::error::{HttpError, Result};
use crate::plain::PlainHttp;
use crate::request::{HttpRequest, HttpResponse, HttpTransport};
use crate::url::Url;

/// 同时支持 http / https 的传输：按 scheme 分派。
///
/// - `http://` → [`PlainHttp`]（零依赖路径）
/// - `https://` → ureq + rustls
pub struct AutoHttp {
    plain: PlainHttp,
    agent: ureq::Agent,
}

impl AutoHttp {
    /// 新建。
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .user_agent("styx-http/0.1")
            .build();
        AutoHttp {
            plain: PlainHttp::new(),
            agent,
        }
    }
}

impl Default for AutoHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport for AutoHttp {
    fn name(&self) -> &str {
        "auto-http"
    }

    fn supports_https(&self) -> bool {
        true
    }

    fn send(&self, req: &HttpRequest) -> Result<HttpResponse> {
        let url = Url::parse(&req.url)?;
        if !url.is_https() {
            return self.plain.send(req);
        }

        let mut builder = self
            .agent
            .request(&req.method, &req.url)
            .timeout(req.timeout);
        for (k, v) in &req.headers {
            builder = builder.set(k, v);
        }
        let result = match &req.body {
            Some(body) => builder.send_string(body),
            None => builder.call(),
        };

        match result {
            Ok(resp) => {
                let status = resp.status();
                let mut headers = std::collections::BTreeMap::new();
                for name in resp.headers_names() {
                    if let Some(v) = resp.header(&name) {
                        headers.insert(name.to_lowercase(), v.to_string());
                    }
                }
                let body = resp
                    .into_string()
                    .map_err(|e| HttpError::Io(format!("读取 HTTPS 响应失败：{e}")))?;
                Ok(HttpResponse {
                    status,
                    headers,
                    body,
                })
            }
            Err(ureq::Error::Status(code, resp)) => {
                // ureq 把 4xx/5xx 当错误返回；这里还原成正常响应，
                // 由上层用 `error_for_status()` 统一决定怎么处理。
                let body = resp.into_string().unwrap_or_default();
                let mut headers = std::collections::BTreeMap::new();
                headers.insert("content-type".into(), "text/plain".into());
                Ok(HttpResponse {
                    status: code,
                    headers,
                    body,
                })
            }
            Err(ureq::Error::Transport(t)) => Err(if t.kind() == ureq::ErrorKind::Dns {
                HttpError::Io(format!("DNS 解析失败：{t}"))
            } else {
                HttpError::Tls(format!("{t}"))
            }),
        }
    }
}
