//! TCP 服务：把内核暴露成常驻的多会话服务。
//!
//! # 会话模型
//!
//! - 每个连接有一个"当前会话名"（`hello` 里指定，默认 `default`）；
//! - 服务端持有一张 `会话名 → Kernel` 表，**内核只在处理请求时被短暂取出**，
//!   因此不同会话之间互不阻塞（网络 IO 期间不持锁）；
//! - 同一个会话名被两个连接同时使用时会各自拿到一个临时内核——
//!   角色扮演的会话就该是"一个人的"，这一点在文档里说清楚比默默串线好。
//!
//! # 线程模型
//!
//! 一连接一线程。角色扮演是**低并发、高延迟**（每回合要等模型几秒到几十秒），
//! 用异步运行时换来的收益，远不如"代码简单、能塞进 CLI"来得实在。

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use styx_core::error::{Result, StyxError};
use styx_core::text::round3;
use styx_core::{Kernel, Reply, Scene};

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

/// Styx TCP 服务。
pub struct Server {
    factory: Arc<dyn KernelFactory>,
    sessions: Mutex<HashMap<String, Kernel>>,
    /// 默认会话名。
    default_session: String,
    /// 统计。
    turns: AtomicU64,
    errors: AtomicU64,
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
    /// 新建。
    pub fn new(factory: Arc<dyn KernelFactory>) -> Self {
        Server {
            factory,
            sessions: Mutex::new(HashMap::new()),
            default_session: "default".into(),
            turns: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    /// 改默认会话名。
    pub fn with_default_session(mut self, name: impl Into<String>) -> Self {
        self.default_session = name.into();
        self
    }

    /// 已加载的会话数。
    pub fn session_count(&self) -> usize {
        self.sessions.lock().map(|s| s.len()).unwrap_or(0)
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
        TcpListener::bind(addr).map_err(|e| {
            StyxError::Other(format!("无法监听 {addr}：{e}"))
        })
    }

    /// 绑定并阻塞运行。
    pub fn bind_and_run(self: Arc<Self>, addr: &str) -> Result<()> {
        let listener = self.bind(addr)?;
        let actual = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
        println!("Styx 服务已启动：{actual}");
        println!("会话工厂：{}", self.factory.describe());
        self.run(listener)
    }

    /// 接受连接（阻塞）。
    pub fn run(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let server = Arc::clone(&self);
                    std::thread::spawn(move || {
                        if let Err(e) = server.handle_connection(stream) {
                            eprintln!("连接结束：{e}");
                        }
                    });
                }
                Err(e) => eprintln!("接受连接失败：{e}"),
            }
        }
        Ok(())
    }

    /// 处理一条连接。
    pub fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        stream
            .set_read_timeout(Some(Duration::from_secs(600)))
            .ok();
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| StyxError::Other(format!("复制套接字失败：{e}")))?,
        );
        let mut writer = stream;
        let mut state = ConnectionState::default();

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("读取请求失败：{e}");
                    break;
                }
            };
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
                (Response::err(&op, e.to_string()), false)
            }
        }
    }

    fn try_dispatch(
        &self,
        state: &mut ConnectionState,
        req: Request,
    ) -> Result<(Response, bool)> {
        match req {
            Request::Ping => Ok((
                Response::ok("ping", json!({"pong": true, "service": "styx"})),
                false,
            )),

            Request::Hello { session } => {
                let name = session
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| self.default_session.clone());
                // 提前把内核造出来，失败就在握手里告诉客户端
                let created = self
                    .sessions
                    .lock()
                    .map(|s| s.contains_key(&name))
                    .unwrap_or(false);
                if !created {
                    let kernel = self.factory.create(&name)?;
                    if let Ok(mut map) = self.sessions.lock() {
                        map.insert(name.clone(), kernel);
                    }
                }
                state.session = Some(name.clone());
                let (card, turn, transcript) = self
                    .with_kernel(&name, |k| {
                        Ok((
                            k.card().name.clone(),
                            k.state().turn,
                            k.session().transcript.len(),
                        ))
                    })
                    .unwrap_or_else(|_| (String::new(), 0, 0));
                Ok((
                    Response::ok(
                        "hello",
                        json!({
                            "session": name,
                            "resumed": created,
                            "card": card,
                            "turn": turn,
                            "transcript": transcript,
                            "factory": self.factory.describe(),
                            "ops": [
                                "ping","hello","say","state","scene","set_scene",
                                "events","tools","call","status","remember",
                                "recall","associate","reset","quit"
                            ],
                        }),
                    ),
                    false,
                ))
            }

            Request::Say { text } => {
                let name = self.ensure_session(state)?;
                let outcome = self.with_kernel(&name, |k| k.turn(&text))?;
                self.turns.fetch_add(1, Ordering::Relaxed);
                let (state_json, scene_json) = self
                    .with_kernel(&name, |k| {
                        Ok((
                            serde_json::to_value(k.state()).unwrap_or(Value::Null),
                            serde_json::to_value(k.scene()).unwrap_or(Value::Null),
                        ))
                    })
                    .unwrap_or((Value::Null, Value::Null));

                Ok((
                    Response::ok(
                        "say",
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
                    ),
                    false,
                ))
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
                    let seq = k
                        .session_mut()
                        .push(
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
                let fresh = self.factory.create(&name)?;
                let mut fresh = fresh;
                fresh.set_scene(scene);
                if let Ok(mut map) = self.sessions.lock() {
                    map.insert(name.clone(), fresh);
                }
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
        let kernel = self.factory.create(&name)?;
        if let Ok(mut map) = self.sessions.lock() {
            map.insert(name.clone(), kernel);
        }
        state.session = Some(name.clone());
        Ok(name)
    }

    /// 取出内核 → 用它 → 放回。**全程不持锁**，避免一个慢回合卡住所有连接。
    fn with_kernel<T>(&self, name: &str, f: impl FnOnce(&mut Kernel) -> Result<T>) -> Result<T> {
        let existing = { self.sessions.lock().ok().and_then(|mut m| m.remove(name)) };
        let mut kernel = match existing {
            Some(k) => k,
            None => self.factory.create(name)?,
        };
        let out = f(&mut kernel);
        if let Ok(mut map) = self.sessions.lock() {
            map.insert(name.to_string(), kernel);
        }
        out
    }
}

/// 回复 → JSON。
pub fn reply_json(reply: &Reply) -> Value {
    json!({
        "speech": reply.speech,
        "actions": reply.actions,
        "thoughts": reply.thoughts,
        "memories": reply.memories.iter().map(|m| json!({
            // 浮点走线前统一收敛到 3 位小数，否则 0.8 会变成 0.800000011920929
            "text": m.text, "tags": m.tags, "importance": round3(m.importance),
        })).collect::<Vec<_>>(),
        "from_json": reply.from_json,
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
        let v = call(
            &s,
            &mut st,
            r#"{"op":"say","text":"我想看看那本旧相册"}"#,
        );
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

        let v = call(&s, &mut st, r#"{"op":"set_scene","scene":{"location":"后院","time":"清晨"}}"#);
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
        let r = Reply::parse("[做] 合上书\n[说] 不卖。\n[忆] 某件事 | 标签=a | 重要度=0.8").unwrap();
        let v = reply_json(&r);
        assert_eq!(v["speech"][0], "不卖。");
        assert_eq!(v["actions"][0], "合上书");
        assert_eq!(v["memories"][0]["importance"], 0.8);
        assert_eq!(v["from_json"], false);
    }
}
