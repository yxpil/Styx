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

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use styx_core::text::round3;
use styx_core::StickerCatalog;
use styx_guard::{Exposure, Token};
use styx_observ::Alerter;
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
    /// 启动时刻（`/healthz` 报 uptime 用）。
    started: Instant,
    /// 告警规则引擎。`None` 表示不评估（个人模式默认如此）。
    ///
    /// 评估挂在 **`/metrics` 被拉取的那一刻**，而不是自己起一个心跳线程：
    /// 指标本来就该由外部按固定间隔来取，顺着这次取数做一次判断，
    /// 既省一个线程，也天然和采集周期对齐。
    alerter: Option<Arc<Alerter>>,
    /// 访问令牌。`None` 表示不校验（个人模式默认如此）。
    token: Option<Token>,
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
            started: Instant::now(),
            alerter: None,
            token: None,
        }
    }

    /// 挂上告警引擎。
    pub fn with_alerts(mut self, alerter: Arc<Alerter>) -> Self {
        self.alerter = Some(alerter);
        self
    }

    /// 要求访问令牌。
    pub fn with_token(mut self, token: Token) -> Self {
        self.token = Some(token);
        self
    }

    /// 是否已启用访问令牌。
    pub fn requires_token(&self) -> bool {
        self.token.is_some()
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
        // 把"这个端口有多开放"说出来。安全上最常见的失误不是认证写错，
        // 是**根本没意识到它是开放的**——这句话就是防这个的。
        let exposure = Exposure::of(addr);
        match exposure {
            Exposure::Loopback => {}
            Exposure::Private => {
                println!("· 监听地址不是本机专属：同一网段的人都能连到这里");
            }
            Exposure::Public => {
                println!("· **监听地址公网可达**：任何能路由到这台机器的人都能连");
            }
        }
        if exposure != Exposure::Loopback && self.token.is_none() {
            println!("· 提示：当前**没有设置访问令牌**。对外提供访问时请用 --token，");
            println!("  或生成一个：styx web --generate-token");
        }

        println!("Styx Web 已启动：http://{shown}");
        println!(
            "会话：{} · 表情包：{} 张",
            self.default_session,
            self.catalog.len()
        );
        if self.token.is_some() {
            println!("访问令牌：已启用（运维端点 /healthz /readyz /metrics 免令牌）");
        }
        if self.catalog.is_empty() {
            println!(
                "提示：{} 里没有图片，表情包面板会是空的（把 PNG 放进去再启动即可）",
                self.sticker_dir.display()
            );
        }
        let m = self.server.metrics();
        m.describe("styx_http_requests_total", "累计处理的 HTTP 请求数");
        m.describe("styx_http_request_seconds", "一次 HTTP 请求的耗时（秒）");
        m.describe("styx_http_5xx_total", "累计 5xx 响应数");
        if let Some(a) = &self.alerter {
            println!("告警规则：{} 条", a.rules().len());
        }
        println!("按 Ctrl+C 优雅结束（正在进行的回合会跑完）。");
        self.serve(listener)
    }

    /// 接受连接（阻塞）。
    ///
    /// 一连接一线程，与 `styx-server` 同一套模型——所以也必须过同一道门。
    ///
    /// 这里原本写着"本地单人界面，不需要连接数保护"。那句话的问题在于它
    /// 把**部署形态**当成了**代码性质**：同一份二进制，绑在 `127.0.0.1`
    /// 时确实不需要，绑在 `0.0.0.0` 上面对的就是没有上限的连接请求。
    /// 门和关闭信号都从 `Server` 借用，于是 `web` 与 `serve` 两个入口
    /// 共享的是**这一个进程**的配额，而不是各算各的。
    pub fn serve(self: Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        let gate = Arc::clone(self.server.gate());
        let shutdown = Arc::clone(self.server.shutdown_handle());
        let limits = self.server.limits().clone();

        if let Ok(addr) = listener.local_addr() {
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                shutdown.wait_triggered();
                // 叫醒卡在 accept 上的循环，让它有机会看到关闭标志。
                let _ = TcpStream::connect(addr);
            });
        }

        for stream in listener.incoming() {
            if shutdown.is_shutting_down() {
                break;
            }
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    styx_observ::log_warn!("web", "接受连接失败：{e}");
                    continue;
                }
            };
            let Some(permit) = gate.acquire(limits.acquire_timeout()) else {
                let mut s = stream;
                let _ = s.write_all(b"503 service busy: too many connections\n");
                let _ = s.flush();
                continue;
            };
            let me = Arc::clone(&self);
            std::thread::spawn(move || {
                let _permit = permit;
                if let Err(e) = me.handle(stream) {
                    styx_observ::log_debug!("web", "连接结束：{e}");
                }
            });
        }

        let drained = shutdown.wait_idle(limits.drain_timeout());
        if drained {
            styx_observ::log_info!("web", "已停止接受连接，在飞请求已收尾");
        } else {
            styx_observ::log_warn!(
                "web",
                "已停止接受连接，但仍有 {} 个请求没收尾",
                shutdown.active()
            );
        }
        Ok(())
    }

    /// 处理一条连接：读一个请求、回一个响应、关闭。
    pub fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        // 读超时只为防"连上不发数据的半开连接"；写超时留足回合时间
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        stream
            .set_write_timeout(Some(Duration::from_secs(300)))
            .ok();
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
        let metrics = self.server.metrics();
        metrics.inc("styx_http_requests_total", 1);
        metrics.observe("styx_http_request_seconds", started.elapsed().as_secs_f64());
        if resp.status >= 500 {
            metrics.inc("styx_http_5xx_total", 1);
        }
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
        if let Some(denied) = Self::check_auth(self.token.as_ref(), req) {
            return denied;
        }
        let session = self.session_name(req);

        match (req.method.as_str(), req.path.as_str()) {
            // ---- 静态资源（内嵌，断网可用）----
            ("GET", "/") | ("GET", "/index.html") => http::Response::html(assets::INDEX_HTML),
            ("GET", "/app.css") => {
                http::Response::asset("text/css; charset=utf-8", assets::APP_CSS)
            }
            ("GET", "/app.js") => {
                http::Response::asset("text/javascript; charset=utf-8", assets::APP_JS)
            }
            ("GET", "/favicon.ico") => http::Response::new(204, "image/x-icon", Vec::new()),

            // ---- 健康探针 ----
            //
            // 这两个端点刻意**不碰内核、不做会话名清洗**：探针要回答的是
            // "这个进程还能不能干活"，它自己就不能依赖任何会一起坏掉的东西。
            // 放进这里之后，容器编排、systemd、监控才能问出一个二元答案，
            // 而不是靠"端口连得上"这种既慢又含糊的推断。
            ("GET", "/healthz") => http::Response::json(
                200,
                &json!({
                    "ok": true,
                    "service": "styx",
                    "version": styx_core::VERSION,
                    "uptime_ms": self.started.elapsed().as_millis() as u64,
                }),
            ),
            ("GET", "/readyz") => {
                let shutting = self.server.is_shutting_down();
                let ready = !shutting;
                http::Response::json(
                    if ready { 200 } else { 503 },
                    &json!({
                        "ok": ready,
                        "shutting_down": shutting,
                        "sessions": self.server.session_count(),
                        "in_flight": self.server.in_flight(),
                        "rejected": self.server.rejected(),
                    }),
                )
            }

            // ---- 指标 ----
            //
            // 顺手评估一轮告警：拉取方本来就按固定间隔来取数，
            // 判断跟着这次取数走，就不必再养一个心跳线程。
            ("GET", "/metrics") => {
                let metrics = self.server.metrics();
                if let Some(a) = &self.alerter {
                    // 投递交给 `AlertSink`（默认写结构化日志），这里不重复打印。
                    a.evaluate(metrics);
                }
                let body = metrics.render();
                let mut resp = http::Response::new(
                    200,
                    "text/plain; version=0.0.4; charset=utf-8",
                    body.into_bytes(),
                );
                // 指标必须每次都重新拉：中间任何一层缓存都会让"当前值"
                // 变成"某个时刻的值"，而面板看不出区别。
                resp.headers
                    .push(("Cache-Control".into(), "no-store".into()));
                resp
            }

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
                self.json_op(
                    &session,
                    styx_server::Request::Events { limit: Some(limit) },
                )
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

    /// 运维端点：**不校验令牌**。
    ///
    /// 探针和指标采集器通常不带任何凭据。把它们也挡在门外，等于逼人
    /// 二选一：要么关掉认证，要么把令牌写进采集配置里——两种都比放行
    /// 这三个端点更糟。而这三个端点里没有一句对话。
    fn is_ops_endpoint(path: &str) -> bool {
        matches!(path, "/healthz" | "/readyz" | "/metrics")
    }

    /// 校验访问令牌；不通过时返回现成的 401 响应。
    ///
    /// 三种带法都收：`Authorization: Bearer`（标准）、`X-Styx-Token`
    /// （不想和别的 `Authorization` 打架时用）、`?token=`（浏览器里
    /// 直接敲地址也能用）。
    fn check_auth(token: Option<&Token>, req: &http::Request) -> Option<http::Response> {
        let token = token?;
        if Self::is_ops_endpoint(&req.path) {
            return None;
        }
        let presented = req
            .headers
            .get("authorization")
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
            .map(|s| s.to_string())
            .or_else(|| req.headers.get("x-styx-token").cloned())
            .or_else(|| req.str_param("token"));

        match presented {
            Some(presented) if token.verify(&presented) => None,
            _ => Some(http::Response::json(
                401,
                &json!({
                    "ok": false,
                    "error": "缺少或错误的访问令牌",
                }),
            )),
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn req(path: &str) -> http::Request {
        http::Request {
            method: "GET".into(),
            path: path.into(),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn without_a_token_everything_is_allowed() {
        assert!(WebServer::check_auth(None, &req("/api/state")).is_none());
    }

    #[test]
    fn with_a_token_requests_must_present_it() {
        let t = Token::new(SECRET).unwrap();

        let denied = WebServer::check_auth(Some(&t), &req("/api/state"));
        assert_eq!(denied.expect("应当被拒").status, 401);

        let mut ok = req("/api/state");
        ok.headers
            .insert("authorization".into(), format!("Bearer {SECRET}"));
        assert!(WebServer::check_auth(Some(&t), &ok).is_none());
    }

    #[test]
    fn a_wrong_token_is_rejected() {
        let t = Token::new(SECRET).unwrap();
        let mut bad = req("/api/state");
        bad.headers
            .insert("authorization".into(), "Bearer nope".into());
        assert_eq!(WebServer::check_auth(Some(&t), &bad).unwrap().status, 401);
    }

    #[test]
    fn the_token_can_also_come_from_a_header_or_a_query() {
        let t = Token::new(SECRET).unwrap();

        let mut header = req("/api/state");
        header.headers.insert("x-styx-token".into(), SECRET.into());
        assert!(WebServer::check_auth(Some(&t), &header).is_none());

        let mut query = req("/api/state");
        query.query.insert("token".into(), SECRET.into());
        assert!(WebServer::check_auth(Some(&t), &query).is_none());
    }

    #[test]
    fn ops_endpoints_never_require_a_token() {
        let t = Token::new(SECRET).unwrap();
        for path in ["/healthz", "/readyz", "/metrics"] {
            assert!(
                WebServer::check_auth(Some(&t), &req(path)).is_none(),
                "{path} 不该要令牌——探针和采集器不带凭据"
            );
        }
    }

    #[test]
    fn the_static_shell_is_protected_too() {
        // 界面本身也要挡住：否则未授权的人至少能把完整前端和 API 形状拿走。
        let t = Token::new(SECRET).unwrap();
        assert_eq!(
            WebServer::check_auth(Some(&t), &req("/")).unwrap().status,
            401
        );
        assert_eq!(
            WebServer::check_auth(Some(&t), &req("/app.js"))
                .unwrap()
                .status,
            401
        );
    }
}
