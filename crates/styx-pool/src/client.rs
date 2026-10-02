//! [`PoolPort`] 的 MemoryPool 实现。
//!
//! 对接 [MemoryPool](https://github.com/yxpil/MemoryPool) 的 HTTP 服务
//! （默认 `127.0.0.1:8751`），也可以直接用它的 **BIT Remote** 入口 `/invoke`——
//! 后者走的是 `{"params": {"action": "search", ...}}` 信封，
//! 与 [BIT](https://github.com/yxpil/bit) 的工具调用协议同构。
//!
//! # 与长期记忆的分工
//!
//! | 端口 | 存什么 | 典型内容 |
//! |---|---|---|
//! | [`styx_core::ports::MemoryPort`] | 这个角色的私人经历 | "母亲留下了一张旧照片" |
//! | [`PoolPort`] | 多个 agent 之间要共享的事实 | "陈默今晚会在城南" |
//!
//! 两者都可能落在同一台机器上，但语义不同：私人记忆会随角色成长而变化，
//! 共享记忆则是"世界状态"——其他 agent 也应该看得到。

use std::time::Duration;

use styx_core::ports::{MemoryNote, PoolPort, PoolStats, Recalled};
use styx_http::{default_transport, HttpRequest, HttpTransport};

use crate::error::{PoolError, Result};
use styx_core::error::Result as StyxResult;

/// MemoryPool 连接配置。
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// 服务地址，例如 `http://127.0.0.1:8751`。
    pub base_url: String,
    /// Bearer token（`serve --token` 启用时必填）。
    pub token: String,
    /// 超时。
    pub timeout: Duration,
    /// 是否走 BIT Remote 的 `/invoke` 入口而不是 REST。
    pub use_remote: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig {
            base_url: "http://127.0.0.1:8751".into(),
            token: String::new(),
            timeout: Duration::from_secs(10),
            use_remote: false,
        }
    }
}

impl PoolConfig {
    /// 从环境变量补齐：`MEMORYPOOL_URL` / `MEMORYPOOL_TOKEN`。
    pub fn with_env(mut self) -> Self {
        if let Ok(u) = std::env::var("MEMORYPOOL_URL") {
            if !u.is_empty() && self.base_url == "http://127.0.0.1:8751" {
                self.base_url = u;
            }
        }
        if self.token.is_empty() {
            if let Ok(t) = std::env::var("MEMORYPOOL_TOKEN") {
                self.token = t;
            }
        }
        self
    }
}

/// 基于 MemoryPool 的共享记忆池。
pub struct MemoryPool {
    cfg: PoolConfig,
    transport: std::sync::Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for MemoryPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryPool")
            .field("base_url", &self.cfg.base_url)
            .field("remote", &self.cfg.use_remote)
            .finish_non_exhaustive()
    }
}

impl MemoryPool {
    /// 用默认传输新建。
    pub fn new(cfg: PoolConfig) -> Self {
        MemoryPool {
            cfg: cfg.with_env(),
            transport: default_transport(),
        }
    }

    /// 注入自定义传输（测试用）。
    pub fn with_transport(mut self, t: std::sync::Arc<dyn HttpTransport>) -> Self {
        self.transport = t;
        self
    }

    /// 配置。
    pub fn config(&self) -> &PoolConfig {
        &self.cfg
    }

    /// 探活：`GET /health`（MemoryPool 的这个端点永不做 token 保护）。
    pub fn ping(&self) -> bool {
        let url = format!("{}/health", self.cfg.base_url.trim_end_matches('/'));
        let req = HttpRequest::get(url).with_timeout(self.cfg.timeout);
        self.transport
            .send(&req)
            .map(|r| r.is_success())
            .unwrap_or(false)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }

    fn get(&self, path: &str) -> Result<serde_json::Value> {
        let req = HttpRequest::get(self.url(path))
            .with_bearer(&self.cfg.token)
            .with_timeout(self.cfg.timeout);
        let resp = self
            .transport
            .send(&req)
            .map_err(|e| PoolError::Http(e.to_string()))?;
        if !resp.is_success() {
            return Err(PoolError::Status {
                status: resp.status,
                body: resp.body.chars().take(300).collect(),
            });
        }
        serde_json::from_str(&resp.body)
            .map_err(|e| PoolError::Decode(format!("{e}；正文：{}", truncate(&resp.body))))
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let req = HttpRequest::post_json(self.url(path), body.to_string())
            .with_bearer(&self.cfg.token)
            .with_timeout(self.cfg.timeout);
        let resp = self
            .transport
            .send(&req)
            .map_err(|e| PoolError::Http(e.to_string()))?;
        if !resp.is_success() {
            return Err(PoolError::Status {
                status: resp.status,
                body: resp.body.chars().take(300).collect(),
            });
        }
        if resp.body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&resp.body)
            .map_err(|e| PoolError::Decode(format!("{e}；正文：{}", truncate(&resp.body))))
    }

    /// 通过 BIT Remote 的 `/invoke` 信封发一条动作。
    fn invoke(&self, action: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "tool": "memorypool",
            "invoked_by": "styx",
            "params": merge_action(action, params),
        });
        self.post("/invoke", body)
    }
}

fn merge_action(action: &str, params: serde_json::Value) -> serde_json::Value {
    let mut obj = match params {
        serde_json::Value::Object(o) => o,
        _ => serde_json::Map::new(),
    };
    obj.insert("action".into(), serde_json::Value::String(action.into()));
    serde_json::Value::Object(obj)
}

impl PoolPort for MemoryPool {
    fn name(&self) -> &str {
        "memorypool"
    }

    fn health(&self) -> bool {
        self.ping()
    }

    fn remember(&self, note: &MemoryNote) -> StyxResult<String> {
        let payload = serde_json::json!({
            "text": note.text,
            "tags": note.tags,
            "importance": note.importance,
            "source": if note.source.is_empty() { "styx" } else { note.source.as_str() },
        });
        let v = if self.cfg.use_remote {
            self.invoke("add", payload)?
        } else {
            self.post("/mem", payload)?
        };
        Ok(v.get("id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string())
    }

    fn recall(&self, query: &str, limit: usize) -> StyxResult<Vec<Recalled>> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let v = if self.cfg.use_remote {
            self.invoke(
                "search",
                serde_json::json!({ "query": query, "limit": limit }),
            )?
        } else {
            let path = format!(
                "/mem/search?q={}&limit={}",
                percent_encode(query),
                limit.max(1)
            );
            self.get(&path)?
        };
        Ok(json_to_recalled_list(&v, "pool"))
    }

    fn stats(&self) -> StyxResult<PoolStats> {
        let v = if self.cfg.use_remote {
            self.invoke("stats", serde_json::json!({}))?
        } else {
            self.get("/health")?
        };
        let count = v
            .get("count")
            .and_then(|c| c.as_u64())
            .or_else(|| {
                v.get("stats")
                    .and_then(|s| s.get("count"))
                    .and_then(|c| c.as_u64())
            })
            .unwrap_or(0) as usize;
        Ok(PoolStats {
            count,
            detail: format!("{} · {count} 条", self.cfg.base_url),
        })
    }

    fn status(&self) -> String {
        match self.stats() {
            Ok(s) => format!("memorypool {}（{}）", self.cfg.base_url, s.detail),
            Err(e) => format!("memorypool {} [不可达：{e}]", self.cfg.base_url),
        }
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

/// 把 MemoryPool 返回的 JSON（数组或 `{"memories": [...]}`）转成 [`Recalled`]。
pub fn json_to_recalled_list(v: &serde_json::Value, origin: &str) -> Vec<Recalled> {
    let arr = match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(o) => {
            for key in ["memories", "results", "data", "items"] {
                if let Some(a) = o.get(key).and_then(|x| x.as_array()) {
                    return a.iter().filter_map(|x| json_to_recalled(x, origin)).collect();
                }
            }
            // 单条对象
            return json_to_recalled(v, origin).into_iter().collect();
        }
        _ => return Vec::new(),
    };
    arr.iter().filter_map(|x| json_to_recalled(x, origin)).collect()
}

fn json_to_recalled(v: &serde_json::Value, origin: &str) -> Option<Recalled> {
    let o = v.as_object()?;
    let text = o
        .get("text")
        .or_else(|| o.get("content"))
        .and_then(|x| x.as_str())?
        .to_string();
    if text.trim().is_empty() {
        return None;
    }
    Some(Recalled {
        id: o
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        text,
        score: o.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
        importance: o
            .get("importance")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.5) as f32,
        tags: o
            .get("tags")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        created_at: o
            .get("created_at")
            .and_then(|x| x.as_i64())
            .or_else(|| o.get("created_at").and_then(|x| x.as_str()).and_then(parse_rfc3339_ms)),
        origin: origin.to_string(),
    })
}

/// 极简 RFC 3339 → 毫秒（MemoryPool 用 `2026-01-01T00:00:00Z` 这种格式）。
fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, rest) = s.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    let mut t = rest.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next().unwrap_or("0").parse().ok()?;
    let se: i64 = t
        .next()
        .unwrap_or("0")
        .split('.')
        .next()
        .unwrap_or("0")
        .parse()
        .ok()?;
    Some(days_from_civil(y, mo, da) * 86_400_000 + (h * 3600 + mi * 60 + se) * 1000)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 查询串百分号编码（只保留 RFC 3986 的 unreserved 字符）。
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_matches_rfc3986() {
        assert_eq!(percent_encode("abc-_.~"), "abc-_.~");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("旧照片"), "%E6%97%A7%E7%85%A7%E7%89%87");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn parses_memory_list() {
        let v = serde_json::json!([
            {"id":"m1","text":"陈默今晚在城南","tags":["陈默"],"importance":0.8,"score":1.4,
             "created_at":"2026-01-01T00:00:00Z"},
            {"id":"m2","text":"","importance":0.1}
        ]);
        let got = json_to_recalled_list(&v, "pool");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "m1");
        assert_eq!(got[0].tags, vec!["陈默"]);
        assert!((got[0].score - 1.4).abs() < 1e-6);
        assert_eq!(got[0].created_at, Some(1_767_225_600_000));
        assert_eq!(got[0].origin, "pool");
    }

    #[test]
    fn parses_wrapped_memory_list() {
        let v = serde_json::json!({"memories": [{"id":"x","text":"某事"}]});
        let got = json_to_recalled_list(&v, "pool");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "某事");
        // 缺 score 时默认 0，不影响可用性
        assert_eq!(got[0].score, 0.0);
    }

    #[test]
    fn parses_single_object() {
        let v = serde_json::json!({"id":"x","text":"单条","importance":0.9});
        let got = json_to_recalled_list(&v, "pool");
        assert_eq!(got.len(), 1);
        assert!((got[0].importance - 0.9).abs() < 1e-6);
    }

    #[test]
    fn merges_action_into_params() {
        let v = merge_action("search", serde_json::json!({"query":"x","limit":3}));
        assert_eq!(v["action"], "search");
        assert_eq!(v["query"], "x");
        assert_eq!(v["limit"], 3);
    }

    #[test]
    fn rfc3339_parsing() {
        assert_eq!(parse_rfc3339_ms("2026-01-01T00:00:00Z"), Some(1_767_225_600_000));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("garbage"), None);
    }

    #[test]
    fn config_reads_env() {
        let c = PoolConfig::default();
        assert!(!c.use_remote);
        assert_eq!(c.base_url, "http://127.0.0.1:8751");
    }
}
