//! 真实 HTTP 集成测试：起一个真的监听端口，用真的套接字发请求。
//!
//! 这里刻意**不 mock 传输层**。上一次把 MightBe 的行协议写成桩之后
//! 学到一件事：协议层最容易出的错（少一个字段、读 body 的方式不对、
//! 路径校验漏了）恰好都是桩会替你盖住的那些。所以这一层只认真实字节。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

use styx_core::{CharacterCard, Kernel, KernelConfig, Scene, StickerCatalog};

// ------------------------------------------------------------------ 夹具

/// 一个 1×1 的合法 PNG（67 字节）。
fn png_1px() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG 魔数
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR 长度 + 类型
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1×1
        0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, // 8bit RGBA
        0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, // IDAT
        0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
        0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82, // IEND
    ]
}

/// 建一个临时目录。
///
/// ## 为什么不用 `std::env::temp_dir()`
///
/// 它在 Windows 上指 `%TEMP%`，而这个位置的写入经常被安全策略或杀毒软件拦掉。
/// 本项目就撞上过：`std::fs::write` 报 `Os { code: 5, PermissionDenied }`，
/// 6 个用例集体挂掉——看起来像代码 bug，其实是环境。
///
/// `CARGO_TARGET_TMPDIR` 是 Cargo 专为集成测试准备的目录，位于 `target/` 里面。
/// 那个位置一定是可写的（不然编译都过不去），也一定排在 `.gitignore` 里。
///
/// ## 唯一性必须用一个**原子计数器**，不能只靠毫秒时间戳
///
/// 测试是并行跑的，同一个进程里几个用例可能在**同一毫秒**里各要一个目录。
/// 早先靠 `now_millis()` 区分，于是两个用例拿到同一个路径、同时往里写——
/// Windows 上这报的是 `Os { code: 5, PermissionDenied }`（共享冲突），
/// 而且**只在并行时偶发**，单独跑那个用例永远复现不出来。
fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let mut p = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    p.push(format!(
        "styx-web-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("建临时目录");
    p
}

fn offline_kernel(_session: &str, catalog: Arc<StickerCatalog>) -> styx_core::Result<Kernel> {
    let mut card = CharacterCard::new("林夏");
    card.persona = "外冷内热的旧书店主".into();
    card.speech_style = "短句。".into();
    card.boundaries = vec!["绝不承认自己害怕孤独".into()];

    Kernel::builder(card, Scene::new("拾光旧书店"))
        .llm(styx_llm::offline_llm())
        .memory(Arc::new(styx_memory::InMemoryMemory::new()))
        .assoc(Arc::new(styx_assoc::InMemoryAssoc::new()))
        .stickers(catalog)
        .config(KernelConfig::default())
        .build()
}

/// 起一个真的服务，返回地址。
///
/// 夹具目录的 tag 特意叫 `stickers` 而不是 `fixture`：这个目录名会被
/// `/api/bootstrap` 原样报给前端（用户要知道往哪儿放图），
/// 而 `bootstrap_carries_the_card_state_and_sticker_catalog` 会断言
/// 报出来的是**同一个**目录。名字叫 `fixture` 的话，那条断言从写下来
/// 那天起就没被执行到过（当时前面一行就先把测试干掉了），一直没人发现。
fn start() -> SocketAddr {
    let dir = temp_dir("stickers");
    std::fs::write(dir.join("happy_01.png"), png_1px()).unwrap();
    std::fs::write(dir.join("cry_03.png"), png_1px()).unwrap();
    // 目录里混进非图片文件是常态，它不该出现在表情包面板里
    std::fs::write(dir.join("README.md"), "# 这不是表情包").unwrap();

    let catalog = styx_web::load_catalog(&dir);
    assert_eq!(catalog.len(), 2);

    let server = Arc::new(styx_server::Server::new(Arc::new(move |s: &str| {
        offline_kernel(s, Arc::clone(&catalog))
    })));
    let web = Arc::new(styx_web::WebServer::new(
        server,
        styx_web::load_catalog(&dir),
        &dir,
    ));

    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定端口");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = web.serve(listener);
    });
    addr
}

/// 发出一个请求，读回 (状态码, 响应头, body)。
fn call(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).expect("连接");
    s.set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .ok();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    s.write_all(req.as_bytes()).unwrap();
    s.flush().unwrap();

    let mut r = BufReader::new(s);
    let mut status_line = String::new();
    r.read_line(&mut status_line).unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("状态行")
        .parse()
        .expect("状态码");

    let mut headers = String::new();
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap() == 0 {
            break;
        }
        headers.push_str(&line);
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).unwrap();
    (status, headers, buf)
}

fn get_json(addr: SocketAddr, path: &str) -> serde_json::Value {
    let (status, _, body) = call(addr, "GET", path, None);
    assert_eq!(status, 200, "{path}");
    serde_json::from_slice(&body).expect("JSON 响应")
}

fn post_json(addr: SocketAddr, path: &str, body: &str) -> (u16, serde_json::Value) {
    let (status, _, buf) = call(addr, "POST", path, Some(body));
    let v = serde_json::from_slice(&buf).unwrap_or(serde_json::Value::Null);
    (status, v)
}

// ------------------------------------------------------------------ 静态

#[test]
fn serves_the_single_page_and_its_assets() {
    let addr = start();

    let (status, headers, body) = call(addr, "GET", "/", None);
    assert_eq!(status, 200);
    assert!(headers.to_lowercase().contains("text/html"));
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("<title>Styx"));
    assert!(html.contains("id=\"stage\""));

    let (status, headers, css) = call(addr, "GET", "/app.css", None);
    assert_eq!(status, 200);
    assert!(headers.to_lowercase().contains("text/css"));
    assert!(String::from_utf8(css).unwrap().contains("--bg"));

    let (status, _, js) = call(addr, "GET", "/app.js", None);
    assert_eq!(status, 200);
    assert!(String::from_utf8(js).unwrap().contains("/api/bootstrap"));

    let (status, _, _) = call(addr, "GET", "/favicon.ico", None);
    assert_eq!(status, 204);
}

// ------------------------------------------------------------- bootstrap

#[test]
fn bootstrap_carries_the_card_state_and_sticker_catalog() {
    let addr = start();
    let b = get_json(addr, "/api/bootstrap?session=test-boot");

    assert_eq!(b["ok"], true);
    assert_eq!(b["hello"]["card"], "林夏");
    assert_eq!(b["session"], "test-boot");
    assert!(b["state"]["mood"].is_object());
    assert!(b["scene"]["location"].as_str().unwrap().contains("拾光"));

    let stickers = b["stickers"]["stickers"].as_array().unwrap();
    assert_eq!(stickers.len(), 2, "非图片文件不该进目录");
    assert_eq!(stickers[0]["id"], "happy_01");
    assert_eq!(stickers[0]["label"], "开心");
    assert_eq!(stickers[0]["url"], "/stickers/happy_01.png");
    // 图片地址必须是前端能直接用的相对路径，目录要报给用户看
    assert!(b["stickers"]["dir"].as_str().unwrap().contains("sticker"));
}

// ------------------------------------------------------------------ 回合

#[test]
fn say_advances_the_turn_over_http() {
    let addr = start();
    let (status, v) = post_json(
        addr,
        "/api/say",
        r#"{"text":"我想看看那本旧相册。","session":"test-say"}"#,
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["turn"], 1);
    assert!(!v["plain"].as_str().unwrap().is_empty());
    assert!(v["prompt"].as_str().unwrap().contains("tok"));
    assert!(v["state"]["mood"].is_object());

    // 会话确实被推进了
    let st = get_json(addr, "/api/state?session=test-say");
    assert_eq!(st["turn"], 1);
}

#[test]
fn say_without_text_is_a_400_not_a_crash() {
    let addr = start();
    let (status, _) = post_json(addr, "/api/say", r#"{"session":"test-empty"}"#);
    assert_eq!(status, 400);

    let (status, _) = post_json(addr, "/api/say", r#"{"text":"   ","session":"test-empty"}"#);
    assert_eq!(status, 400);

    // 服务本身还活着
    let v = get_json(addr, "/api/status?session=test-empty");
    assert_eq!(v["ok"], true);
}

#[test]
fn sending_a_sticker_runs_a_turn_and_records_its_id() {
    let addr = start();
    let (status, v) = post_json(
        addr,
        "/api/sticker",
        r#"{"id":"cry_03","session":"test-sticker"}"#,
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["turn"], 1);

    // 关键回归：表情包必须以 *用户输入* 进事件流，且 meta 里带编号，
    // 否则前端刷新后只能把图渲染成一句转述。
    let ev = get_json(addr, "/api/events?limit=50&session=test-sticker");
    let events = ev["events"].as_array().unwrap();
    let input = events
        .iter()
        .find(|e| e["kind"] == "user_input")
        .expect("应当有一条用户输入事件");
    assert_eq!(input["meta"]["sticker_id"], "cry_03");
    assert!(input["text"].as_str().unwrap().contains("发来一张表情包"));
}

#[test]
fn an_unknown_sticker_id_is_reported_without_killing_the_session() {
    let addr = start();
    let (status, v) = post_json(
        addr,
        "/api/sticker",
        r#"{"id":"nope_99","session":"test-bad-sticker"}"#,
    );
    assert_eq!(status, 400);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("没有这张表情包"));

    // 同一个会话还能继续
    let (status, v) = post_json(
        addr,
        "/api/say",
        r#"{"text":"在吗","session":"test-bad-sticker"}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(v["turn"], 1);
}

#[test]
fn sessions_do_not_share_a_transcript() {
    let addr = start();
    post_json(addr, "/api/say", r#"{"text":"甲说的话","session":"alice"}"#);
    post_json(addr, "/api/say", r#"{"text":"乙说的话","session":"bob"}"#);

    let a = get_json(addr, "/api/state?session=alice");
    let b = get_json(addr, "/api/state?session=bob");
    assert_eq!(a["turn"], 1);
    assert_eq!(b["turn"], 1);

    let ev_a = get_json(addr, "/api/events?limit=50&session=alice");
    let texts: Vec<String> = ev_a["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "user_input")
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(texts, vec!["甲说的话".to_string()]);
}

#[test]
fn reset_sends_the_session_back_to_turn_zero() {
    let addr = start();
    post_json(
        addr,
        "/api/say",
        r#"{"text":"第一句","session":"test-reset"}"#,
    );
    assert_eq!(get_json(addr, "/api/state?session=test-reset")["turn"], 1);

    let (status, v) = post_json(addr, "/api/reset", r#"{"session":"test-reset"}"#);
    assert_eq!(status, 200);
    assert_eq!(v["reset"], true);
    assert_eq!(get_json(addr, "/api/state?session=test-reset")["turn"], 0);
}

// ------------------------------------------------------------------ 图片

#[test]
fn sticker_images_are_served_with_the_right_type() {
    let addr = start();
    let (status, headers, body) = call(addr, "GET", "/stickers/happy_01.png", None);
    assert_eq!(status, 200);
    assert!(headers.to_lowercase().contains("image/png"));
    assert!(headers.to_lowercase().contains("max-age"));
    assert_eq!(body, png_1px(), "字节必须原样返回");
}

#[test]
fn unknown_and_unsafe_image_paths_are_refused() {
    let addr = start();

    // 不在目录里
    let (status, _, _) = call(addr, "GET", "/stickers/nope.png", None);
    assert_eq!(status, 404);

    // 目录里存在但不是图片 → 不在白名单
    let (status, _, _) = call(addr, "GET", "/stickers/README.md", None);
    assert_eq!(status, 404);

    // 目录穿越：既过不了字符白名单，也过不了白名单登记
    for path in [
        "/stickers/..%2FCargo.toml",
        "/stickers/../../secret.png",
        "/stickers/happy_01.png%2F..%2F..%2FCargo.toml",
    ] {
        let (status, _, _) = call(addr, "GET", path, None);
        assert!(
            status == 400 || status == 404,
            "{path} 不该被服务（得到 {status}）"
        );
    }
}

// ------------------------------------------------------------------ 杂项

#[test]
fn unknown_paths_and_methods_are_answered_properly() {
    let addr = start();
    let (status, _, _) = call(addr, "GET", "/nope", None);
    assert_eq!(status, 404);

    let (status, _, _) = call(addr, "DELETE", "/api/state", None);
    assert_eq!(status, 405);

    let (status, _, _) = call(addr, "OPTIONS", "/api/say", None);
    assert_eq!(status, 204);
}

#[test]
fn query_parameters_can_carry_chinese_session_names() {
    let addr = start();
    // body 里是原始 UTF-8，query 里是百分号编码——两者必须落到同一个会话
    let encoded = "%E6%9E%97%E5%A4%8F"; // 林夏
    let (status, v) = post_json(addr, "/api/say", r#"{"text":"你好","session":"林夏"}"#);
    assert_eq!(status, 200, "{v}");

    let st = get_json(addr, &format!("/api/state?session={encoded}"));
    assert_eq!(st["card"], "林夏");
    assert_eq!(st["turn"], 1, "同一会话名不该被拆成两个会话");
}

#[test]
fn a_client_that_hangs_up_early_does_not_take_the_server_down() {
    let addr = start();
    {
        // 连上就断：服务端应当安静地结束这条连接
        let s = TcpStream::connect(addr).unwrap();
        drop(s);
    }
    // 半截请求头
    {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"GET /api/state HTTP/1.1\r\nHost: x\r\n")
            .unwrap();
        s.flush().unwrap();
        drop(s);
    }
    // 服务端照常工作
    let v = get_json(addr, "/api/status");
    assert_eq!(v["ok"], true);
}
