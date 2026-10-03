//! URL 解析：只支持 Styx 用得到的那一小撮语法。
//!
//! 刻意不引入 `url` crate——我们需要处理的只有
//! `http(s)://host[:port][/path][?query]`，自己写反而更好控制、更少依赖。

use crate::error::{HttpError, Result};

/// 解析后的 URL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// `http` 或 `https`。
    pub scheme: String,
    /// 主机名（IPv6 会去掉方括号）。
    pub host: String,
    /// 端口（未指定时按 scheme 取默认值）。
    pub port: u16,
    /// 路径 + 查询串，至少为 `/`。
    pub path: String,
}

impl Url {
    /// 解析。
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let (scheme, rest) = match raw.split_once("://") {
            Some((s, r)) => (s.to_lowercase(), r),
            None => return Err(HttpError::InvalidUrl(format!("缺少协议前缀：{raw}"))),
        };
        if scheme != "http" && scheme != "https" {
            return Err(HttpError::InvalidUrl(format!("不支持的协议：{scheme}")));
        }
        let default_port = if scheme == "https" { 443 } else { 80 };

        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(HttpError::InvalidUrl(format!("缺少主机名：{raw}")));
        }

        // 去掉 userinfo（我们不用它，但得容忍）
        let authority = authority
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(authority);

        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            // IPv6 字面量
            match rest.split_once(']') {
                Some((h, tail)) => {
                    let port = match tail.strip_prefix(':') {
                        Some(p) => p
                            .parse::<u16>()
                            .map_err(|_| HttpError::InvalidUrl(format!("端口非法：{p}")))?,
                        None => default_port,
                    };
                    (h.to_string(), port)
                }
                None => {
                    return Err(HttpError::InvalidUrl(format!("IPv6 地址不完整：{raw}")));
                }
            }
        } else {
            match authority.rsplit_once(':') {
                // 有冒号就必须跟一个合法端口：否则 `http://h:abc/` 会被
                // 悄悄当成主机名 `h:abc` 接受，错误被推迟到一个莫名其妙的
                // DNS/连接失败上。
                Some((h, p)) => {
                    if p.is_empty() {
                        return Err(HttpError::InvalidUrl(format!("端口缺失：{raw}")));
                    }
                    if !p.chars().all(|c| c.is_ascii_digit()) {
                        return Err(HttpError::InvalidUrl(format!("端口非法：{p}")));
                    }
                    let port = p
                        .parse::<u16>()
                        .map_err(|_| HttpError::InvalidUrl(format!("端口超出范围：{p}")))?;
                    (h.to_string(), port)
                }
                None => (authority.to_string(), default_port),
            }
        };

        if host.is_empty() {
            return Err(HttpError::InvalidUrl(format!("缺少主机名：{raw}")));
        }

        let path = if path.is_empty() { "/" } else { path };
        Ok(Url {
            scheme,
            host,
            port,
            path: path.to_string(),
        })
    }

    /// `host[:port]` 形式的 Host 头值（非默认端口才带端口）。
    pub fn host_header(&self) -> String {
        let default = if self.scheme == "https" { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// 是否 HTTPS。
    pub fn is_https(&self) -> bool {
        self.scheme == "https"
    }

    /// 拼接一个相对路径，例如 `base.join("/chat/completions")`。
    pub fn join(&self, suffix: &str) -> String {
        let base = self.path.trim_end_matches('/');
        let suffix = suffix.trim_start_matches('/');
        if suffix.is_empty() {
            if base.is_empty() {
                "/".to_string()
            } else {
                base.to_string()
            }
        } else {
            format!("{base}/{suffix}")
        }
    }

    /// 追加查询串。
    pub fn with_query(&self, query: &str) -> String {
        let sep = if self.path.contains('?') { '&' } else { '?' };
        format!("{}{}{}", self.path, sep, query)
    }
}

/// 从完整 URL 里拿到 `(Url, path)`：`path` 已包含查询串。
pub fn split(raw: &str) -> Result<(Url, String)> {
    let url = Url::parse(raw)?;
    let path = url.path.clone();
    Ok((url, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_http() {
        let u = Url::parse("http://127.0.0.1:8080/v1/chat/completions").unwrap();
        assert_eq!(u.scheme, "http");
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/v1/chat/completions");
        assert_eq!(u.host_header(), "127.0.0.1:8080");
        assert!(!u.is_https());
    }

    #[test]
    fn parses_https_default_port() {
        let u = Url::parse("https://api.openai.com/v1").unwrap();
        assert_eq!(u.port, 443);
        assert!(u.is_https());
        assert_eq!(u.host_header(), "api.openai.com");
    }

    #[test]
    fn parses_bare_authority() {
        let u = Url::parse("https://example.com").unwrap();
        assert_eq!(u.path, "/");
        assert_eq!(u.port, 443);
    }

    #[test]
    fn parses_ipv6() {
        let u = Url::parse("http://[::1]:9527/x").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 9527);
    }

    #[test]
    fn strips_userinfo() {
        let u = Url::parse("http://user:pass@host:80/x").unwrap();
        assert_eq!(u.host, "host");
        assert_eq!(u.port, 80);
    }

    #[test]
    fn keeps_query_string() {
        let u = Url::parse("http://h:1/mem/search?q=a&limit=3").unwrap();
        assert_eq!(u.path, "/mem/search?q=a&limit=3");
        assert_eq!(u.with_query("extra=1"), "/mem/search?q=a&limit=3&extra=1");
    }

    #[test]
    fn join_appends_path() {
        let u = Url::parse("https://h/v1/").unwrap();
        assert_eq!(u.join("chat/completions"), "/v1/chat/completions");
        let u = Url::parse("https://h/").unwrap();
        assert_eq!(u.join("/mem"), "/mem");
        assert_eq!(u.join(""), "/");
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(Url::parse("ftp://x/y").is_err());
        assert!(Url::parse("no-scheme").is_err());
        assert!(Url::parse("http://").is_err());
        assert!(Url::parse("http://h:abc/").is_err());
    }
}
