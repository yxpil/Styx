//! 极小的 HTTP/1.1 服务端：够用、可读、零依赖。
//!
//! # 为什么不引 axum / hyper
//!
//! Styx 的内核是**同步**的，服务端也是"一连接一线程"（见 `styx-server`）。
//! 为了让一个本地单人使用的界面起来，把整个 tokio 生态拖进来，
//! 会让 9 个 crate 的工作区多出几十个间接依赖，还得在同步与异步之间
//! 架桥——收益是负数。
//!
//! 而真正需要的能力其实很少：
//!
//! - 读一个请求（请求行 + 头 + 定长 body）；
//! - 回一个响应（状态行 + 头 + body）；
//! - `Connection: close`。
//!
//! 就这样。不做 keep-alive、不做分块传输、不做 HTTP/2——
//! 单机页面上，一次请求新建一个连接的开销完全可以忽略。
//!
//! # 安全边界
//!
//! 这是一个**给本机浏览器用的**服务，默认只绑 `127.0.0.1`。即便如此，
//! 依然把三件事做死：请求行/头/body 各有硬上限（防内存耗尽）、
//! 文件路径只允许白名单内的文件名（防目录穿越）、
//! 响应头里不反射任何用户输入。

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};

/// 请求行上限。
const MAX_LINE: usize = 8 * 1024;
/// 全部请求头上限。
const MAX_HEADERS: usize = 16 * 1024;
/// 请求体上限（1 MiB）：这个界面只发 JSON，用不到更大。
pub const MAX_BODY: usize = 1024 * 1024;

/// 一个已解析的请求。
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// 路径（已百分号解码，不含 query）。
    pub path: String,
    /// 查询参数（已解码，键小写不敏感不去）。
    pub query: BTreeMap<String, String>,
    /// 请求头（键统一小写）。
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    /// 查询参数或 JSON body 里的一个字符串字段。
    ///
    /// 前端有时用 query（`?session=x`）、有时用 body（`{"session":"x"}`），
    /// 让两种写法都能用，比强制一种更省事。
    pub fn str_param(&self, key: &str) -> Option<String> {
        if let Some(v) = self.query.get(key) {
            if !v.is_empty() {
                return Some(v.clone());
            }
        }
        self.json()
            .and_then(|j| j.get(key).and_then(|v| v.as_str()).map(|s| s.to_string()))
    }

    /// 查询参数或 JSON body 里的一个数字字段。
    pub fn num_param(&self, key: &str) -> Option<usize> {
        if let Some(v) = self.query.get(key).and_then(|v| v.parse().ok()) {
            return Some(v);
        }
        self.json()
            .and_then(|j| j.get(key).and_then(|v| v.as_u64()))
            .map(|n| n as usize)
    }

    /// 把 body 当 JSON 解析（解析失败返回 `None`，不报错）。
    pub fn json(&self) -> Option<serde_json::Value> {
        if self.body.is_empty() {
            return None;
        }
        serde_json::from_slice(&self.body).ok()
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// 一个待发送的响应。
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// 额外响应头（`Cache-Control` 之类）。
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Response {
            status,
            content_type: content_type.to_string(),
            body,
            headers: Vec::new(),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Response::new(
            status,
            "text/plain; charset=utf-8",
            body.into().into_bytes(),
        )
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Response::new(status, "application/json; charset=utf-8", body)
    }

    pub fn html(body: &'static str) -> Self {
        Response::new(200, "text/html; charset=utf-8", body.as_bytes().to_vec())
    }

    pub fn asset(content_type: &str, body: &'static str) -> Self {
        let mut r = Response::new(200, content_type, body.as_bytes().to_vec());
        r.headers.push(("Cache-Control".into(), "no-cache".into()));
        r
    }

    pub fn bytes(content_type: &str, body: Vec<u8>, cache_secs: u32) -> Self {
        let mut r = Response::new(200, content_type, body);
        r.headers
            .push(("Cache-Control".into(), format!("max-age={cache_secs}")));
        r
    }

    pub fn not_found(what: &str) -> Self {
        Response::text(404, format!("404 没有这个资源：{what}"))
    }

    pub fn bad_request(msg: &str) -> Self {
        Response::text(400, format!("400 {msg}"))
    }

    pub fn server_error(msg: &str) -> Self {
        Response::text(500, format!("500 {msg}"))
    }

    pub fn with_header(mut self, key: &str, value: impl Into<String>) -> Self {
        self.headers.push((key.to_string(), value.into()));
        self
    }
}

/// 读一个请求。
///
/// 返回 `Ok(None)` 表示对端干净地关掉了连接（不是错误）。
pub fn read_request(r: &mut impl BufRead) -> Result<Option<Request>, String> {
    let Some(line) = read_line(r)? else {
        return Ok(None);
    };
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        // 前导空行：宽容地跳过（有些客户端会多发一个 CRLF）
        let Some(line) = read_line(r)? else {
            return Ok(None);
        };
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            return Ok(None);
        }
        return parse_request(r, &line);
    }
    parse_request(r, line)
}

fn parse_request(r: &mut impl BufRead, line: &str) -> Result<Option<Request>, String> {
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_uppercase();
    let target = parts.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() {
        return Err(format!("请求行不完整：{line}"));
    }

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    // ---- 头 ----
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    let mut total = line.len();
    while let Some(h) = read_line(r)? {
        total += h.len();
        if total > MAX_HEADERS {
            return Err("请求头过大".into());
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    // ---- body ----
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(format!("请求体过大（{len} 字节）"));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut body)
            .map_err(|e| format!("读取请求体失败：{e}"))?;
    }

    Ok(Some(Request {
        method,
        path: percent_decode(raw_path),
        query: parse_query(raw_query),
        headers,
        body,
    }))
}

/// 读一行，并对长度设上限（防止一个不换行的流把内存撑爆）。
fn read_line(r: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut buf: Vec<u8> = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .map_err(|e| format!("读取请求失败：{e}"))?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE {
        return Err("请求行过大".into());
    }
    Ok(Some(String::from_utf8_lossy(&buf).to_string()))
}

/// 写一个响应。
///
/// 一律 `Connection: close`：请求结束就关连接，状态机最简单，
/// 也不会出现"上一个请求没读完、下一个请求从中间开始读"的错位。
pub fn write_response(w: &mut impl Write, resp: &Response) -> std::io::Result<()> {
    let mut head = String::with_capacity(256);
    head.push_str(&format!(
        "HTTP/1.1 {} {}\r\n",
        resp.status,
        reason(resp.status)
    ));
    head.push_str(&format!("Content-Type: {}\r\n", resp.content_type));
    head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    head.push_str("X-Content-Type-Options: nosniff\r\n");
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");

    w.write_all(head.as_bytes())?;
    w.write_all(&resp.body)?;
    w.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// 查询串 → 表。
pub fn parse_query(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let k = percent_decode(k);
        if k.is_empty() {
            continue;
        }
        out.insert(k, percent_decode(v));
    }
    out
}

/// 百分号解码（`+` 视为空格）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 一个安全的路径段：不含分隔符、不含 `..`、不含控制字符。
///
/// 图片文件名直接来自请求，是这里唯一"用户可控且会碰到文件系统"的地方，
/// 所以用**白名单字符集**而不是黑名单。
pub fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// 扩展名 → MIME。
pub fn mime_of(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn req(raw: &str) -> Request {
        let mut r = BufReader::new(raw.as_bytes());
        read_request(&mut r).unwrap().unwrap()
    }

    #[test]
    fn parses_a_get_with_query() {
        let r =
            req("GET /api/state?session=%E6%9E%97%E5%A4%8F&limit=3 HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/api/state");
        assert_eq!(r.query.get("session").unwrap(), "林夏");
        assert_eq!(r.query.get("limit").unwrap(), "3");
        assert_eq!(r.headers.get("host").unwrap(), "x");
        assert!(r.body.is_empty());
    }

    #[test]
    fn parses_a_post_with_a_json_body() {
        let body = r#"{"text":"你好","session":"a"}"#;
        let raw = format!(
            "POST /api/say HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let r = req(&raw);
        assert_eq!(r.method, "POST");
        assert_eq!(r.json().unwrap()["text"], "你好");
        // query 与 body 都能取到参数，且 query 优先
        assert_eq!(r.str_param("session").unwrap(), "a");
        assert_eq!(r.str_param("missing"), None);
    }

    #[test]
    fn body_is_read_exactly_by_content_length() {
        // 一个粘在 body 后面的字节不该被吞进本次请求（否则下一个请求会错位）
        let body = "{\"a\":1}";
        let raw = format!(
            "POST /x HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}EXTRA",
            body.len()
        );
        let mut r = BufReader::new(raw.as_bytes());
        let first = read_request(&mut r).unwrap().unwrap();
        assert_eq!(first.body_string(), body);
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "EXTRA");
    }

    #[test]
    fn tolerates_a_missing_content_length() {
        let r = req("POST /x HTTP/1.1\r\nHost: a\r\n\r\n");
        assert!(r.body.is_empty());
        assert!(r.json().is_none());
    }

    #[test]
    fn rejects_an_oversized_body_before_reading_it() {
        let raw = format!(
            "POST /x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let mut r = BufReader::new(raw.as_bytes());
        let err = read_request(&mut r).unwrap_err();
        assert!(err.contains("请求体过大"), "{err}");
    }

    #[test]
    fn clean_eof_is_none_not_an_error() {
        let mut r = BufReader::new(&b""[..]);
        assert!(read_request(&mut r).unwrap().is_none());
    }

    #[test]
    fn rejects_a_garbage_request_line() {
        let mut r = BufReader::new(&b"GARBAGE\r\n\r\n"[..]);
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn writes_a_well_formed_response() {
        let mut out: Vec<u8> = Vec::new();
        write_response(&mut out, &Response::text(404, "没有")).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(s.contains("Content-Length: 6"));
        assert!(s.contains("Connection: close"));
        assert!(s.ends_with("没有"));
    }

    #[test]
    fn response_json_has_a_stable_content_type() {
        let r = Response::json(200, &serde_json::json!({"ok": true}));
        assert!(r.content_type.starts_with("application/json"));
        assert_eq!(r.body, br#"{"ok":true}"#.to_vec());
    }

    #[test]
    fn path_segments_are_whitelisted() {
        assert!(is_safe_segment("happy_01.png"));
        assert!(is_safe_segment("a-b_c.webp"));
        assert!(!is_safe_segment("../../etc/passwd"));
        assert!(!is_safe_segment("a/b.png"));
        assert!(!is_safe_segment("a\\b.png"));
        assert!(!is_safe_segment(""));
        assert!(!is_safe_segment("a b.png"));
        assert!(!is_safe_segment(&"x".repeat(200)));
    }

    #[test]
    fn mime_lookup_covers_images_and_assets() {
        assert_eq!(mime_of("a.png"), "image/png");
        assert_eq!(mime_of("a.JPG"), "image/jpeg");
        assert_eq!(mime_of("a.js"), "text/javascript; charset=utf-8");
        assert_eq!(mime_of("noext"), "application/octet-stream");
    }

    #[test]
    fn query_decoding_handles_chinese_and_plus() {
        let q = parse_query("name=%E6%9E%97%E5%A4%8F&q=a+b&empty=&flag");
        assert_eq!(q["name"], "林夏");
        assert_eq!(q["q"], "a b");
        assert_eq!(q["empty"], "");
        assert_eq!(q["flag"], "");
    }
}
