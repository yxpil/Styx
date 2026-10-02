//! 请求 / 响应模型与传输 trait。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{HttpError, Result};

/// 一次 HTTP 请求。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// 方法（大写）。
    pub method: String,
    /// 完整 URL。
    pub url: String,
    /// 附加请求头。
    pub headers: Vec<(String, String)>,
    /// 请求体（UTF-8）。
    pub body: Option<String>,
    /// 超时（连接 + 读写）。
    pub timeout: Duration,
}

impl HttpRequest {
    /// GET。
    pub fn get(url: impl Into<String>) -> Self {
        HttpRequest {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// POST 一段 JSON。
    pub fn post_json(url: impl Into<String>, body: impl Into<String>) -> Self {
        let mut r = HttpRequest {
            method: "POST".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Some(body.into()),
            timeout: Duration::from_secs(60),
        };
        r.headers
            .push(("Content-Type".into(), "application/json".into()));
        r
    }

    /// DELETE。
    pub fn delete(url: impl Into<String>) -> Self {
        HttpRequest {
            method: "DELETE".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// 追加请求头。
    pub fn with_header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    /// 追加 `Authorization: Bearer`。
    pub fn with_bearer(self, token: &str) -> Self {
        if token.is_empty() {
            self
        } else {
            self.with_header("Authorization", format!("Bearer {token}"))
        }
    }

    /// 设置超时。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// 一次 HTTP 响应。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// 状态码。
    pub status: u16,
    /// 响应头（键统一小写）。
    pub headers: BTreeMap<String, String>,
    /// 响应体。
    pub body: String,
}

impl HttpResponse {
    /// 状态码是否 2xx。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 取某个响应头。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }

    /// 解析响应体为 JSON。
    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_str(&self.body).map_err(|e| {
            HttpError::Decode(format!(
                "响应不是合法 JSON：{e}；正文前 200 字节：{}",
                &self.body[..self.body.len().min(200)]
            ))
        })
    }

    /// 2xx 之外一律转成错误（带上正文片段，便于排查）。
    pub fn error_for_status(self) -> Result<Self> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(HttpError::Status {
                code: self.status,
                body: self.body.chars().take(400).collect(),
            })
        }
    }
}

/// HTTP 传输。
///
/// 只有两个实现：[`crate::plain::PlainHttp`]（标准库，明文）与
/// `crate::tls::TlsHttp`（需 `tls` feature）。没有异步运行时。
pub trait HttpTransport: Send + Sync {
    /// 实现名。
    fn name(&self) -> &str;

    /// 发送请求。
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse>;

    /// 是否支持 HTTPS。
    fn supports_https(&self) -> bool {
        false
    }
}

/// 拿到默认传输：有 `tls` feature 时用 `AutoHttp`（按 scheme 分派），
/// 否则退化为纯标准库的明文实现。
pub fn default_transport() -> Arc<dyn HttpTransport> {
    #[cfg(feature = "tls")]
    {
        Arc::new(crate::tls::AutoHttp::new())
    }
    #[cfg(not(feature = "tls"))]
    {
        Arc::new(crate::plain::PlainHttp::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: BTreeMap::new(),
            body: body.to_string(),
        }
    }

    #[test]
    fn builder_sets_headers() {
        let r = HttpRequest::post_json("http://h/x", "{}")
            .with_bearer("sk-1")
            .with_timeout(Duration::from_secs(5));
        assert_eq!(r.method, "POST");
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == "Content-Type" && v == "application/json"));
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer sk-1"));
        assert_eq!(r.timeout.as_secs(), 5);
    }

    #[test]
    fn empty_bearer_is_skipped() {
        let r = HttpRequest::get("http://h/x").with_bearer("");
        assert!(r.headers.is_empty());
    }

    #[test]
    fn status_helpers() {
        assert!(resp(200, "{}").is_success());
        let e = resp(404, "not found").error_for_status().unwrap_err();
        assert!(e.to_string().contains("404"));
        assert!(e.to_string().contains("not found"));
    }

    #[test]
    fn json_decode_error_is_descriptive() {
        let e = resp(200, "<html>oops</html>").json().unwrap_err();
        assert!(e.to_string().contains("合法 JSON"));
    }
}
