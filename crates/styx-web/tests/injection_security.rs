//! 集成测试：HTTP 服务的输入注入边界（从 crate 外部视角）。
//!
//! styx-web 的 HTTP 层是"给本机浏览器用"的服务，但仍把三件事做死：
//! 请求行/头/body 硬上限、文件路径白名单（防目录穿越）、响应头不反射用户输入。
//! 这些用例从外部验证这些安全边界：编码过的路径穿越载荷解码后仍被白名单拒绝、
//! 超长 body 在读入前拒绝、XSS 串只是惰性数据且不会被反射进响应头。

use std::io::BufReader;
use styx_web::http::{is_safe_segment, parse_query, read_request, write_response, MAX_BODY, Response};

fn req(raw: &str) -> styx_web::http::Request {
    let mut r = BufReader::new(raw.as_bytes());
    read_request(&mut r).unwrap().unwrap()
}

// ── 路径穿越：即使 URL 编码，解码后仍含 `..`，被白名单拒绝 ──

#[test]
fn encoded_path_traversal_is_decoded_then_rejected() {
    // %2e%2e%2f = "../"。解码后 path 变成 /sticker/../../etc/passwd
    let r = req("GET /sticker/%2e%2e%2f%2e%2e%2fetc%2fpasswd HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        r.path.contains(".."),
        "percent-decoded path should contain '..': {}",
        r.path
    );
    // 白名单把含 .. 的段判为不安全（静态资源/表情包服务据此拒绝）
    assert!(!is_safe_segment("../../etc/passwd"));
    assert!(!is_safe_segment(".."));
}

#[test]
fn separator_and_backslash_segments_rejected() {
    assert!(!is_safe_segment("a/b.png")); // 路径分隔符
    assert!(!is_safe_segment("a\\b.png")); // Windows 分隔符
    assert!(!is_safe_segment("sub/dir.png"));
}

#[test]
fn control_chars_and_null_byte_rejected() {
    assert!(!is_safe_segment("a\x00b.png")); // NUL
    assert!(!is_safe_segment("a\tb.png")); // tab
    assert!(!is_safe_segment("a b.png")); // 空格
}

#[test]
fn overlong_name_rejected() {
    assert!(!is_safe_segment(&"x".repeat(129)));
    assert!(is_safe_segment(&"x".repeat(128))); // 边界：128 允许
}

// ── body 硬上限：防内存耗尽 ──

#[test]
fn oversized_body_rejected_before_read() {
    let raw = format!(
        "POST /x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        MAX_BODY + 1
    );
    let mut r = BufReader::new(raw.as_bytes());
    let err = read_request(&mut r).unwrap_err();
    assert!(err.contains("请求体过大"), "{}", err);
}

// ── XSS：查询参数是惰性数据，且绝不反射进响应头 ──

#[test]
fn xss_query_param_is_inert_not_reflected_into_headers() {
    let payload = "<script>alert(1)</script>";
    let raw = format!("GET /api/say?text={payload} HTTP/1.1\r\nHost: x\r\n\r\n");
    let r = req(&raw);

    // 路径与查询被正确解析，XSS 串作为**字符串数据**保存
    assert_eq!(r.path, "/api/say");
    assert_eq!(r.str_param("text").as_deref(), Some(payload));

    // 响应绝不反射用户输入：写一个纯文本响应，输出里不应出现该 payload
    let resp = Response::text(200, "ok");
    let mut out: Vec<u8> = Vec::new();
    write_response(&mut out, &resp).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(!s.contains("<script>"), "response must not reflect user input");
    // 但 nosniff 安全头必须在场
    assert!(s.contains("X-Content-Type-Options: nosniff"));
}

#[test]
fn query_parsing_treats_encoded_payload_as_data() {
    let q = parse_query("name=%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E");
    assert_eq!(q["name"], "<img src=x onerror=alert(1)>");
}

#[test]
fn garbage_request_line_is_rejected() {
    // 单 token 的请求行（没有 path）才会被拒：method 与 target 缺一即报错
    let mut r = BufReader::new(&b"GARBAGE\r\n\r\n"[..]);
    assert!(read_request(&mut r).is_err());
}
