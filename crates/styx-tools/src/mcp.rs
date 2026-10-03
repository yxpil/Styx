//! MCP 与 BIT Remote 桥。
//!
//! 这一层让 Styx **不是一座孤岛**：
//!
//! | 来源 | 协议 | 你的仓库 |
//! |---|---|---|
//! | MCP 服务器 | Streamable HTTP + JSON-RPC 2.0 | [PANOPTES](https://github.com/yxpil/PANOPTES)、[SECFORGE](https://github.com/yxpil/SECFORGE)、[TentacleTool](https://github.com/yxpil/TentacleTool) |
//! | BIT Remote | `POST {"params": {...}}` 信封 | [BIT](https://github.com/yxpil/bit)、[BrainTentacle](https://github.com/yxpil/BrainTentacle) |
//!
//! # 关于 SSE
//!
//! MCP 的 Streamable HTTP 允许服务端用 `text/event-stream` 回一个事件流
//! （哪怕只有一个事件）。这里做了兼容：读 `data:` 行、取最后一个合法 JSON。
//! 不兼容响应的后果会是"工具明明有，调用却解析失败"，属于很难查的坑。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use styx_core::error::{Result, StyxError};
use styx_core::ports::{ToolPort, ToolSpec};
use styx_http::{default_transport, HttpRequest, HttpResponse, HttpTransport};

/// MCP Streamable HTTP 客户端。
pub struct McpClient {
    url: String,
    token: String,
    timeout: Duration,
    transport: Arc<dyn HttpTransport>,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
    label: String,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("url", &self.url)
            .field("session", &self.session.lock().ok().and_then(|s| s.clone()))
            .finish()
    }
}

impl McpClient {
    /// 新建。
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        let label = url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("mcp")
            .to_string();
        McpClient {
            url,
            token: String::new(),
            timeout: Duration::from_secs(30),
            transport: default_transport(),
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
            label,
        }
    }

    /// Bearer token。
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = token.into();
        self
    }

    /// 超时。
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// 注入传输（测试用）。
    pub fn with_transport(mut self, t: Arc<dyn HttpTransport>) -> Self {
        self.transport = t;
        self
    }

    /// 服务器名（从 URL 末段猜的，用于工具 origin）。
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 地址。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 握手：`initialize` + `notifications/initialized`，并保存会话 id。
    ///
    /// 有些服务器不要求会话，也不接受 `initialize`；失败时**不当作致命错误**，
    /// 只是没有会话 id 而已——后续 `tools/list` 往往仍能成功。
    pub fn initialize(&self) -> Result<Value> {
        let result = self.rpc(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "styx", "version": styx_core::VERSION }
            }),
        )?;
        // 通知没有响应，失败也无所谓
        let _ = self.notify("notifications/initialized", json!({}));
        Ok(result)
    }

    /// 列出工具。
    pub fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let result = self.rpc("tools/list", json!({}))?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or_else(|| {
                StyxError::Tool(
                    self.label.clone(),
                    "tools/list 的响应里没有 tools 数组".into(),
                )
            })?;
        Ok(tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                Some(ToolSpec {
                    name,
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type":"object"})),
                    origin: format!("mcp:{}", self.label),
                })
            })
            .collect())
    }

    /// 调用工具。返回 MCP 的原始 `result`。
    pub fn call(&self, name: &str, arguments: Value) -> Result<Value> {
        self.rpc(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    /// 调用工具并把 `content` 拼成纯文本（对内核最友好的形态）。
    pub fn call_text(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self.call(name, arguments)?;
        let is_error = result
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false);
        let text = mcp_content_to_text(&result);
        if is_error {
            return Err(StyxError::Tool(name.to_string(), text));
        }
        if text.is_empty() {
            return Ok(result.to_string());
        }
        Ok(text)
    }

    /// 发一个 JSON-RPC 请求。
    pub fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let resp = self.post(body)?;
        let v = decode_body(&resp)?;
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知 JSON-RPC 错误");
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            return Err(StyxError::Tool(
                self.label.clone(),
                format!("{method} 失败（{code}）：{msg}"),
            ));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// 发一个通知（无 id、无响应）。
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.post(body)?;
        Ok(())
    }

    fn post(&self, body: Value) -> Result<HttpResponse> {
        let mut req = HttpRequest::post_json(&self.url, body.to_string())
            .with_timeout(self.timeout)
            .with_bearer(&self.token);
        // MCP Streamable HTTP 要求客户端同时接受 JSON 与 SSE
        req = req.with_header("Accept", "application/json, text/event-stream");
        req.headers
            .push(("MCP-Protocol-Version".into(), "2025-06-18".into()));
        if let Ok(s) = self.session.lock() {
            if let Some(sid) = s.as_ref() {
                req = req.with_header("Mcp-Session-Id", sid.clone());
            }
        }

        let resp = self.transport.send(&req).map_err(|e| {
            StyxError::Tool(self.label.clone(), format!("连接 MCP 服务器失败：{e}"))
        })?;

        // 记录会话 id（部分服务器在 initialize 的响应头里给）
        if let Some(sid) = resp.header("mcp-session-id") {
            if !sid.trim().is_empty() {
                if let Ok(mut s) = self.session.lock() {
                    *s = Some(sid.trim().to_string());
                }
            }
        }

        if !resp.is_success() {
            return Err(StyxError::Tool(
                self.label.clone(),
                format!(
                    "MCP 服务器返回 {}：{}",
                    resp.status,
                    resp.body.chars().take(300).collect::<String>()
                ),
            ));
        }
        Ok(resp)
    }
}

/// 从一个 HTTP 响应里解出 JSON：兼容 `application/json` 与 `text/event-stream`。
pub fn decode_body(resp: &HttpResponse) -> Result<Value> {
    let is_sse = resp
        .header("content-type")
        .map(|c| c.to_lowercase().contains("text/event-stream"))
        .unwrap_or(false);
    if is_sse {
        return parse_sse_json(&resp.body).ok_or_else(|| {
            StyxError::Tool(
                "mcp".into(),
                format!(
                    "SSE 响应里没有可解析的 JSON：{}",
                    resp.body.chars().take(300).collect::<String>()
                ),
            )
        });
    }
    serde_json::from_str(&resp.body).map_err(|e| {
        StyxError::Tool(
            "mcp".into(),
            format!(
                "响应不是合法 JSON：{e}；正文：{}",
                resp.body.chars().take(300).collect::<String>()
            ),
        )
    })
}

/// 从 SSE 文本里取最后一个可解析的 JSON 对象。
///
/// 只认 `data:` 行；同时容忍单个事件的 `data:` 后面直接跟裸 JSON。
pub fn parse_sse_json(body: &str) -> Option<Value> {
    let mut last: Option<Value> = None;
    for line in body.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(payload) {
            last = Some(v);
        }
    }
    last
}

/// 把 MCP `content` 数组拼成纯文本。
pub fn mcp_content_to_text(result: &Value) -> String {
    let Some(items) = result.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for item in items {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            Some("resource") => {
                if let Some(t) = item
                    .get("resource")
                    .and_then(|r| r.get("text"))
                    .and_then(|t| t.as_str())
                {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            _ => {
                // 图片、音频等二进制内容只标注存在，不塞进提示词
                if let Some(t) = item.get("type").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&format!("[{t} 内容已省略]"));
                }
            }
        }
    }
    out
}

/// 把一个 MCP 服务器包装成 [`ToolPort`]。
///
/// 工具列表在首次成功后缓存——MCP 的 `tools/list` 是幂等且很少变化的，
/// 每回合重列一次纯属浪费一次往返。
pub struct McpToolPort {
    client: McpClient,
    cache: Mutex<Option<Vec<ToolSpec>>>,
}

impl std::fmt::Debug for McpToolPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolPort")
            .field("client", &self.client)
            .finish()
    }
}

impl McpToolPort {
    /// 新建（会先握手，失败则退化为"不握手直接列工具"）。
    pub fn connect(client: McpClient) -> Self {
        let _ = client.initialize();
        McpToolPort {
            client,
            cache: Mutex::new(None),
        }
    }

    /// 强制刷新工具列表。
    pub fn refresh(&self) -> Result<Vec<ToolSpec>> {
        let tools = self.client.list_tools()?;
        if let Ok(mut c) = self.cache.lock() {
            *c = Some(tools.clone());
        }
        Ok(tools)
    }

    /// 底层客户端。
    pub fn client(&self) -> &McpClient {
        &self.client
    }
}

impl ToolPort for McpToolPort {
    fn name(&self) -> &str {
        self.client.label()
    }

    fn list(&self) -> Vec<ToolSpec> {
        if let Ok(c) = self.cache.lock() {
            if let Some(t) = c.as_ref() {
                return t.clone();
            }
        }
        self.refresh().unwrap_or_default()
    }

    fn invoke(&self, tool: &str, args: Value) -> Result<Value> {
        let text = self.client.call_text(tool, args)?;
        Ok(json!({ "text": text }))
    }

    fn status(&self) -> String {
        format!(
            "mcp:{}（{} 个工具）@ {}",
            self.client.label(),
            self.list().len(),
            self.client.url()
        )
    }
}

/// BIT Remote 协议的工具桥：`POST {"tool_id":…, "tool":…, "invoked_by":…, "params":{…}}`。
///
/// 信封字段对齐 BIT 的官方约定（见 MemoryPool wiki 的 Protocol 一节与其
/// `tests/cli.rs::serve_health_invoke_and_rest_api`）：
/// - `tool_id`：BIT agent 用来路由的工具实例 id，服务端当前不读但信封要求携带；
/// - `tool`：工具名，服务端在 `params.action` 缺失时回退读它；
/// - `invoked_by`：调用方标识，官方测试用 `agent:<名字>` 风格。
pub struct BitRemoteToolPort {
    url: String,
    tool: String,
    tool_id: String,
    invoked_by: String,
    token: String,
    timeout: Duration,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for BitRemoteToolPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitRemoteToolPort")
            .field("url", &self.url)
            .field("tool", &self.tool)
            .finish()
    }
}

impl BitRemoteToolPort {
    /// 新建。
    pub fn new(url: impl Into<String>, tool: impl Into<String>) -> Self {
        let tool = tool.into();
        BitRemoteToolPort {
            url: url.into(),
            tool_id: format!("styx-{tool}"),
            tool,
            invoked_by: "agent:styx".into(),
            token: String::new(),
            timeout: Duration::from_secs(30),
            transport: default_transport(),
        }
    }

    /// 覆盖 `tool_id`（BIT agent 用它路由工具实例）。
    pub fn with_tool_id(mut self, id: impl Into<String>) -> Self {
        self.tool_id = id.into();
        self
    }

    /// 覆盖 `invoked_by`（调用方标识，官方风格是 `agent:<名字>`）。
    pub fn with_invoked_by(mut self, who: impl Into<String>) -> Self {
        self.invoked_by = who.into();
        self
    }

    /// Bearer token。
    pub fn with_token(mut self, t: impl Into<String>) -> Self {
        self.token = t.into();
        self
    }

    /// 注入传输（测试用）。
    pub fn with_transport(mut self, t: Arc<dyn HttpTransport>) -> Self {
        self.transport = t;
        self
    }

    /// 把入参信封成 BIT Remote 载荷。
    pub fn envelope(&self, args: Value) -> Value {
        json!({
            "tool_id": self.tool_id,
            "tool": self.tool,
            "invoked_by": self.invoked_by,
            "params": args,
        })
    }
}

impl ToolPort for BitRemoteToolPort {
    fn name(&self) -> &str {
        &self.tool
    }

    fn list(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            self.tool.clone(),
            format!("BIT Remote 工具（{}）", self.url),
        )
        .with_origin("bit")]
    }

    fn invoke(&self, _tool: &str, args: Value) -> Result<Value> {
        let req = HttpRequest::post_json(&self.url, self.envelope(args).to_string())
            .with_timeout(self.timeout)
            .with_bearer(&self.token);
        let resp = self
            .transport
            .send(&req)
            .map_err(|e| StyxError::Tool(self.tool.clone(), e.to_string()))?;
        if !resp.is_success() {
            // BIT 约定的错误体是 {"error": "<message>"}（见 MemoryPool serve.rs）
            let detail = serde_json::from_str::<Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or_else(|| resp.body.chars().take(200).collect());
            return Err(StyxError::Tool(
                self.tool.clone(),
                format!("返回 {}：{detail}", resp.status),
            ));
        }
        serde_json::from_str(&resp.body)
            .map_err(|e| StyxError::Tool(self.tool.clone(), format!("响应不是合法 JSON：{e}")))
    }

    fn status(&self) -> String {
        format!("bit:{} @ {}", self.tool, self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 一条假响应：状态码、响应头、响应体。
    type FakeResponse = (u16, BTreeMap<String, String>, String);

    struct FakeTransport {
        responses: Mutex<Vec<FakeResponse>>,
        seen: Mutex<Vec<HttpRequest>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<(u16, &str)>) -> Arc<Self> {
            Arc::new(FakeTransport {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|(s, b)| (s, BTreeMap::new(), b.to_string()))
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            })
        }
        fn with_header(self: Arc<Self>, k: &str, v: &str) -> Arc<Self> {
            {
                let mut r = self.responses.lock().unwrap();
                if let Some(first) = r.first_mut() {
                    first.1.insert(k.to_ascii_lowercase(), v.to_string());
                }
            }
            self
        }
    }

    impl HttpTransport for FakeTransport {
        fn name(&self) -> &str {
            "fake"
        }
        fn send(&self, req: &HttpRequest) -> styx_http::Result<HttpResponse> {
            self.seen.lock().unwrap().push(req.clone());
            let mut r = self.responses.lock().unwrap();
            let (status, headers, body) = if r.len() > 1 {
                r.remove(0)
            } else {
                r.first()
                    .cloned()
                    .unwrap_or((200, BTreeMap::new(), "{}".into()))
            };
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        }
    }

    fn client(t: Arc<FakeTransport>) -> McpClient {
        McpClient::new("http://127.0.0.1:9000/mcp").with_transport(t)
    }

    #[test]
    fn lists_tools_from_a_plain_json_response() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[
            {"name":"screenshot","description":"截屏","inputSchema":{"type":"object"}},
            {"name":"click","description":"点击"}
        ]}}"#;
        let t = FakeTransport::new(vec![(200, body)]);
        let tools = client(t.clone()).list_tools().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "screenshot");
        assert_eq!(tools[0].description, "截屏");
        assert_eq!(tools[0].origin, "mcp:mcp");
        // 缺 inputSchema 时给一个宽松默认值
        assert_eq!(tools[1].schema["type"], "object");

        let req = &t.seen.lock().unwrap()[0];
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("accept") && v.contains("text/event-stream")));
        let sent: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(sent["method"], "tools/list");
        assert_eq!(sent["jsonrpc"], "2.0");
    }

    #[test]
    fn parses_sse_responses() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"a\"}]}}\n\n";
        let t = FakeTransport::new(vec![(200, sse)]);
        // 没有 content-type 头时按 JSON 解析会失败，这里显式给一个 SSE 头
        let mut r = t.responses.lock().unwrap();
        r[0].1
            .insert("content-type".into(), "text/event-stream".into());
        drop(r);
        let tools = client(t).list_tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "a");
    }

    #[test]
    fn parse_sse_json_takes_the_last_data_frame() {
        let sse = ": keepalive\ndata: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        assert_eq!(parse_sse_json(sse).unwrap()["b"], 2);
        assert!(parse_sse_json(": only comments").is_none());
        assert!(parse_sse_json("data: [DONE]").is_none());
    }

    #[test]
    fn jsonrpc_error_is_surfaced_with_code() {
        let body =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let t = FakeTransport::new(vec![(200, body)]);
        let err = client(t).list_tools().unwrap_err();
        assert!(err.to_string().contains("-32601"));
        assert!(err.to_string().contains("Method not found"));
    }

    #[test]
    fn session_id_from_initialize_is_reused() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}"#;
        let tools = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;
        let t = FakeTransport::new(vec![(200, init), (200, tools)])
            .with_header("mcp-session-id", "sess-123");
        let c = client(t.clone());
        c.initialize().unwrap();
        let _ = c.list_tools();
        let seen = t.seen.lock().unwrap();
        // 第一次（initialize）之后的所有请求都要带上会话 id
        let with_sid = seen
            .iter()
            .filter(|r| {
                r.headers
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("mcp-session-id") && v == "sess-123")
            })
            .count();
        assert!(with_sid >= 1, "会话 id 应当被复用：{seen:?}");
    }

    #[test]
    fn tool_call_returns_text() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[
            {"type":"text","text":"第一行"},{"type":"text","text":"第二行"}
        ],"isError":false}}"#;
        let t = FakeTransport::new(vec![(200, body)]);
        let c = client(t);
        assert_eq!(c.call_text("x", json!({})).unwrap(), "第一行\n第二行");
    }

    #[test]
    fn tool_call_error_flag_becomes_an_error() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"权限不足"}],"isError":true}}"#;
        let t = FakeTransport::new(vec![(200, body)]);
        let err = client(t).call_text("x", json!({})).unwrap_err();
        assert!(err.to_string().contains("权限不足"));
    }

    #[test]
    fn binary_content_is_summarised_not_inlined() {
        let v = json!({"content":[
            {"type":"image","data":"BASE64..."},
            {"type":"text","text":"说明"}
        ]});
        let t = mcp_content_to_text(&v);
        assert!(t.contains("[image 内容已省略]"));
        assert!(t.contains("说明"));
        assert!(!t.contains("BASE64"));
    }

    #[test]
    fn http_error_is_reported_with_body() {
        let t = FakeTransport::new(vec![(500, "internal error")]);
        let err = client(t).list_tools().unwrap_err();
        assert!(err.to_string().contains("500"));
        assert!(err.to_string().contains("internal error"));
    }

    #[test]
    fn mcp_tool_port_caches_and_reports_status() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"},{"name":"b"}]}}"#;
        let t = FakeTransport::new(vec![(200, body)]);
        let port = McpToolPort::connect(client(t.clone()));
        let first = port.list();
        assert_eq!(first.len(), 2);
        let calls_after_first = t.seen.lock().unwrap().len();
        // 再列一次不该产生新的请求（缓存生效）
        let _ = port.list();
        assert_eq!(t.seen.lock().unwrap().len(), calls_after_first);
        assert!(port.status().contains("@ http://127.0.0.1:9000/mcp"));
        assert_eq!(port.name(), "mcp");
    }

    #[test]
    fn mcp_tool_port_invoke_wraps_text() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"OK"}]}}"#;
        let t = FakeTransport::new(vec![(200, body)]);
        let port = McpToolPort::connect(client(t));
        let out = port.invoke("x", json!({"a":1})).unwrap();
        assert_eq!(out["text"], "OK");
    }

    #[test]
    fn bit_remote_envelope_shape() {
        // 默认信封：四件套对齐 BIT 官方约定（MemoryPool wiki / tests/cli.rs）
        let env = BitRemoteToolPort::new("http://127.0.0.1:8751/invoke", "memorypool")
            .envelope(json!({"action":"search","query":"x"}));
        assert_eq!(env["tool_id"], "styx-memorypool");
        assert_eq!(env["tool"], "memorypool");
        assert_eq!(env["invoked_by"], "agent:styx");
        assert_eq!(env["params"]["action"], "search");
        // 两个字段都可覆盖
        let custom = BitRemoteToolPort::new("http://x", "t")
            .with_tool_id("tool-1")
            .with_invoked_by("agent:test")
            .envelope(json!({}));
        assert_eq!(custom["tool_id"], "tool-1");
        assert_eq!(custom["invoked_by"], "agent:test");
        let port = BitRemoteToolPort::new("http://127.0.0.1:8751/invoke", "memorypool");
        assert_eq!(port.list().len(), 1);
        assert!(port.status().contains("bit:memorypool"));
    }

    #[test]
    fn bit_remote_invoke_round_trip() {
        let t = FakeTransport::new(vec![(200, r#"{"count":1,"results":[]}"#)]);
        let port =
            BitRemoteToolPort::new("http://h/invoke", "memorypool").with_transport(t.clone());
        let out = port
            .invoke("memorypool", json!({"action":"search","query":"x"}))
            .unwrap();
        assert_eq!(out["count"], 1);
        let req = &t.seen.lock().unwrap()[0];
        let sent: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(sent["params"]["action"], "search");
    }

    #[test]
    fn bit_remote_reports_http_errors() {
        let t = FakeTransport::new(vec![(400, "bad action")]);
        let port = BitRemoteToolPort::new("http://h/invoke", "t").with_transport(t);
        let err = port.invoke("t", json!({})).unwrap_err();
        assert!(err.to_string().contains("400"));
        assert!(err.to_string().contains("bad action"));
    }
}
