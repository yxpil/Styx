//! # styx-web — 浏览器前端
//!
//! 把 [`styx_server::Server`]（一行一个 JSON 的 TCP 服务）接到浏览器上。
//!
//! ## 为什么是"翻译层"而不是"第二个实现"
//!
//! 浏览器不能直接开 TCP，所以需要 HTTP。但**回合逻辑一行都不该重复**：
//! HTTP 请求被翻译成一条 [`styx_server::Request`]，交给同一个 `Server::dispatch`，
//! 再把响应原样序列化成 JSON 回给前端。
//!
//! 于是同一份内核有了三个入口，且行为必然一致：
//!
//! ```text
//!   styx repl          → 内核
//!   styx serve（TCP）  → 内核
//!   styx web（HTTP）   → styx-server 的 dispatch → 内核
//! ```
//!
//! ## 前端长什么样
//!
//! 单页、零构建、零 CDN（`include_str!` 内嵌进二进制，断网可用）：
//! 左边是舞台（台词 / 动作 / 内心 / 表情图），右边是状态与调试侧栏，
//! 底部是输入框和**表情包面板**。表情包图片由 [`StickerCatalog`] 的
//! 白名单驱动——只有目录里登记过的文件才会被读出来。

pub mod assets;
pub mod http;

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use styx_core::text::round3;
use styx_core::StickerCatalog;
use styx_server::{ConnectionState, Server};

/// 浏览器打开时一次拉取多少条历史事件（刷新后据此重建画面）。
const BOOTSTRAP_EVENTS: usize = 120;

/// Web 前端服务。
pub struct WebServer {
    server: Arc<Server>,
    catalog: Arc<StickerCatalog>,
    /// 表情包图片所在目录（只从中读已经登记在目录里的文件名）。
    sticker_dir: PathBuf,
    default_session: String,
    verbose: bool,
}

impl std::fmt::Debug for WebServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebServer")
            .field("stickers", &self.catalog.len())
            .field("sticker_dir", &self.sticker_dir)
            .field("session", &self.default_session)
            .finish()
    }
}

impl WebServer {
    /// 新建。
    pub fn new(
        server: Arc<Server>,
        catalog: Arc<StickerCatalog>,
        sticker_dir: impl Into<PathBuf>,
    ) -> Self {
        WebServer {
            server,
            catalog,
            sticker_dir: sticker_dir.into(),
            default_session: "web".into(),
            verbose: false,
        }
    }

    /// 默认会话名（浏览器没带 `session` 参数时用它）。
    pub fn with_default_session(mut self, name: impl Into<String>) -> Self {
        self.default_session = name.into();
        self
    }

    /// 打印每个请求（调试用）。
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    pub fn catalog(&self) -> &StickerCatalog {
        &self.catalog
    }

    /// 绑定并阻塞运行。
    pub fn bind_and_run(self: Arc<Self>, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        let actual = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| addr.to_string());
        let shown = if actual.starts_with("0.0.0.0") {
            actual.replacen("0.0.0.0", "127.0.0.1", 1)
        } else {
            actual.clone()
        };
        println!("Styx Web 已启动：http://{shown}");
        println!("会话：{} · 表情包：{} 张", self.default_session, self.catalog.len());
        if self.catalog.is_empty() {
            println!(
                "提示：{} 里没有图片，表情包面板会是空的（把 PNG 放进去再启动即可）",
                self.sticker_dir.display()
            );
        }
        println!("按 Ctrl+C 结束。");
        self.serve(listener)
    }

    /// 接受连接（阻塞）。
    ///
    /// 一连接一线程：与 `styx-server` 同一套模型。这是个本地单人界面，
    /// 不需要连接数保护——真正慢的是模型，不是这里。
    pub fn serve(self: Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let me = Arc::clone(&self);
                    std::thread::spawn(move || {
                        if let Err(e) = me.handle(stream) {
                            eprintln!("连接结束：{e}");
                        }
                    });
                }
                Err(e) => eprintln!("接受连接失败：{e}"),
            }
        }
        Ok(())
    }

    /// 处理一条连接：读一个请求、回一个响应、关闭。
    pub fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        // 读超时只为防"连上不发数据的半开连接"；写超时留足回合时间
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(300))).ok();
        stream.set_nodelay(true).ok();

        let started = Instant::now();
        let mut reader = std::io::BufReader::new(stream.try_clone()?);
        let req = match http::read_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            Err(e) => {
                let resp = http::Response::bad_request(&e);
                if self.verbose {
                    eprintln!("<- 400 {e}");
                }
                return http::write_response(&mut stream, &resp);
            }
        };

        let resp = self.route(&req);
        if self.verbose {
            eprintln!(
                "{} {} → {} ({} ms)",
                req.method,
                req.path,
                resp.status,
                started.elapsed().as_millis()
            );
        }
        http::write_response(&mut stream, &resp)
    }

    /// 路由。
    pub fn route(&self, req: &http::Request) -> http::Response {
        let session = self.session_name(req);

        match (req.method.as_str(), req.path.as_str()) {
            // ---- 静态资源（内嵌，断网可用）----
            ("GET", "/") | ("GET", "/index.html") => http::Response::html(assets::INDEX_HTML),
            ("GET", "/app.css") => http::Response::asset("text/css; charset=utf-8", assets::APP_CSS),
            ("GET", "/app.js") => {
                http::Response::asset("text/javascript; charset=utf-8", assets::APP_JS)
            }
            ("GET", "/favicon.ico") => http::Response::new(204, "image/x-icon", Vec::new()),

            // ---- 启动时一次拉全 ----
            ("GET", "/api/bootstrap") => http::Response::json(200, &self.bootstrap(&session)),

            // ---- 只读查询 ----
            ("GET", "/api/state") | ("POST", "/api/state") => {
                self.json_op(&session, styx_server::Request::State)
            }
            ("GET", "/api/scene") => self.json_op(&session, styx_server::Request::Scene),
            ("GET", "/api/status") => self.json_op(&session, styx_server::Request::Status),
            ("GET", "/api/events") => {
                let limit = req.num_param("limit").unwrap_or(60);
                self.json_op(&session, styx_server::Request::Events { limit: Some(limit) })
            }
            ("GET", "/api/stickers") => http::Response::json(200, &self.stickers_json()),

            // ---- 推进回合 ----
            ("GET", "/api/say") | ("POST", "/api/say") => {
                let Some(text) = req.str_param("text") else {
                    return http::Response::bad_request("缺少 text");
                };
                if text.trim().is_empty() {
                    return http::Response::bad_request("text 不能为空");
                }
                self.json_op(&session, styx_server::Request::Say { text })
            }
            ("GET", "/api/sticker") | ("POST", "/api/sticker") => {
                let Some(id) = req.str_param("id") else {
                    return http::Response::bad_request("缺少 id");
                };
                self.json_op(&session, styx_server::Request::Sticker { id })
            }

            // ---- 会话控制 ----
            ("POST", "/api/reset") | ("GET", "/api/reset") => {
                self.json_op(&session, styx_server::Request::Reset { scene: None })
            }
            ("POST", "/api/remember") => {
                let Some(text) = req.str_param("text") else {
                    return http::Response::bad_request("缺少 text");
                };
                let tags: Vec<String> = req
                    .json()
                    .and_then(|j| {
                        j.get("tags").and_then(|t| t.as_array()).map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                    })
                    .unwrap_or_default();
                let importance = req
                    .json()
                    .and_then(|j| j.get("importance").and_then(|v| v.as_f64()))
                    .map(|v| v as f32);
                self.json_op(
                    &session,
                    styx_server::Request::Remember {
                        text,
                        tags,
                        importance,
                    },
                )
            }
            ("GET", "/api/recall") | ("POST", "/api/recall") => {
                let Some(query) = req.str_param("query") else {
                    return http::Response::bad_request("缺少 query");
                };
                let limit = req.num_param("limit");
                self.json_op(&session, styx_server::Request::Recall { query, limit })
            }
            ("GET", "/api/associate") | ("POST", "/api/associate") => {
                let Some(seed) = req.str_param("seed") else {
                    return http::Response::bad_request("缺少 seed");
                };
                let limit = req.num_param("limit");
                self.json_op(&session, styx_server::Request::Associate { seed, limit })
            }

            ("OPTIONS", _) => http::Response::new(204, "text/plain", Vec::new()),

            // ---- 表情包图片：只认目录里登记过的文件名 ----
            ("GET", path) if path.starts_with("/stickers/") => {
                self.image(path.trim_start_matches("/stickers/"))
            }

            (m, _) if m != "GET" && m != "POST" => {
                http::Response::text(405, format!("405 不支持的方法：{m}"))
            }
            (_, path) => http::Response::not_found(path),
        }
    }

    // ---------------------------------------------------------------- 内部

    /// 会话名：取参数、去掉控制字符、限长，空则用默认。
    fn session_name(&self, req: &http::Request) -> String {
        let cleaned: String = req
            .str_param("session")
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_control())
            .take(48)
            .collect();
        let cleaned = cleaned.trim().to_string();
        if cleaned.is_empty() {
            self.default_session.clone()
        } else {
            cleaned
        }
    }

    /// 把一条协议请求交给 `styx-server` 的 dispatch，取回 JSON。
    ///
    /// `ConnectionState` 是"连接级"的，而 HTTP 每请求都是一个新连接，
    /// 所以每次都要把会话名重新放进去——这正是会话名比连接更重要的原因：
    /// 角色扮演的连续性挂在会话上，不挂在套接字上。
    fn dispatch(&self, session: &str, op: styx_server::Request) -> Value {
        let mut state = ConnectionState {
            session: Some(session.to_string()),
            served: 0,
        };
        let (resp, _close) = self.server.dispatch(&mut state, op);
        serde_json::to_value(&resp).unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}))
    }

    /// 协议响应 → HTTP 响应（`ok:false` 用 400，前端统一读 body）。
    fn json_op(&self, session: &str, op: styx_server::Request) -> http::Response {
        let v = self.dispatch(session, op);
        let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
        http::Response::json(if ok { 200 } else { 400 }, &v)
    }

    /// 启动数据：卡片、场景、状态、最近事件、表情包目录。
    fn bootstrap(&self, session: &str) -> Value {
        json!({
            "ok": true,
            "version": styx_core::VERSION,
            "session": session,
            "hello": self.dispatch(session, styx_server::Request::Hello {
                session: Some(session.to_string()),
            }),
            "state": self.dispatch(session, styx_server::Request::State),
            "scene": self.dispatch(session, styx_server::Request::Scene),
            "status": self.dispatch(session, styx_server::Request::Status),
            "events": self.dispatch(
                session,
                styx_server::Request::Events { limit: Some(BOOTSTRAP_EVENTS) },
            ),
            "stickers": self.stickers_json(),
        })
    }

    /// 表情包清单（给前端铺网格用）。
    fn stickers_json(&self) -> Value {
        let list: Vec<Value> = self
            .catalog
            .all()
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "file": s.file,
                    "emotion": s.emotion,
                    "label": s.label,
                    "description": s.description,
                    "tags": s.tags,
                    "valence": round3(s.valence),
                    "arousal": round3(s.arousal),
                    "url": format!("/stickers/{}", s.file),
                })
            })
            .collect();
        json!({
            "count": list.len(),
            "dir": self.sticker_dir.display().to_string(),
            "stickers": list,
        })
    }

    /// 读一张表情包图片。
    ///
    /// 两道门：文件名必须通过字符白名单，且必须在目录里登记过。
    /// 这样 `GET /stickers/../../secret.png` 连第一步都过不去。
    fn image(&self, name: &str) -> http::Response {
        if !http::is_safe_segment(name) {
            return http::Response::bad_request("文件名不合法");
        }
        if !self.catalog.has_file(name) {
            return http::Response::not_found(name);
        }
        let path = self.sticker_dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => http::Response::bytes(http::mime_of(name), bytes, 3600),
            Err(e) => http::Response::server_error(&format!(
                "读不到 {}：{e}（文件被移动或删除了？）",
                path.display()
            )),
        }
    }
}

/// 在若干候选位置里找一个存在的表情包目录。
///
/// 顺序刻意如此：显式指定 > 配置 > 当前目录 > 仓库内的默认目录。
/// 最后一档是为了 `cargo run -p styx -- web` 在仓库根直接能跑起来。
pub fn locate_sticker_dir(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|p| p.is_dir())
        .map(|p| p.to_path_buf())
}

/// 从目录加载表情包目录（目录不存在则返回空目录，不报错）。
pub fn load_catalog(dir: &Path) -> Arc<StickerCatalog> {
    Arc::new(StickerCatalog::load_dir(dir))
}

/// 打开浏览器（尽力而为，失败只留一句提示）。
pub fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);

    match std::process::Command::new(cmd).args(args).spawn() {
        Ok(_) => {}
        Err(e) => eprintln!("· 打不开浏览器（{e}），手动访问 {url} 即可"),
    }
}
