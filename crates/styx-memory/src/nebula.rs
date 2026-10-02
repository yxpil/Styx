//! [`MemoryPort`] 的 Nebula 实现。
//!
//! 通过 Nebula 的 TCP 加密服务访问长期记忆。之所以走**网络协议**而不是
//! 直接依赖 `nebula-engine` crate：
//!
//! - 记忆库是一个**独立的数据资产**，应当由它的主人（Nebula 进程）持有，
//!   而不是被嵌进每一个使用它的程序里；
//! - Nebula 自己就是这么设计的（单文件 `.ndb` + `serve` 多客户端）；
//! - Styx 因此可以零硬依赖地编译，离线时自动降级到内存实现。
//!
//! 但"走协议"不等于"弱集成"：这里复刻了 Nebula 的 NEBULA2 握手、
//! Argon2id/HKDF/HMAC 挑战应答与 ChaCha20-Poly1305 加密帧，
//! 与官方客户端在字节层面互通。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use styx_core::ports::{MemoryNote, MemoryPort, Recalled};

use crate::error::{MemoryError, Result};
use styx_core::error::Result as StyxResult;
use crate::schema;
use crate::wire::NebulaClient;

/// Nebula 连接配置。
#[derive(Debug, Clone)]
pub struct NebulaConfig {
    /// 服务地址，例如 `127.0.0.1:7878`。
    pub addr: String,
    /// 用户名（单文件模式固定为 `admin`）。
    pub user: String,
    /// 密码（也可用 `--password-env` 从环境变量读取，避免写进配置）。
    pub password: String,
    /// 角色命名空间，用于 `source` 标记与状态展示。
    pub namespace: String,
    /// 连接超时。
    pub timeout: Duration,
    /// 出错时是否自动重连一次。
    pub auto_reconnect: bool,
}

impl Default for NebulaConfig {
    fn default() -> Self {
        NebulaConfig {
            addr: "127.0.0.1:7878".into(),
            user: "admin".into(),
            password: String::new(),
            namespace: "styx".into(),
            timeout: Duration::from_secs(10),
            auto_reconnect: true,
        }
    }
}

impl NebulaConfig {
    /// 从环境变量补齐密码：`NEBULA_PASSWORD` / `NEBULA_USER` / `NEBULA_ADDR`。
    ///
    /// 与 Nebula 官方 CLI 的环境变量名保持一致。
    pub fn with_env(mut self) -> Self {
        if self.password.is_empty() {
            if let Ok(p) = std::env::var("NEBULA_PASSWORD") {
                self.password = p;
            }
        }
        if let Ok(u) = std::env::var("NEBULA_USER") {
            if self.user == "admin" && !u.is_empty() {
                self.user = u;
            }
        }
        if let Ok(a) = std::env::var("NEBULA_ADDR") {
            if self.addr == "127.0.0.1:7878" && !a.is_empty() {
                self.addr = a;
            }
        }
        self
    }
}

/// 基于 Nebula 的长期记忆。
pub struct NebulaMemory {
    cfg: NebulaConfig,
    inner: Mutex<Option<NebulaClient>>,
    connected: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl std::fmt::Debug for NebulaMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NebulaMemory")
            .field("addr", &self.cfg.addr)
            .field("connected", &self.connected.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl NebulaMemory {
    /// 立即连接。连接失败时返回 Err，调用方可据此降级到内存实现。
    pub fn connect(cfg: NebulaConfig) -> Result<Self> {
        let cfg = cfg.with_env();
        if cfg.password.is_empty() {
            return Err(MemoryError::Config(
                "未提供 Nebula 密码（可用配置项或环境变量 NEBULA_PASSWORD）".into(),
            ));
        }
        let client = NebulaClient::connect_as(
            &cfg.addr,
            &cfg.user,
            &cfg.password,
            cfg.timeout,
        )?;
        Ok(NebulaMemory {
            cfg,
            inner: Mutex::new(Some(client)),
            connected: AtomicBool::new(true),
            last_error: Mutex::new(None),
        })
    }

    /// 连接配置。
    pub fn config(&self) -> &NebulaConfig {
        &self.cfg
    }

    /// 探测连通性（会真的发一次 ping）。
    pub fn ping(&self) -> bool {
        self.with_client(|c| c.ping()).unwrap_or(false)
    }

    /// 直接执行一条 SQL（调试用，透传给 Nebula）。
    pub fn sql(&self, sql: &str) -> Result<Vec<Vec<String>>> {
        self.with_client(|c| {
            let (_, rows) = c.query(sql)?;
            Ok(rows)
        })
    }

    /// 最近留下的错误信息。
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|e| e.clone())
    }

    /// 拿到底层客户端执行一段操作；按需自动重连。
    ///
    /// 这里刻意**在失败时重试一次**：Nebula 在空闲超时或服务重启后会断开，
    /// 而角色扮演是长时间低频交互的程序，一次透明重连比抛错给用户友好得多。
    fn with_client<T>(&self, f: impl Fn(&mut NebulaClient) -> Result<T>) -> Result<T> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| MemoryError::Other("Nebula 客户端锁被污染".into()))?;

        if guard.is_none() {
            *guard = Some(self.reconnect()?);
        }

        let first = f(guard.as_mut().expect("上面刚确保非空"));
        match first {
            Ok(v) => {
                self.connected.store(true, Ordering::Relaxed);
                self.set_last_error(None);
                Ok(v)
            }
            Err(e) => {
                self.set_last_error(Some(e.to_string()));
                if !self.cfg.auto_reconnect {
                    self.connected.store(false, Ordering::Relaxed);
                    return Err(e);
                }
                // 断线 → 重连一次再试
                *guard = None;
                let client = self.reconnect()?;
                *guard = Some(client);
                let retry = f(guard.as_mut().expect("刚写入的客户端"));
                match retry {
                    Ok(v) => {
                        self.connected.store(true, Ordering::Relaxed);
                        self.set_last_error(None);
                        Ok(v)
                    }
                    Err(e2) => {
                        self.connected.store(false, Ordering::Relaxed);
                        self.set_last_error(Some(e2.to_string()));
                        Err(e2)
                    }
                }
            }
        }
    }

    fn reconnect(&self) -> Result<NebulaClient> {
        NebulaClient::connect_as(
            &self.cfg.addr,
            &self.cfg.user,
            &self.cfg.password,
            self.cfg.timeout,
        )
        .inspect_err(|e| {
            self.connected.store(false, Ordering::Relaxed);
            self.set_last_error(Some(e.to_string()));
        })
    }

    fn set_last_error(&self, msg: Option<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = msg;
        }
    }
}

impl MemoryPort for NebulaMemory {
    fn name(&self) -> &str {
        "nebula"
    }

    fn health(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn remember(&self, note: &MemoryNote) -> StyxResult<String> {
        let mut note = note.clone();
        if note.source.is_empty() {
            note.source = format!("styx:{}", self.cfg.namespace);
        }
        let sql = schema::insert_sql(&note);
        let id = self.with_client(|c| {
            let resp = c.sql(&sql)?;
            // Nebula 的写回执形如 "OK, inserted memory main.1"，
            // 从中把 id 抠出来，省掉一次回读。
            Ok(resp
                .message
                .as_deref()
                .and_then(parse_inserted_id)
                .unwrap_or_default())
        })?;
        Ok(id)
    }

    fn recall(&self, query: &str, limit: usize) -> StyxResult<Vec<Recalled>> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let sql = schema::search_sql(query, limit);
        let hits = self.with_client(|c| {
            let (cols, rows) = c.query(&sql)?;
            Ok(schema::rows_to_recalled(&cols, &rows, "nebula"))
        })?;
        Ok(hits)
    }

    fn related(&self, id: &str, limit: usize) -> StyxResult<Vec<Recalled>> {
        let sql = schema::related_sql(id, limit);
        let hits = self.with_client(|c| {
            let (cols, rows) = c.query(&sql)?;
            Ok(schema::rows_to_recalled(&cols, &rows, "nebula"))
        })?;
        Ok(hits)
    }

    fn forget(&self, id: &str) -> StyxResult<bool> {
        let sql = schema::delete_sql(id);
        let removed = self.with_client(|c| {
            let resp = c.sql(&sql)?;
            Ok(resp.affected > 0)
        })?;
        Ok(removed)
    }

    fn status(&self) -> String {
        let health = if self.health() { "在线" } else { "离线" };
        match self.last_error() {
            Some(e) => format!(
                "nebula {}@{} [{}]（最近错误：{}）",
                self.cfg.user, self.cfg.addr, health, e
            ),
            None => format!("nebula {}@{} [{}]", self.cfg.user, self.cfg.addr, health),
        }
    }
}

/// 从 `"OK, inserted memory main.1"` 里取 `"1"`。
pub fn parse_inserted_id(message: &str) -> Option<String> {
    let tail = message.rsplit('.').next()?.trim();
    if tail.is_empty() {
        return None;
    }
    // 允许 "1" 或 "kb.1" 这类形式；取最后的连续数字
    let digits: String = tail
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use styx_core::error::StyxError;

    #[test]
    fn parses_inserted_ids_from_nebula_ack() {
        assert_eq!(
            parse_inserted_id("OK, inserted memory main.1").as_deref(),
            Some("1")
        );
        assert_eq!(
            parse_inserted_id("OK, inserted memory kb.42").as_deref(),
            Some("42")
        );
        assert_eq!(parse_inserted_id("OK").as_deref(), None);
        assert_eq!(parse_inserted_id("").as_deref(), None);
    }

    #[test]
    fn config_reads_env_defaults() {
        let cfg = NebulaConfig::default();
        assert_eq!(cfg.user, "admin");
        assert_eq!(cfg.addr, "127.0.0.1:7878");
        assert!(cfg.auto_reconnect);
        // with_env 在环境变量缺失时不应改动默认值
        let cfg = NebulaConfig::default().with_env();
        assert_eq!(cfg.user, "admin");
    }

    #[test]
    fn missing_password_is_a_config_error() {
        let cfg = NebulaConfig {
            password: String::new(),
            ..Default::default()
        };
        // 确保环境变量不会干扰
        std::env::remove_var("NEBULA_PASSWORD");
        let err = NebulaMemory::connect(cfg).unwrap_err();
        assert!(matches!(err, MemoryError::Config(_)), "got {err:?}");
    }

    #[test]
    fn unreachable_server_reports_a_connection_error() {
        let cfg = NebulaConfig {
            addr: "127.0.0.1:1".into(), // 几乎不可能有服务
            password: "pw".into(),
            timeout: Duration::from_millis(300),
            ..Default::default()
        };
        let err = NebulaMemory::connect(cfg).unwrap_err();
        assert!(matches!(err, MemoryError::Io(_)), "got {err:?}");
    }

    #[test]
    fn memory_error_maps_into_styx_error() {
        let e: StyxError = MemoryError::Sql("bad sql".into()).into();
        assert!(matches!(e, StyxError::Memory(_)));
        let e: StyxError = MemoryError::Offline("down".into()).into();
        assert!(matches!(e, StyxError::PortUnavailable { port: "memory", .. }));
        assert!(e.is_degradable());
    }
}
