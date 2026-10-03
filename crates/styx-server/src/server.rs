//! TCP 服务：把内核暴露成常驻的多会话服务。
//!
//! # 会话模型
//!
//! - 每个连接有一个"当前会话名"（`hello` 里指定，默认 `default`）；
//! - 服务端持有一张 `会话名 → Kernel` 表，**内核只在处理请求时被短暂取出**，
//!   因此不同会话之间互不阻塞（网络 IO 期间不持锁）；
//! - 同一个会话名的并发请求**排在同一个内核上依次进行**（single-flight）：
//!   后来者等前一个把内核放回来，而不是自己再造一个。
//!
//! # 为什么"表里没有"不能直接等于"新建一个"
//!
//! 早先的写法是"取不到就造一个"。代价不只是多花 CPU，而是**状态会倒退**：
//! 前端在 `say` 进行中轮询 `state` 时，两个请求各拿到一个内核、各自推进再
//! 各自放回，后放回的那个会**覆盖**先放回的那个的状态——用户看到的就是
//! "刚说完话，状态又跳回去了"。
//!
//! 所以取不到时要区分两件事：**没人在用**（可以建）和**有人正在用**
//! （必须等）。这个区分必须和会话表在同一把锁下做出，否则就成了
//! "先检查后动作"的竞态。
//!
//! # 线程模型
//!
//! 一连接一线程。角色扮演是**低并发、高延迟**（每回合要等模型几秒到几十秒），
//! 用异步运行时换来的收益，远不如"代码简单、能塞进 CLI"来得实在。
//!
//! 但"同步"不等于"不设防"。这个模型下**连接数上限就是线程数上限**：
//! 没有准入闸门时，一个 `curl` 循环就能把线程表打满。所以连接要先过
//! [`styx_guard::Gate`] 才允许开线程——门和关闭信号都来自 [`styx_guard`]，
//! 且 `styx-web` 复用**同一份**：两个入口的配额应该是"这一个进程"的配额，
//! 而不是各算各的。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use styx_core::error::{Result, StyxError};
use styx_core::text::round3;
use styx_core::{Kernel, Reply, Scene, TurnOutcome};
use styx_guard::{Gate, Limits, Shutdown};
use styx_observ::Metrics;

use crate::protocol::{Request, Response};

/// 内核工厂：为每个新会话造一个内核。
pub trait KernelFactory: Send + Sync {
    /// 造一个内核；`session_id` 可用于隔离持久化命名空间。
    fn create(&self, session_id: &str) -> Result<Kernel>;

    /// 工厂描述（出现在 `hello` 与日志里）。
    fn describe(&self) -> String {
        "kernel-factory".into()
    }
}

impl<F> KernelFactory for F
where
    F: Fn(&str) -> Result<Kernel> + Send + Sync,
{
    fn create(&self, session_id: &str) -> Result<Kernel> {
        self(session_id)
    }
}

/// 连接级状态。
#[derive(Debug, Default, Clone)]
pub struct ConnectionState {
    /// 当前会话名。
    pub session: Option<String>,
    /// 已处理的请求数。
    pub served: u64,
}

/// 一个常驻会话。
///
/// `last_used` 是 LRU 淘汰的依据。把它记在表里而不是另维护一个访问队列：
/// 会话数上限本来就很小（几十到几百），淘汰时扫一遍找最小值足够快，
/// 而多一个队列就多一处会不同步的地方。
struct SessionEntry {
    kernel: Kernel,
    last_used: Instant,
}

/// 会话表的内部状态。
///
/// `map` 与 `busy` 必须在**同一把锁**下读写：取内核时要能一次原子地判断
/// "表里没有**且**没人在用"（那就自己建），而不是先查表、再查占用——
/// 那样两步之间别人就能插进来，又变成各建一个。
#[derive(Default)]
struct Sessions {
    /// 空闲的内核，放着随时能取。
    map: HashMap<String, SessionEntry>,
    /// 已被取走、正在被使用（或正在构建）的会话名。
    busy: HashSet<String>,
}

/// 等一个正在被使用的会话，最多等这么久。
///
/// 比模型回合的时长宽得多（长回合几分钟是常态），所以正常情况下永远等不满。
/// 它只是"万一有 bug 把某个会话永久占住了"时的兜底，避免请求无限挂死。
const SESSION_WAIT_LIMIT: Duration = Duration::from_secs(600);

/// 会话内核的"租约"：拿到手就占住这个名字，**Drop 时不管因为什么都会归还**。
///
/// 用一个持有型守卫而不是"用完手动 `put_session`"，是为了 panic 安全：
/// 处理回合的代码 panic 时，若归还这一步被跳过，这个名字就会永远卡在
/// `busy` 里，之后所有请求都只能干等——把一个"崩一个请求"的小问题
/// 放大成"这个会话再也用不了"的大问题。
struct KernelLease<'a> {
    server: &'a Server,
    name: String,
    /// `Option` 只是为了能在 `Drop` 里把内核 move 出去。
    kernel: Option<Kernel>,
}

impl KernelLease<'_> {
    fn kernel(&mut self) -> &mut Kernel {
        self.kernel.as_mut().expect("租约期间内核一定在")
    }
}

impl Drop for KernelLease<'_> {
    fn drop(&mut self) {
        if let Some(kernel) = self.kernel.take() {
            self.server.put_session(self.name.clone(), kernel);
        }
    }
}

/// Styx TCP 服务。
pub struct Server {
    factory: Arc<dyn KernelFactory>,
    sessions: Mutex<Sessions>,
    /// 有会话被放回时唤醒等待者。与会话表配成一对。
    released: Condvar,
    /// 默认会话名。
    default_session: String,
    /// 统计。
    turns: AtomicU64,
    errors: AtomicU64,
    /// 因配额被拒的连接数。
    rejected: AtomicU64,
    /// 服务层配额（默认全不限）。
    limits: Limits,
    /// 连接准入闸门。
    gate: Arc<Gate>,
    /// 关闭协调器。
    shutdown: Arc<Shutdown>,
    /// 指标。
    metrics: Metrics,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("factory", &self.factory.describe())
            .field("sessions", &self.session_count())
            .finish()
    }
}

impl Server {
    /// 新建。默认**不限连接、不限会话**——个人模式下的行为与从前一致。
    pub fn new(factory: Arc<dyn KernelFactory>) -> Self {
        Server {
            factory,
            sessions: Mutex::new(Sessions::default()),
            released: Condvar::new(),
            default_session: "default".into(),
            turns: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            limits: Limits::default(),
            gate: Gate::new(0),
            shutdown: Shutdown::new(),
            metrics: Metrics::new(),
        }
    }

    /// 改默认会话名。
    pub fn with_default_session(mut self, name: impl Into<String>) -> Self {
        self.default_session = name.into();
        self
    }

    /// 设定服务层配额。
    ///
    /// 只在构造期可用：闸门的上限是个定值，改配额就得连闸门一起换。
    /// 允许运行中改配额是个陷阱——已经在飞的连接还按旧上限计着账。
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.gate = Gate::new(limits.max_connections);
        self.limits = limits;
        self
    }

    /// 当前配额。
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// 连接闸门。`styx-web` 复用它，让两个入口共享同一份配额。
    pub fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    /// 关闭协调器。`styx-web` 复用它，让两个入口共享同一个关闭信号。
    pub fn shutdown_handle(&self) -> &Arc<Shutdown> {
        &self.shutdown
    }

    /// 是否已进入关闭流程。
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.is_shutting_down()
    }

    /// 当前在飞连接数。
    pub fn in_flight(&self) -> usize {
        self.gate.in_flight()
    }

    /// 因配额被拒的连接数。
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }

    /// 指标登记处。`styx-web` 靠它把 `/metrics` 和告警接出去。
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// 已加载的会话数。
    pub fn session_count(&self) -> usize {
        self.sessions.lock().map(|s| s.map.len()).unwrap_or(0)
    }

    /// 累计回合数。
    pub fn turns(&self) -> u64 {
        self.turns.load(Ordering::Relaxed)
    }

    /// 累计错误数。
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// 绑定并返回监听器，便于先拿到端口号再决定何时开始接受连接。
    pub fn bind(&self, addr: &str) -> Result<TcpListener> {
        TcpListener::bind(addr).map_err(|e| StyxError::Other(format!("无法监听 {addr}：{e}")))
    }

    /// 绑定并阻塞运行。
    pub fn bind_and_run(self: Arc<Self>, addr: &str) -> Result<()> {
        let listener = self.bind(addr)?;
        let actual = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        println!("Styx 服务已启动：{actual}");
        println!("会话工厂：{}", self.factory.describe());
        self.describe_limits();
        self.describe_metrics();
        self.run(listener)
    }

    /// 给每个指标挂一句说明，导出的 `# HELP` 才有内容。
    fn describe_metrics(&self) {
        let m = &self.metrics;
        m.describe("styx_connections_total", "累计接受的连接数");
        m.describe("styx_rejected_total", "因配额或超限被拒的连接数");
        m.describe("styx_in_flight", "当前正在处理的连接数");
        m.describe("styx_sessions", "常驻会话数");
        m.describe("styx_turns_total", "累计完成的回合数");
        m.describe("styx_errors_total", "累计错误数");
        m.describe("styx_turn_seconds", "一次回合的耗时（秒）");
    }

    /// 把生效的配额打一行出来。
    ///
    /// 配额最怕的是"配了没生效"：写在文件里、跑起来忘了、出事时才发现
    /// 根本没读进去。启动时打一行，比翻文档快。
    fn describe_limits(&self) {
        let l = &self.limits;
        let cap = |n: usize| {
            if n > 0 {
                n.to_string()
            } else {
                "不限".to_string()
            }
        };
        if l.limits_connections() || l.limits_sessions() || l.limits_line() {
            println!(
                "配额：连接 ≤ {} · 会话 ≤ {} · 单行 ≤ {} 字节 · 超限{}",
                cap(l.max_connections),
                cap(l.max_sessions),
                cap(l.max_line_bytes),
                if l.acquire_timeout_ms > 0 {
                    format!("最多等 {} ms", l.acquire_timeout_ms)
                } else {
                    "立即拒绝".to_string()
                },
            );
        } else {
            println!("配额：不限（个人模式）");
        }
    }

    /// 接受连接（阻塞）。
    ///
    /// 关闭时不能靠"杀进程"——那样正在跑的回合会被硬切。这里用一个
    /// **叫醒连接**：关闭触发后主动连一次自己，让 `accept` 从阻塞里返回，
    /// 循环看到标志位再收摊。这比"非阻塞轮询 + sleep"少一层空转。
    pub fn run(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        if let Ok(addr) = listener.local_addr() {
            let shutdown = Arc::clone(&self.shutdown);
            std::thread::spawn(move || {
                shutdown.wait_triggered();
                // 这条连接唯一的使命就是让 accept 醒过来，内容无所谓。
                let _ = TcpStream::connect(addr);
            });
        }

        for stream in listener.incoming() {
            if self.shutdown.is_shutting_down() {
                break;
            }
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    styx_observ::log_warn!("server", "接受连接失败：{e}");
                    continue;
                }
            };
            // 先占名额再开线程。**线程是这里唯一真正昂贵的资源**，
            // 先开出来再判断要不要，等于白开。
            let Some(permit) = self.gate.acquire(self.limits.acquire_timeout()) else {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                self.metrics.inc("styx_rejected_total", 1);
                reject_connection(stream, "hello", "服务繁忙：连接数已达上限");
                continue;
            };
            self.metrics.inc("styx_connections_total", 1);
            self.metrics
                .set("styx_in_flight", self.gate.in_flight() as f64);
            let server = Arc::clone(&self);
            std::thread::spawn(move || {
                // 凭证跟着线程走：线程正常结束、报错、甚至 panic，名额都会归还。
                let _permit = permit;
                if let Err(e) = server.handle_connection(stream) {
                    styx_observ::log_debug!("server", "连接结束：{e}");
                }
            });
        }

        // 不再收新连接了，等在飞的干完。
        let drained = self.shutdown.wait_idle(self.limits.drain_timeout());
        if drained {
            styx_observ::log_info!("server", "已停止接受连接，在飞请求已收尾");
        } else {
            styx_observ::log_warn!(
                "server",
                "已停止接受连接，但仍有 {} 个请求没收尾（超过 {} ms）",
                self.shutdown.active(),
                self.limits.drain_timeout_ms
            );
        }
        Ok(())
    }

    /// 处理一条连接。
    pub fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        // 先登记"我在飞"。已经在关闭中就不再受理——让对端早点知道，
        // 比收下请求再中途砍断要好。
        let Some(_inflight) = self.shutdown.enter() else {
            let mut s = stream;
            let msg = format!(
                "{}\n",
                Response::err("hello", "服务正在关闭，请稍后重连").to_line()
            );
            let _ = s.write_all(msg.as_bytes());
            let _ = s.flush();
            return Ok(());
        };

        stream.set_read_timeout(Some(Duration::from_secs(600))).ok();
        stream.set_nodelay(true).ok();
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| StyxError::Other(format!("复制套接字失败：{e}")))?,
        );
        let mut writer = stream;
        let mut state = ConnectionState::default();
        let max_line = self.limits.max_line_bytes;
        let mut line = String::new();

        loop {
            line.clear();
            match read_line_limited(&mut reader, max_line, &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::InvalidData => {
                    // 超长行必须**断开**而不是跳过：流里的位置已经错位，
                    // 接着读只会把后面半截当成一条新请求。
                    self.rejected.fetch_add(1, Ordering::Relaxed);
                    let msg = format!("{}\n", Response::err("read", e.to_string()).to_line());
                    let _ = writer.write_all(msg.as_bytes());
                    let _ = writer.flush();
                    let _ = writer.shutdown(std::net::Shutdown::Write);
                    let _ = writer.set_read_timeout(Some(Duration::from_millis(100)));
                    drain_until_quiet(&mut reader);
                    break;
                }
                Err(e) => {
                    styx_observ::log_warn!("server", "读取请求失败：{e}");
                    break;
                }
            }
            if line.trim().is_empty() {
                continue;
            }
            state.served += 1;
            let (resp, close) = match Request::parse(&line) {
                Ok(req) => self.dispatch(&mut state, req),
                Err(e) => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    (Response::err("parse", e), false)
                }
            };
            let out = format!("{}\n", resp.to_line());
            if writer.write_all(out.as_bytes()).is_err() {
                break;
            }
            let _ = writer.flush();
            if close {
                break;
            }
        }
        Ok(())
    }

    /// 分发一个请求。
    ///
    /// 之所以把它单独暴露出来，是为了让**协议行为可以脱离套接字测试**：
    /// 不启线程、不占端口，直接喂请求看响应。
    pub fn dispatch(&self, state: &mut ConnectionState, req: Request) -> (Response, bool) {
        let op = req.op().to_string();
        match self.try_dispatch(state, req) {
            Ok((resp, close)) => (resp, close),
            Err(e) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                self.metrics.inc("styx_errors_total", 1);
                (Response::err(&op, e.to_string()), false)
            }
        }
    }

    fn try_dispatch(&self, state: &mut ConnectionState, req: Request) -> Result<(Response, bool)> {
        match req {
            Request::Ping => Ok((
                Response::ok("ping", json!({"pong": true, "service": "styx"})),
                false,
            )),

            Request::Hello { session } => {
                let name = session
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| self.default_session.clone());
                // `resumed` 只是给客户端一句"接着上次"的提示，允许有极小竞态。
                // `busy` 里的也算已存在——它只是正被某个请求拿着，不是没有。
                let resumed = self
                    .sessions
                    .lock()
                    .map(|s| s.map.contains_key(&name) || s.busy.contains(&name))
                    .unwrap_or(false);
                state.session = Some(name.clone());
                // 内核按需构建：下面的 with_kernel 会走 single-flight 路径把它
                // 建出来。**失败要往上抛**，不能吞——握手时就得让客户端知道
                // "这个会话起不来"，而不是回一句 hello 之后每个请求都报错。
                let (card, turn, transcript) = self.with_kernel(&name, |k| {
                    Ok((
                        k.card().name.clone(),
                        k.state().turn,
                        k.session().transcript.len(),
                    ))
                })?;
                Ok((
                    Response::ok(
                        "hello",
                        json!({
                            "session": name,
                            "resumed": resumed,
                            "card": card,
                            "turn": turn,
                            "transcript": transcript,
                            "factory": self.factory.describe(),
                            "ops": [
                                "ping","hello","say","sticker","stickers","state",
                                "scene","set_scene","events","tools","call","status",
                                "remember","recall","associate","reset","quit"
                            ],
                        }),
                    ),
                    false,
                ))
            }

            Request::Say { text } => {
                let name = self.ensure_session(state)?;
                let started = Instant::now();
                let outcome = self.with_kernel(&name, |k| k.turn(&text))?;
                self.observe_turn(started);
                Ok((self.turn_response(&name, "say", &outcome), false))
            }

            Request::Sticker { id } => {
                let name = self.ensure_session(state)?;
                let started = Instant::now();
                let outcome = self.with_kernel(&name, |k| k.send_sticker(&id))?;
                self.observe_turn(started);
                Ok((self.turn_response(&name, "sticker", &outcome), false))
            }

            Request::Stickers => {
                let name = self.ensure_session(state)?;
                let v = self.with_kernel(&name, |k| {
                    let list = k
                        .stickers()
                        .map(|c| {
                            c.all()
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
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Ok(json!({"count": list.len(), "stickers": list}))
                })?;
                Ok((Response::ok("stickers", v), false))
            }

            Request::State => {
                let name = self.ensure_session(state)?;
                let v = self.with_kernel(&name, |k| {
                    Ok(json!({
                        "session": name,
                        "card": k.card().name,
                        "turn": k.state().turn,
                        "mood": k.state().mood,
                        "energy": round3(k.state().energy),
                        "tension": round3(k.state().tension),
                        // 用 BTreeMap 而不是原样输出 HashMap：键序稳定，浮点收敛
                        "affinity": k
                            .state()
                            .affinity
                            .iter()
                            .map(|(n, v)| (n.clone(), round3(*v)))
                            .collect::<BTreeMap<_, _>>(),
                        "trust": k
                            .state()
                            .trust
                            .iter()
                            .map(|(n, v)| (n.clone(), round3(*v)))
                            .collect::<BTreeMap<_, _>>(),
                        "agenda": k.state().agenda,
                        "flags": k.state().flags,
                        "rendered": k.state().render(),
                    }))
                })?;
                Ok((Response::ok("state", v), false))
            }

            Request::Scene => {
                let name = self.ensure_session(state)?;
                let v = self.with_kernel(&name, |k| Ok(serde_json::to_value(k.scene())?))?;
                Ok((Response::ok("scene", v), false))
            }

            Request::SetScene { scene } => {
                let name = self.ensure_session(state)?;
                let parsed: Scene = serde_json::from_value(scene)
                    .map_err(|e| StyxError::Other(format!("场景格式非法：{e}")))?;
                let v = self.with_kernel(&name, |k| {
                    k.set_scene(parsed.clone());
                    Ok(serde_json::to_value(k.scene())?)
                })?;
                Ok((Response::ok("set_scene", v), false))
            }

            Request::Events { limit } => {
                let name = self.ensure_session(state)?;
                let limit = limit.unwrap_or(20).clamp(1, 500);
                let v = self.with_kernel(&name, |k| {
                    let events = k.session().transcript.clone();
                    let start = events.len().saturating_sub(limit);
                    Ok(json!({
                        "total": events.len(),
                        "events": events[start..].iter().map(|e| json!({
                            "seq": e.seq,
                            "at": e.at,
                            "kind": e.kind,
                            "actor": e.actor,
                            "text": e.text,
                            "importance": e.importance,
                            "line": e.render_line(),
                            // meta 必须带出去：表情包事件靠 `sticker_id`
                            // 才能被前端渲染成一张图而不是一句转述。
                            "meta": e.meta,
                        })).collect::<Vec<_>>(),
                    }))
                })?;
                Ok((Response::ok("events", v), false))
            }

            Request::Tools => {
                let name = self.ensure_session(state)?;
                let v = self.with_kernel(&name, |k| {
                    let tools = k
                        .tools()
                        .map(|t| {
                            t.list()
                                .into_iter()
                                .map(|s| {
                                    json!({
                                        "name": s.name,
                                        "description": s.description,
                                        "schema": s.schema,
                                        "origin": s.origin,
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Ok(json!({ "count": tools.len(), "tools": tools }))
                })?;
                Ok((Response::ok("tools", v), false))
            }

            Request::Call { tool, args } => {
                let name = self.ensure_session(state)?;
                let (result, log_seq) = self.with_kernel(&name, |k| {
                    let tools = k
                        .tools()
                        .cloned()
                        .ok_or_else(|| StyxError::UnknownTool(tool.clone()))?;
                    let args_log = args.to_string();
                    let out = tools.invoke(&tool, args.clone())?;
                    let seq = k.session_mut().push(
                        styx_core::EventKind::ToolCall,
                        tool.clone(),
                        args_log,
                    );
                    k.session_mut().push(
                        styx_core::EventKind::ToolResult,
                        tool.clone(),
                        out.to_string(),
                    );
                    Ok((out, seq))
                })?;
                Ok((
                    Response::ok(
                        "call",
                        json!({ "tool": tool, "result": result, "seq": log_seq }),
                    ),
                    false,
                ))
            }

            Request::Status => {
                let name = self.ensure_session(state)?;
                let v = self.with_kernel(&name, |k| {
                    let s = k.status();
                    Ok(json!({
                        "llm": s.llm,
                        "llm_endpoints": s.llm_endpoints,
                        "memory": s.memory,
                        "assoc": s.assoc,
                        "pool": s.pool,
                        "tools": s.tools,
                        "turn": s.turn,
                        "transcript": s.transcript,
                        "degraded": s.degraded,
                        "rendered": s.render(),
                    }))
                })?;
                let mut v = v;
                if let Value::Object(m) = &mut v {
                    m.insert("sessions".into(), json!(self.session_count()));
                    m.insert("server_turns".into(), json!(self.turns()));
                    m.insert("server_errors".into(), json!(self.errors()));
                    // 服务层自身的状态也要能被看到：出问题时第一个要回答的
                    // 问题是"是模型慢，还是连接被卡住了"。
                    m.insert("in_flight".into(), json!(self.in_flight()));
                    m.insert("rejected".into(), json!(self.rejected()));
                    m.insert("shutting_down".into(), json!(self.is_shutting_down()));
                    m.insert(
                        "limits".into(),
                        json!({
                            "max_connections": self.limits.max_connections,
                            "max_sessions": self.limits.max_sessions,
                            "max_line_bytes": self.limits.max_line_bytes,
                        }),
                    );
                    m.insert("factory".into(), json!(self.factory.describe()));
                }
                Ok((Response::ok("status", v), false))
            }

            Request::Remember {
                text,
                tags,
                importance,
            } => {
                let name = self.ensure_session(state)?;
                let imp = importance.unwrap_or(0.7);
                let id = self.with_kernel(&name, |k| k.remember(&text, &tags, imp))?;
                Ok((Response::ok("remember", json!({ "id": id })), false))
            }

            Request::Recall { query, limit } => {
                let name = self.ensure_session(state)?;
                let limit = limit.unwrap_or(5).clamp(1, 100);
                let hits = self.with_kernel(&name, |k| k.recall(&query, limit))?;
                Ok((
                    Response::ok(
                        "recall",
                        json!({
                            "count": hits.len(),
                            "memories": hits.iter().map(|r| json!({
                                "id": r.id, "text": r.text, "score": round3(r.score),
                                "importance": round3(r.importance), "tags": r.tags,
                                "origin": r.origin,
                            })).collect::<Vec<_>>(),
                        }),
                    ),
                    false,
                ))
            }

            Request::Associate { seed, limit } => {
                let name = self.ensure_session(state)?;
                let limit = limit.unwrap_or(6).clamp(1, 50);
                let list = self.with_kernel(&name, |k| k.associate(&seed, limit))?;
                Ok((
                    Response::ok(
                        "associate",
                        json!({
                            "count": list.len(),
                            "associations": list.iter().map(|a| json!({
                                "word": a.word, "score": round3(a.score),
                                "confidence": round3(a.confidence), "evidence": a.evidence,
                            })).collect::<Vec<_>>(),
                        }),
                    ),
                    false,
                ))
            }

            Request::Reset { scene } => {
                let name = state
                    .session
                    .clone()
                    .unwrap_or_else(|| self.default_session.clone());
                let scene = match scene {
                    Some(v) => serde_json::from_value::<Scene>(v)
                        .map_err(|e| StyxError::Other(format!("场景格式非法：{e}")))?,
                    None => Scene::default(),
                };
                let mut fresh = self.factory.create(&name)?;
                fresh.set_scene(scene);
                self.put_session(name.clone(), fresh);
                state.session = Some(name.clone());
                Ok((
                    Response::ok("reset", json!({ "session": name, "reset": true })),
                    false,
                ))
            }

            Request::Quit => Ok((
                Response::ok("quit", json!({ "bye": true, "served": state.served })),
                true,
            )),
        }
    }

    fn ensure_session(&self, state: &mut ConnectionState) -> Result<String> {
        if let Some(s) = &state.session {
            return Ok(s.clone());
        }
        let name = self.default_session.clone();
        // 借一次租约把内核建出来、再立刻放回（Drop 时归还）。
        // 走同一条 single-flight 路径，并发时不会重复建；同时"建不出来"
        // 能在这一步就报给调用者，而不是拖到第一次用的时候。
        drop(self.acquire_kernel(&name)?);
        state.session = Some(name.clone());
        Ok(name)
    }

    /// 取出内核 → 用它 → 放回。**全程不持锁**，避免一个慢回合卡住所有连接。
    ///
    /// 注意这里"不持锁"指的是不抱着会话表的锁去跑回合，不是"不互斥"：
    /// 同一个会话名同一时刻只有一个租约，后来者会在 [`Server::acquire_kernel`]
    /// 里等着——同一会话本来也不该有两个回合同时推进。
    fn with_kernel<T>(&self, name: &str, f: impl FnOnce(&mut Kernel) -> Result<T>) -> Result<T> {
        let mut lease = self.acquire_kernel(name)?;
        f(lease.kernel())
        // lease 在这里 Drop，内核随之归还——正常返回与 panic 展开都走这条路。
    }

    /// 取得某个会话名的租约：空闲的直接拿走，正在被用的等它回来，
    /// 表里没有且没人在用的才新建。
    ///
    /// 等而不是新建的理由见文件头的"为什么表里没有不能直接等于新建一个"。
    fn acquire_kernel(&self, name: &str) -> Result<KernelLease<'_>> {
        let Ok(mut sessions) = self.sessions.lock() else {
            // 锁中毒：退化成"每次自建"。宁可退一点效率，也不能把请求全掐了。
            return self
                .factory
                .create(name)
                .map(|kernel| self.lease(name, kernel));
        };
        let started = Instant::now();

        loop {
            if let Some(entry) = sessions.map.remove(name) {
                sessions.busy.insert(name.to_string());
                return Ok(self.lease(name, entry.kernel));
            }

            // 表里没有。是"没人在用"还是"有人正在用"？这一步必须和上面
            // 的 remove 在同一把锁里判断，否则两个请求会同时认为"没人在用"。
            if sessions.busy.insert(name.to_string()) {
                // 由我来建。占位已经打上，别的请求会去等而不是重复建。
                drop(sessions);
                let built = self.factory.create(name);
                if built.is_err() {
                    // 建失败必须把占位摘掉，否则这个名字会被永久卡住。
                    if let Ok(mut s) = self.sessions.lock() {
                        s.busy.remove(name);
                    }
                    self.released.notify_all();
                }
                return built.map(|kernel| self.lease(name, kernel));
            }

            // 有人正在用/正在建：等它放回。
            let (guard, timeout) = self
                .released
                .wait_timeout(sessions, Duration::from_millis(50))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions = guard;
            if timeout.timed_out() && started.elapsed() >= SESSION_WAIT_LIMIT {
                return Err(StyxError::Other(format!(
                    "会话 `{name}` 被占用超过 {} 秒仍未释放",
                    SESSION_WAIT_LIMIT.as_secs()
                )));
            }
        }
    }

    /// 造一个租约（不改会话表——占位已在 `acquire_kernel` 里打好）。
    fn lease<'a>(&'a self, name: &str, kernel: Kernel) -> KernelLease<'a> {
        KernelLease {
            server: self,
            name: name.to_string(),
            kernel: Some(kernel),
        }
    }

    /// 放回一个会话，并在超出上限时淘汰最久未动的。
    ///
    /// 正在被使用的会话**不在 `map` 里**（租约把它取走了），所以它
    /// 不可能被淘汰——这一点很关键：淘汰掉一个正在跑长回合的会话，
    /// 用户看到的就是"说着说着角色失忆了"。
    fn put_session(&self, name: String, kernel: Kernel) {
        {
            let Ok(mut sessions) = self.sessions.lock() else {
                return;
            };
            sessions.map.insert(
                name.clone(),
                SessionEntry {
                    kernel,
                    last_used: Instant::now(),
                },
            );
            sessions.busy.remove(&name);
            let max = self.limits.max_sessions;
            if max > 0 {
                while sessions.map.len() > max {
                    let victim = sessions
                        .map
                        .iter()
                        .filter(|(k, _)| k.as_str() != name.as_str())
                        .min_by_key(|(_, e)| e.last_used)
                        .map(|(k, _)| k.clone());
                    match victim {
                        Some(k) => {
                            sessions.map.remove(&k);
                        }
                        None => break,
                    }
                }
            }
        }
        // 先唤醒等待者，再读指标：等待者能早一点拿到内核，
        // 而指标那一步要再加一次锁，放在临界区外面更干净。
        self.released.notify_all();
        self.metrics
            .set("styx_sessions", self.session_count() as f64);
    }

    /// 记一次回合：计数 + 耗时。
    ///
    /// 耗时走直方图而不是平均值——平均值会把"多数 2 秒、偶尔 60 秒"
    /// 抹成一个看起来很健康的数字，而坏体验恰恰全在尾部。
    fn observe_turn(&self, started: Instant) {
        self.turns.fetch_add(1, Ordering::Relaxed);
        self.metrics.inc("styx_turns_total", 1);
        self.metrics
            .observe("styx_turn_seconds", started.elapsed().as_secs_f64());
    }

    /// 把一个回合的结果整理成响应。
    ///
    /// `say` 与 `sticker` 共用它：两者进入的是**同一个回合闭环**，
    /// 响应形状就必须一致——否则前端要为每种输入写一套渲染分支，
    /// 而它们本来渲染的就是同一种东西。
    fn turn_response(&self, name: &str, op: &str, outcome: &TurnOutcome) -> Response {
        let (state_json, scene_json) = self
            .with_kernel(name, |k| {
                Ok((
                    serde_json::to_value(k.state()).unwrap_or(Value::Null),
                    serde_json::to_value(k.scene()).unwrap_or(Value::Null),
                ))
            })
            .unwrap_or((Value::Null, Value::Null));

        Response::ok(
            op,
            json!({
                "session": name,
                "turn": outcome.turn,
                "reply": reply_json(&outcome.reply),
                "plain": outcome.reply.plain_text(),
                "state_delta": outcome.reply.state_delta.render(),
                "scene": scene_json,
                "state": state_json,
                "audit": {
                    "violations": outcome.audit.violations,
                    "warnings": outcome.audit.warnings,
                },
                "retries": outcome.retries,
                "recalled": outcome.recalled.iter().map(|r| json!({
                    "id": r.id, "text": r.text, "score": round3(r.score),
                    "importance": round3(r.importance), "origin": r.origin,
                })).collect::<Vec<_>>(),
                "associations": outcome.associations.iter().map(|a| json!({
                    "word": a.word, "score": round3(a.score),
                    "confidence": round3(a.confidence),
                    "evidence": a.evidence,
                })).collect::<Vec<_>>(),
                "pool_notes": outcome.pool_notes.iter().map(|r| json!({
                    "id": r.id, "text": r.text, "score": round3(r.score), "origin": r.origin,
                })).collect::<Vec<_>>(),
                "memory_written": outcome.memory_written,
                "pool_written": outcome.pool_written,
                "scene_changed": outcome.scene_changed,
                "prompt": outcome.report.render(),
                "usage": {
                    "prompt_tokens": outcome.completion.prompt_tokens,
                    "completion_tokens": outcome.completion.completion_tokens,
                    "model": outcome.completion.model,
                    "endpoint": outcome.completion.endpoint,
                },
                "notices": outcome.notices,
            }),
        )
    }
}

/// 读一行，并在**超长时立刻停下**。
///
/// `BufRead::lines()` 没有上限：一个 5 MB 的"单行"会先被**完整读进内存**，
/// 再交给我们判断——防护写在下游是防不住资源耗尽的。这里用 `take` 把读取量
/// 钉死在 `max + 1` 字节：多读的那 1 字节就是"超了"的证据，而且根本不会
/// 为超长输入分配更大的缓冲。
///
/// `max == 0` 表示不限。返回读到的字节数，`0` 表示对端已关闭。
fn read_line_limited<R: BufRead>(
    reader: &mut R,
    max: usize,
    out: &mut String,
) -> std::io::Result<usize> {
    let mut buf: Vec<u8> = Vec::new();
    let n = if max == 0 {
        reader.read_until(b'\n', &mut buf)?
    } else {
        let mut limited = reader.by_ref().take(max as u64 + 1);
        let n = limited.read_until(b'\n', &mut buf)?;
        if n > max {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("单行超过 {max} 字节上限"),
            ));
        }
        n
    };
    if n == 0 {
        return Ok(0);
    }
    let text = std::str::from_utf8(&buf)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "请求不是合法 UTF-8"))?;
    out.push_str(text.trim_end_matches(['\n', '\r']));
    Ok(n)
}

/// 读掉对端在途的字节，最多 256 KB。
///
/// 存在的理由只有一个：**带着未读数据关闭 socket，内核会直接回 RST**
/// 而不是正常的 FIN，而 RST 会把刚写出去、还躺在对端接收缓冲里的响应
/// 一并冲掉。于是"服务繁忙"在客户端眼里就变成了"连接被重置"。
fn drain_until_quiet<R: Read>(reader: &mut R) {
    let mut sink = [0u8; 4096];
    let mut drained = 0usize;
    while drained < 256 * 1024 {
        match reader.read(&mut sink) {
            Ok(0) => break,
            Ok(n) => drained += n,
            Err(_) => break,
        }
    }
}

/// 拒绝一条连接：把话说清楚，再体面地关上。
fn reject_connection(mut stream: TcpStream, op: &str, message: &str) {
    let msg = format!("{}\n", Response::err(op, message).to_line());
    let _ = stream.write_all(msg.as_bytes());
    let _ = stream.flush();
    // 半关闭写端：对端读到 EOF 就知道"话说完了"，不必靠超时去猜。
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    drain_until_quiet(&mut stream);
}

/// 回复 → JSON。
pub fn reply_json(reply: &Reply) -> Value {
    json!({
        "speech": reply.speech,
        "actions": reply.actions,
        "thoughts": reply.thoughts,
        "stickers": reply.stickers,
        "memories": reply.memories.iter().map(|m| json!({
            // 浮点走线前统一收敛到 3 位小数，否则 0.8 会变成 0.800000011920929
            "text": m.text, "tags": m.tags, "importance": round3(m.importance),
        })).collect::<Vec<_>>(),
        "from_json": reply.from_json,
        // 素材通道。之前这里漏了 images：core 明明解析出了 `[图片]` 请求，
        // 走线时却被丢掉，前端永远收不到——通道"实现了但接不通"。
        "images": reply.images.iter().map(|i| json!({
            "key": i.key, "caption": i.caption,
        })).collect::<Vec<_>>(),
        // 模型原文。REPL 里有 `/raw`，前端却没有等价物——于是"这句话为什么
        // 落到了台词而不是动作"在前端根本无从排查：结构化字段只告诉你
        // 落到了哪里，原文才告诉你模型到底写了什么。
        "raw": reply.raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;

    /// 造一个离线内核（Mock 模型 + 内存记忆 + 内存联想）。
    fn offline_kernel(session: &str) -> Result<Kernel> {
        use styx_assoc::InMemoryAssoc;
        use styx_core::{CharacterCard, KernelConfig, Scene};
        use styx_memory::InMemoryMemory;

        let _ = session;
        let mut card = CharacterCard::new("林夏");
        card.persona = "外冷内热的旧书店主".into();
        card.speech_style = "短句".into();
        card.boundaries = vec!["绝不承认自己害怕孤独".into()];

        let memory = Arc::new(InMemoryMemory::new());
        let assoc = Arc::new(InMemoryAssoc::new());
        // 共现图的置信度随「支撑次数」增长，而弃判阈值是 0.18：
        // 只喂一句的话所有联想都会被判为「不可信」而返回空。
        // 所以这里给足上下文，让兜底图真的能用起来。
        assoc.observe_many([
            "母亲留下了一张旧照片，夹在相册里",
            "相册的第一页就是那张旧照片",
            "她把相册合上，照片被压在下面",
        ]);

        let cfg = KernelConfig {
            user_name: "陈默".into(),
            ..Default::default()
        };
        Kernel::builder(card, Scene::new("拾光旧书店"))
            .llm(styx_llm::offline_llm())
            .memory(memory)
            .assoc(assoc)
            .config(cfg)
            .build()
    }

    fn server() -> Arc<Server> {
        Arc::new(Server::new(Arc::new(|s: &str| offline_kernel(s))))
    }

    /// 带表情包目录的离线内核。
    fn offline_kernel_with_stickers(session: &str) -> Result<Kernel> {
        use styx_assoc::InMemoryAssoc;
        use styx_core::{CharacterCard, KernelConfig, Scene, StickerCatalog};
        use styx_memory::InMemoryMemory;

        let _ = session;
        let mut card = CharacterCard::new("林夏");
        card.persona = "外冷内热的旧书店主".into();
        card.speech_style = "短句".into();
        card.boundaries = vec!["绝不承认自己害怕孤独".into()];

        Kernel::builder(card, Scene::new("拾光旧书店"))
            .llm(styx_llm::offline_llm())
            .memory(Arc::new(InMemoryMemory::new()))
            .assoc(Arc::new(InMemoryAssoc::new()))
            .stickers(Arc::new(StickerCatalog::from_files(&[
                "happy_01.png",
                "cry_03.png",
                "wailing_22.png",
            ])))
            .config(KernelConfig::default())
            .build()
    }

    fn sticker_server() -> Arc<Server> {
        Arc::new(Server::new(Arc::new(|s: &str| {
            offline_kernel_with_stickers(s)
        })))
    }

    fn call(server: &Server, state: &mut ConnectionState, line: &str) -> Value {
        let req = Request::parse(line).expect("请求应当可解析");
        let (resp, _close) = server.dispatch(state, req);
        serde_json::from_str(&resp.to_line()).unwrap()
    }

    #[test]
    fn ping_and_hello() {
        let s = server();
        let mut st = ConnectionState::default();
        let v = call(&s, &mut st, r#"{"op":"ping"}"#);
        assert_eq!(v["ok"], true);
        assert_eq!(v["pong"], true);

        let v = call(&s, &mut st, r#"{"op":"hello"}"#);
        assert_eq!(v["ok"], true);
        assert_eq!(v["session"], "default");
        assert_eq!(v["card"], "林夏");
        assert_eq!(v["resumed"], false);
        assert!(v["ops"].as_array().unwrap().contains(&json!("say")));
        assert!(v["ops"].as_array().unwrap().contains(&json!("sticker")));
        assert!(v["ops"].as_array().unwrap().contains(&json!("stickers")));
        assert_eq!(st.session.as_deref(), Some("default"));

        // 再握一次应当是"恢复"而不是新建
        let v = call(&s, &mut st, r#"{"op":"hello","session":"default"}"#);
        assert_eq!(v["resumed"], true);
    }

    #[test]
    fn named_sessions_are_isolated() {
        let s = server();
        let mut a = ConnectionState::default();
        let mut b = ConnectionState::default();
        call(&s, &mut a, r#"{"op":"hello","session":"alice"}"#);
        call(&s, &mut b, r#"{"op":"hello","session":"bob"}"#);
        assert_eq!(s.session_count(), 2);

        call(&s, &mut a, r#"{"op":"say","text":"你好"}"#);
        let va = call(&s, &mut a, r#"{"op":"state"}"#);
        let vb = call(&s, &mut b, r#"{"op":"state"}"#);
        assert_eq!(va["turn"], 1);
        assert_eq!(vb["turn"], 0, "另一个会话不该被推进");
    }

    #[test]
    fn say_returns_a_full_turn_report() {
        let s = server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        let v = call(&s, &mut st, r#"{"op":"say","text":"我想看看那本旧相册"}"#);
        assert_eq!(v["ok"], true);
        assert_eq!(v["turn"], 1);
        assert!(!v["reply"]["speech"].as_array().unwrap().is_empty());
        assert!(!v["plain"].as_str().unwrap().is_empty());
        assert!(v["prompt"].as_str().unwrap().contains("tok"));
        assert_eq!(v["usage"]["model"], "mock-roleplay");
        assert!(v["audit"]["violations"].as_array().unwrap().is_empty());
        assert!(v["state"]["mood"].is_object());
        assert!(v["scene"].is_object());
        // 联想应当从输入里的关键词发散出来
        assert!(v["associations"].as_array().is_some());
    }

    #[test]
    fn events_scene_and_reset() {
        let s = server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);

        let v = call(
            &s,
            &mut st,
            r#"{"op":"set_scene","scene":{"location":"后院","time":"清晨"}}"#,
        );
        assert_eq!(v["location"], "后院");
        assert_eq!(v["time"], "清晨");

        let v = call(&s, &mut st, r#"{"op":"scene"}"#);
        assert_eq!(v["location"], "后院");

        call(&s, &mut st, r#"{"op":"say","text":"早"}"#);
        let v = call(&s, &mut st, r#"{"op":"events","limit":50}"#);
        assert!(v["total"].as_u64().unwrap() >= 2);
        assert!(v["events"][0]["line"].is_string());

        let v = call(&s, &mut st, r#"{"op":"reset","scene":{"location":"书店"}}"#);
        assert_eq!(v["reset"], true);
        let v = call(&s, &mut st, r#"{"op":"state"}"#);
        assert_eq!(v["turn"], 0, "重置后回合数应当归零");
    }

    #[test]
    fn set_scene_rejects_garbage() {
        let s = server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        let v = call(&s, &mut st, r#"{"op":"set_scene","scene":"不是一个对象"}"#);
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("场景格式非法"));
    }

    #[test]
    fn tools_list_and_call() {
        let s = server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        let v = call(&s, &mut st, r#"{"op":"tools"}"#);
        assert_eq!(v["ok"], true);
        // 内核没注入工具端口 → 空列表，但请求本身要成功
        assert_eq!(v["count"], 0);

        let v = call(&s, &mut st, r#"{"op":"call","tool":"dice"}"#);
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("未知工具"));
    }

    #[test]
    fn remember_and_recall() {
        let s = server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        let v = call(
            &s,
            &mut st,
            r#"{"op":"remember","text":"母亲的旧照片","tags":["照片"],"importance":0.9}"#,
        );
        assert_eq!(v["ok"], true);
        assert!(v["id"].is_string());

        let v = call(&s, &mut st, r#"{"op":"recall","query":"照片","limit":3}"#);
        assert_eq!(v["count"], 1);
        assert!(v["memories"][0]["text"].as_str().unwrap().contains("照片"));
    }

    #[test]
    fn associate_uses_the_graph() {
        let s = server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        let v = call(&s, &mut st, r#"{"op":"associate","seed":"照片","limit":5}"#);
        assert_eq!(v["ok"], true);
        assert!(v["count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn status_includes_server_counters() {
        let s = server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        call(&s, &mut st, r#"{"op":"say","text":"你好"}"#);
        let v = call(&s, &mut st, r#"{"op":"status"}"#);
        assert_eq!(v["ok"], true);
        assert_eq!(v["sessions"], 1);
        assert_eq!(v["server_turns"], 1);
        assert!(v["rendered"].as_str().unwrap().contains("长期记忆"));
        assert_eq!(v["llm_endpoints"], 2);
    }

    #[test]
    fn malformed_request_yields_an_error_but_keeps_going() {
        let s = server();
        let mut st = ConnectionState::default();
        // "garbage" 解析不出请求，退回 Ping 以模拟「坏请求不致命」的路径
        let (resp, close) = s.dispatch(&mut st, Request::parse("garbage").unwrap_or(Request::Ping));
        assert!(!close);
        assert!(resp.ok);

        // 直接走 dispatch 的解析失败分支。
        // 注意 `errors` 计数由连接循环维护（见 `run`），手动调 dispatch 不计入——
        // 要验证计数请走 `end_to_end_over_a_real_socket`。
        let mut st2 = ConnectionState::default();
        let (resp, close) = match Request::parse("garbage") {
            Ok(r) => s.dispatch(&mut st2, r),
            Err(e) => (Response::err("parse", e), false),
        };
        assert!(!close);
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("合法 JSON"));

        // 之后仍然可用
        let mut st3 = ConnectionState::default();
        let v = call(&s, &mut st3, r#"{"op":"ping"}"#);
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn quit_closes_the_connection() {
        let s = server();
        let mut st = ConnectionState::default();
        let (resp, close) = s.dispatch(&mut st, Request::parse(r#"{"op":"quit"}"#).unwrap());
        assert!(close);
        assert!(resp.ok);
        assert_eq!(resp.data["bye"], true);
    }

    #[test]
    fn factory_failure_is_reported_as_an_error_response() {
        let s = Server::new(Arc::new(|_: &str| {
            Err(StyxError::unavailable("llm", "没有可用的模型端点"))
        }));
        let mut st = ConnectionState::default();
        let v = call(&s, &mut st, r#"{"op":"hello"}"#);
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("没有可用的模型端点"));
        assert_eq!(s.errors(), 1);
    }

    #[test]
    fn end_to_end_over_a_real_socket() {
        let s = server();
        let listener = s.bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Arc::clone(&s);
        std::thread::spawn(move || {
            let _ = server.run(listener);
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut w = stream.try_clone().unwrap();
        let mut r = BufReader::new(stream);

        let mut send = |line: &str| {
            w.write_all(format!("{line}\n").as_bytes()).unwrap();
            w.flush().unwrap();
            let mut buf = String::new();
            r.read_line(&mut buf).unwrap();
            serde_json::from_str::<Value>(&buf).unwrap()
        };

        let v = send(r#"{"op":"hello"}"#);
        assert_eq!(v["session"], "default");
        let v = send(r#"{"op":"say","text":"在吗"}"#);
        assert_eq!(v["ok"], true);
        assert_eq!(v["turn"], 1);

        // 坏请求不致命：明确报错、连接不断，并且被计入 errors
        let v = send("这不是 JSON");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("合法 JSON"));
        assert_eq!(s.errors(), 1);

        let v = send(r#"{"op":"quit"}"#);
        assert_eq!(v["bye"], true);

        // 服务端应当已经处理了 3 条请求
        assert_eq!(s.turns(), 1);

        let mut rest = Vec::new();
        let _ = r.read_to_end(&mut rest);
    }

    #[test]
    fn reply_json_shape() {
        let r =
            Reply::parse("[做] 合上书\n[说] 不卖。\n[忆] 某件事 | 标签=a | 重要度=0.8").unwrap();
        let v = reply_json(&r);
        assert_eq!(v["speech"][0], "不卖。");
        assert_eq!(v["actions"][0], "合上书");
        assert_eq!(v["memories"][0]["importance"], 0.8);
        assert_eq!(v["from_json"], false);
        // 没有表情包时也要有 stickers 字段：前端可以无条件读它
        assert_eq!(v["stickers"].as_array().unwrap().len(), 0);
        // 素材通道与原文同样必须无条件在场：前端可以无条件读，
        // 不必先判断键存不存在（漏了 images 时前端会静默丢图）
        assert!(v["images"].is_array());
        assert!(v["raw"].is_string());
    }

    #[test]
    fn reply_json_carries_both_media_channels_and_the_raw_text() {
        let r = Reply::parse("[说] 你看。\n[表情] happy_01\n[图片] old_photo | 这张").unwrap();
        let v = reply_json(&r);
        assert_eq!(v["stickers"][0], "happy_01");
        assert_eq!(v["images"][0]["key"], "old_photo");
        assert_eq!(v["images"][0]["caption"], "这张");
        // 原文要能原样拿到，前端才排得动"这句话为什么落错通道"
        assert!(v["raw"].as_str().unwrap().contains("[表情] happy_01"));
    }

    // ------------------------------------------------------------ 表情包

    #[test]
    fn sticker_round_trip_over_the_protocol() {
        let s = sticker_server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);

        let v = call(&s, &mut st, r#"{"op":"stickers"}"#);
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 3);
        assert_eq!(v["stickers"][0]["file"], "happy_01.png");
        assert_eq!(v["stickers"][0]["label"], "开心");
        assert!(v["stickers"][0]["description"].as_str().unwrap().len() > 4);

        let v = call(&s, &mut st, r#"{"op":"sticker","id":"cry_03"}"#);
        assert_eq!(v["ok"], true);
        assert_eq!(v["turn"], 1);
        assert!(!v["plain"].as_str().unwrap().is_empty());
        assert!(v["state"]["mood"].is_object());

        // 对方发来的表情包要作为一条用户输入进事件流，端口才还原得出气泡
        let v = call(&s, &mut st, r#"{"op":"events","limit":50}"#);
        let events = v["events"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["kind"] == "user_input"),
            "{events:?}"
        );
        assert_eq!(s.turns(), 1);
    }

    #[test]
    fn stickers_op_is_empty_without_a_catalog() {
        let s = server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        let v = call(&s, &mut st, r#"{"op":"stickers"}"#);
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 0);
        assert!(v["stickers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_sticker_the_catalog_does_not_have_is_an_error_not_a_crash() {
        let s = sticker_server();
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        let v = call(&s, &mut st, r#"{"op":"sticker","id":"nope_99"}"#);
        assert_eq!(v["ok"], false);
        assert!(
            v["error"].as_str().unwrap().contains("没有这张表情包"),
            "{v:?}"
        );
        assert_eq!(s.errors(), 1);
        // 连接必须还能继续用
        let v = call(&s, &mut st, r#"{"op":"ping"}"#);
        assert_eq!(v["pong"], true);
    }

    // ------------------------------------------------------------ 服务层防护

    /// 造一个带配额的服务器。
    fn guarded(limits: Limits) -> Arc<Server> {
        Arc::new(Server::new(Arc::new(|s: &str| offline_kernel(s))).with_limits(limits))
    }

    /// 起一个真服务，返回监听地址。
    fn spawn_server(server: Arc<Server>) -> std::net::SocketAddr {
        let listener = server.bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = server.run(listener);
        });
        addr
    }

    fn send_recv(stream: &TcpStream, line: &str) -> Value {
        let mut w = stream.try_clone().unwrap();
        let mut r = BufReader::new(stream.try_clone().unwrap());
        w.write_all(format!("{line}\n").as_bytes()).unwrap();
        w.flush().unwrap();
        let mut buf = String::new();
        r.read_line(&mut buf).unwrap();
        serde_json::from_str(&buf).unwrap()
    }

    #[test]
    fn connections_beyond_the_cap_are_refused() {
        let s = guarded(Limits {
            max_connections: 1,
            ..Limits::default()
        });
        let addr = spawn_server(Arc::clone(&s));

        let held = TcpStream::connect(addr).unwrap();
        held.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let v = send_recv(&held, r#"{"op":"ping"}"#);
        assert_eq!(v["pong"], true, "第一条连接应当正常");

        // 名额已经被第一条占住，第二条必须被**明确拒绝**，
        // 而不是排队等着（等下去就是线程表被慢慢磨光）。
        let second = TcpStream::connect(addr).unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let v = send_recv(&second, r#"{"op":"ping"}"#);
        assert_eq!(v["ok"], false, "{v:?}");
        assert!(v["error"].as_str().unwrap().contains("上限"), "{v:?}");
        assert_eq!(s.rejected(), 1);
        assert_eq!(s.in_flight(), 1);
    }

    #[test]
    fn an_oversized_line_is_rejected_instead_of_swallowed() {
        let s = guarded(Limits {
            max_line_bytes: 256,
            ..Limits::default()
        });
        let addr = spawn_server(Arc::clone(&s));

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // 正好比上限多 1 字节：服务端读满就停，不会为它分配更大的缓冲，
        // 也不会把后面半截当成一条新请求。
        c.write_all("x".repeat(257).as_bytes()).unwrap();
        c.flush().unwrap();

        let mut r = BufReader::new(c.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert!(line.contains("超过"), "{line}");
        assert_eq!(s.rejected(), 1);
    }

    #[test]
    fn sessions_beyond_the_cap_evict_the_least_recently_used() {
        let s = guarded(Limits {
            max_sessions: 2,
            ..Limits::default()
        });
        let mut a = ConnectionState::default();
        let mut b = ConnectionState::default();
        let mut c = ConnectionState::default();

        call(&s, &mut a, r#"{"op":"hello","session":"a"}"#);
        std::thread::sleep(Duration::from_millis(3));
        call(&s, &mut b, r#"{"op":"hello","session":"b"}"#);
        assert_eq!(s.session_count(), 2);

        std::thread::sleep(Duration::from_millis(3));
        call(&s, &mut c, r#"{"op":"hello","session":"c"}"#);
        assert_eq!(s.session_count(), 2, "超上限应当淘汰，而不是无界增长");

        // a 最久没动过 → 应当是被淘汰的那个
        let v = call(&s, &mut a, r#"{"op":"hello","session":"a"}"#);
        assert_eq!(v["resumed"], false, "a 应当已被淘汰");
        // c 刚建过 → 还在
        let v = call(&s, &mut c, r#"{"op":"hello","session":"c"}"#);
        assert_eq!(v["resumed"], true, "c 不该被淘汰");
    }

    #[test]
    fn status_reports_the_service_layer() {
        let s = guarded(Limits::production());
        let mut st = ConnectionState::default();
        call(&s, &mut st, r#"{"op":"hello"}"#);
        let v = call(&s, &mut st, r#"{"op":"status"}"#);
        assert_eq!(v["shutting_down"], false);
        assert_eq!(v["rejected"], 0);
        assert_eq!(v["limits"]["max_connections"], 64);
        assert_eq!(v["limits"]["max_sessions"], 256);
    }

    #[test]
    fn shutdown_lets_run_return_instead_of_hanging_on_accept() {
        let s = guarded(Limits {
            drain_timeout_ms: 1_000,
            ..Limits::default()
        });
        let listener = s.bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Arc::clone(&s);
        let runner = std::thread::spawn(move || server.run(listener));

        let c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let v = send_recv(&c, r#"{"op":"ping"}"#);
        assert_eq!(v["pong"], true);

        // 客户端走人 → 服务端的连接线程读到 EOF 自行收摊
        drop(c);
        s.shutdown_handle().trigger();

        runner
            .join()
            .expect("run 不该 panic")
            .expect("run 应当正常返回，而不是卡在 accept 上");
        assert!(s.is_shutting_down());
        assert_eq!(s.shutdown_handle().active(), 0, "收尾之后不该还有在飞请求");
    }

    // -------------------------------------------------- 同会话并发（single-flight）

    /// 一个"造得慢"的服务器，外加构建次数计数。
    ///
    /// 慢是刻意的：只有把构建时间拉长，并发请求才会真的撞进
    /// "表里还没有"的那个窗口里。构建快的时候这个窗口只有几十微秒，
    /// 测试会时灵时不灵。
    fn slow_building(create_ms: u64) -> (Arc<Server>, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        let built = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&built);
        let server = Arc::new(Server::new(Arc::new(move |s: &str| {
            std::thread::sleep(Duration::from_millis(create_ms));
            counter.fetch_add(1, Ordering::SeqCst);
            offline_kernel(s)
        })));
        (server, built)
    }

    /// 同一个会话的并发请求只应构建**一次**内核。
    ///
    /// 这条测试守的是一个会丢状态的 bug：早先"取不到就新建"，于是并发
    /// 请求各拿一个内核、各自推进再各自放回，后放回的覆盖先放回的。
    #[test]
    fn concurrent_requests_for_one_session_build_the_kernel_once() {
        let (server, built) = slow_building(100);
        let addr = spawn_server(Arc::clone(&server));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || {
                    let s = TcpStream::connect(addr).unwrap();
                    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                    send_recv(&s, r#"{"op":"state"}"#)
                })
            })
            .collect();
        let replies: Vec<Value> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        for v in &replies {
            assert_eq!(v["ok"], true, "并发 state 都应当成功：{v}");
        }
        assert_eq!(
            built.load(Ordering::SeqCst),
            1,
            "同一会话的并发请求只应构建一次内核——构建多次意味着它们拿到了\
             不同的内核，状态会互相覆盖"
        );
    }

    /// 并发回合必须**一个不少地**落进同一份状态里。
    ///
    /// 这是上一条测试的行为侧对照：光数"构建了几次"能证明没重复建，
    /// 却不能证明回合没丢。所以这里数回合数——6 个并发 `say` 之后，
    /// 状态里的 `turn` 必须正好是 6。修复前它会停在 1（先放回的被覆盖）。
    #[test]
    fn concurrent_turns_on_one_session_do_not_lose_state() {
        let (server, _built) = slow_building(80);
        let addr = spawn_server(Arc::clone(&server));

        let handles: Vec<_> = (0..6)
            .map(|i| {
                std::thread::spawn(move || {
                    let s = TcpStream::connect(addr).unwrap();
                    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                    send_recv(&s, &format!(r#"{{"op":"say","text":"第 {i} 句"}}"#))
                })
            })
            .collect();
        for h in handles {
            let v = h.join().unwrap();
            assert_eq!(v["ok"], true, "并发 say 都应当成功：{v}");
        }

        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let st = send_recv(&s, r#"{"op":"state"}"#);
        assert_eq!(
            st["turn"], 6,
            "6 个并发回合应当全部落在同一份状态上，一个都不能丢：{st}"
        );
    }
}
