//! 纯标准库的 HTTP/1.1 客户端。
//!
//! 只做 Styx 需要的事：发一个请求、收一个响应。**没有异步运行时、
//! 没有连接池、没有重定向、没有 gzip**。这不是偷懒，而是刻意的取舍：
//!
//! - Styx 的每一次 HTTP 调用都是"一个请求换一个完整结果"（非流式），
//!   连接复用收益很小；
//! - 我们不发送 `Accept-Encoding: gzip`，服务端就不会压缩，
//!   从而免掉一个解压依赖；
//! - 少一层依赖，就少一处离线构建时踩空的地方。
//!
//! 支持 `Content-Length`、`Transfer-Encoding: chunked`、以及"读到 EOF"
//! 三种响应体定界方式（最后一种用于 HTTP/1.0 风格的服务端）。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::{HttpError, Result};
use crate::request::{HttpRequest, HttpResponse, HttpTransport};
use crate::url::Url;

/// 明文 HTTP 传输。
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainHttp;

impl PlainHttp {
    pub fn new() -> Self {
        PlainHttp
    }
}

impl HttpTransport for PlainHttp {
    fn name(&self) -> &str {
        "std-http"
    }

    fn send(&self, req: &HttpRequest) -> Result<HttpResponse> {
        let url = Url::parse(&req.url)?;
        if url.is_https() {
            return Err(HttpError::NoTls(req.url.clone()));
        }
        let mut stream = connect(&url.host, url.port, req.timeout)?;
        stream.set_read_timeout(Some(req.timeout)).ok();
        stream.set_write_timeout(Some(req.timeout)).ok();
        stream.set_nodelay(true).ok();

        write_request(&mut stream, req, &url)?;
        stream.flush().ok();
        read_response(stream)
    }
}

fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| HttpError::Io(format!("无法解析 {host}:{port} — {e}")))?;
    let mut last_err: Option<String> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(format!("{addr} — {e}")),
        }
    }
    Err(match last_err {
        Some(e) => HttpError::Io(format!("连接 {host}:{port} 失败：{e}")),
        None => HttpError::Io(format!("{host}:{port} 没有可用的地址")),
    })
}

fn write_request(stream: &mut TcpStream, req: &HttpRequest, url: &Url) -> Result<()> {
    // 只有调用方**没有**自己指定这几个头时才用我们的默认值，
    // Content-Length 例外——它必须与实际字节数一致，一律由我们补。
    let has = |name: &str| {
        req.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    };

    let mut head = String::with_capacity(256);
    head.push_str(&format!("{} {} HTTP/1.1\r\n", req.method, url.path));
    if !has("host") {
        head.push_str(&format!("Host: {}\r\n", url.host_header()));
    }
    if !has("user-agent") {
        head.push_str("User-Agent: styx-http/0.1\r\n");
    }
    if !has("accept") {
        head.push_str("Accept: application/json, text/plain, */*\r\n");
    }
    if !has("connection") {
        head.push_str("Connection: close\r\n");
    }
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }

    match req.body.as_ref().map(|b| b.as_bytes()) {
        Some(b) => {
            head.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
            stream
                .write_all(head.as_bytes())
                .map_err(|e| HttpError::Io(format!("写请求头失败：{e}")))?;
            stream
                .write_all(b)
                .map_err(|e| HttpError::Io(format!("写请求体失败：{e}")))?;
        }
        None => {
            head.push_str("\r\n");
            stream
                .write_all(head.as_bytes())
                .map_err(|e| HttpError::Io(format!("写请求失败：{e}")))?;
        }
    }
    Ok(())
}

fn read_response(stream: TcpStream) -> Result<HttpResponse> {
    let mut reader = BufReader::new(stream);

    // ---- 状态行 ----
    let mut line = String::new();
    match reader.read_line(&mut line) {
        // 干净 EOF：对端什么都没发
        Ok(0) => return Err(HttpError::Io("服务端没有返回任何数据".into())),
        Ok(_) => {}
        // 一个字节都没收到就断了，语义上同样是「没有返回任何数据」。
        //
        // 这条分支主要是给 Windows 的：对端若带着未读数据关闭连接，
        // 内核会发 RST 而不是 FIN，读到的就是 ECONNRESET（os error 10054）。
        // 把裸 OS 错误抛给上层，排查时得先猜「是谁 reset 了我」；
        // 归一成同一句话，再把原因附在括号里。
        Err(e) if line.is_empty() => {
            return Err(HttpError::Io(format!("服务端没有返回任何数据（{e}）")));
        }
        Err(e) => return Err(HttpError::Io(format!("读状态行失败：{e}"))),
    }
    let status = parse_status(&line)?;

    // ---- 响应头 ----
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut l = String::new();
        let n = reader
            .read_line(&mut l)
            .map_err(|e| HttpError::Io(format!("读响应头失败：{e}")))?;
        if n == 0 {
            break;
        }
        let trimmed = l.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    // ---- 响应体 ----
    // 204 / 304 / HEAD 无实体
    let no_body = status == 204 || status == 304 || (100..200).contains(&status);
    let body = if no_body {
        String::new()
    } else if headers
        .get("transfer-encoding")
        .map(|v| v.to_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        let bytes = read_chunked(&mut reader)?;
        String::from_utf8_lossy(&bytes).to_string()
    } else if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| HttpError::Io(format!("读响应体失败（期望 {len} 字节）：{e}")))?;
        String::from_utf8_lossy(&buf).to_string()
    } else {
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(|e| HttpError::Io(format!("读响应体失败：{e}")))?;
        String::from_utf8_lossy(&buf).to_string()
    };

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn parse_status(line: &str) -> Result<u16> {
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(HttpError::Decode(format!("状态行无法识别：{line:?}")));
    }
    parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Decode(format!("状态码无法识别：{line:?}")))
}

/// 解析 `Transfer-Encoding: chunked` 的响应体，并消费尾部 trailer。
fn read_chunked(reader: &mut BufReader<TcpStream>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        let n = reader
            .read_line(&mut size_line)
            .map_err(|e| HttpError::Io(format!("读分块长度失败：{e}")))?;
        if n == 0 {
            return Err(HttpError::Io("分块体在结束前被截断".into()));
        }
        let size_token = size_line.trim_end_matches(['\r', '\n']);
        // 允许 `1a;ext=1` 这种带扩展的形式
        let size_hex = size_token.split(';').next().unwrap_or("").trim();
        if size_hex.is_empty() {
            continue;
        }
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| HttpError::Decode(format!("分块长度非法：{size_hex:?}")))?;
        if size == 0 {
            // 读掉 trailer，直到空行
            loop {
                let mut t = String::new();
                let n = reader
                    .read_line(&mut t)
                    .map_err(|e| HttpError::Io(format!("读 trailer 失败：{e}")))?;
                if n == 0 || t.trim_end_matches(['\r', '\n']).is_empty() {
                    break;
                }
            }
            return Ok(out);
        }
        let mut buf = vec![0u8; size];
        reader
            .read_exact(&mut buf)
            .map_err(|e| HttpError::Io(format!("读分块数据失败：{e}")))?;
        out.extend_from_slice(&buf);

        // 分块后必须是 CRLF
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|e| HttpError::Io(format!("分块结尾缺失：{e}")))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader as StdBufReader;
    use std::net::TcpListener;

    /// 起一个只回一段固定响应的假服务端。
    fn serve_once(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = StdBufReader::new(stream.try_clone().unwrap());

            // ---- 请求头：按行读，直到空行 ----
            let mut request = String::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line
                    .to_lowercase()
                    .trim_end()
                    .strip_prefix("content-length:")
                {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                request.push_str(&line);
            }

            // ---- 请求体：按 Content-Length 精确读 ----
            // 这里**不能**用 read_line：客户端不会给 body 补换行，read_line 会
            // 一直等到对端关闭或超时——而客户端正等着响应，于是双方互等到
            // TCP 超时（实测每次挂满 60 秒）。
            if content_length > 0 {
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body);
                request.push_str("\r\n");
                request.push_str(&String::from_utf8_lossy(&body));
            }

            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            request
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn posts_json_and_reads_content_length_body() {
        let (base, handle) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
        );
        let resp = PlainHttp::new()
            .send(&HttpRequest::post_json(format!("{base}/v1/x"), "{\"a\":1}"))
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "{\"ok\":true}");
        assert_eq!(resp.header("content-type"), Some("application/json"));
        assert_eq!(resp.json().unwrap()["ok"], true);

        let req = handle.join().unwrap();
        assert!(req.starts_with("POST /v1/x HTTP/1.1"), "got {req}");
        assert!(req.to_lowercase().contains("host: 127.0.0.1"));
        assert!(req.to_lowercase().contains("content-length: 7"));
    }

    #[test]
    fn reads_chunked_body() {
        let (base, handle) = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        );
        let resp = PlainHttp::new()
            .send(&HttpRequest::get(format!("{base}/x")))
            .unwrap();
        assert_eq!(resp.body, "hello world");
        let _ = handle.join();
    }

    #[test]
    fn reads_body_until_eof_without_length() {
        let (base, handle) = serve_once("HTTP/1.0 200 OK\r\n\r\nno-length-body");
        let resp = PlainHttp::new()
            .send(&HttpRequest::get(format!("{base}/x")))
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "no-length-body");
        let _ = handle.join();
    }

    #[test]
    fn propagates_non_2xx_as_error_when_asked() {
        let (base, handle) =
            serve_once("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 7\r\n\r\noffline");
        let resp = PlainHttp::new()
            .send(&HttpRequest::get(format!("{base}/x")))
            .unwrap();
        assert_eq!(resp.status, 503);
        let err = resp.error_for_status().unwrap_err();
        assert!(err.to_string().contains("503"));
        let _ = handle.join();
    }

    #[test]
    fn https_is_rejected_with_a_helpful_message() {
        let err = PlainHttp::new()
            .send(&HttpRequest::get("https://example.com/x"))
            .unwrap_err();
        assert!(err.to_string().contains("不支持 HTTPS"));
        assert!(err.to_string().contains("features tls"));
    }

    #[test]
    fn empty_response_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            drop(s);
        });
        let err = PlainHttp::new()
            .send(&HttpRequest::get(format!("http://{addr}/x")))
            .unwrap_err();
        assert!(
            err.to_string().contains("没有返回任何数据"),
            "实际错误：{err}"
        );
        let _ = h.join();
    }

    #[test]
    fn parses_status_lines() {
        assert_eq!(parse_status("HTTP/1.1 200 OK\r\n").unwrap(), 200);
        assert_eq!(parse_status("HTTP/1.0 404 Not Found").unwrap(), 404);
        assert!(parse_status("garbage").is_err());
    }
}
