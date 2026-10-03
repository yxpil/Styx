//! OpenAI 兼容的 HTTP 后端。
//!
//! 之所以选"OpenAI 兼容"作为唯一的线上协议，是因为它已经事实上成了行业通用语：
//! 官方 OpenAI、DeepSeek、通义千问（compatible-mode）、Moonshot、Groq、
//! 以及本机的 Ollama / LM Studio / vLLM / one-api 全都说这一套。
//! 只实现它，等于一次性接上了几乎全部模型服务。
//!
//! 请求体只发**必需字段**，不塞各家私有的扩展参数——多端点调度用到的
//! 端点差异（地址、密钥、模型名）都在 [`crate::Endpoint`] 里，不在协议里。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use styx_core::ports::{ChatMessage, Completion, LlmOptions};
use styx_core::text::round3;
use styx_http::{default_transport, HttpRequest, HttpTransport};

use crate::backend::ChatBackend;
use crate::endpoint::Endpoint;
use crate::error::{LlmError, Result};

/// OpenAI 兼容后端。
pub struct OpenAiBackend {
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for OpenAiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiBackend")
            .field("transport", &self.transport.name())
            .finish()
    }
}

impl Default for OpenAiBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiBackend {
    /// 用默认传输（有 `tls` feature 时支持 HTTPS）。
    pub fn new() -> Self {
        OpenAiBackend {
            transport: default_transport(),
        }
    }

    /// 注入自定义传输（测试用）。
    pub fn with_transport(transport: Arc<dyn HttpTransport>) -> Self {
        OpenAiBackend { transport }
    }

    /// 传输名（用于日志）。
    pub fn transport_name(&self) -> &str {
        self.transport.name()
    }
}

impl ChatBackend for OpenAiBackend {
    fn name(&self) -> &str {
        "openai-compatible"
    }

    fn chat(
        &self,
        endpoint: &Endpoint,
        messages: &[ChatMessage],
        opts: &LlmOptions,
    ) -> Result<Completion> {
        endpoint.validate()?;
        let url = endpoint.chat_url();
        let body = build_body(endpoint, messages, opts);

        let mut req = HttpRequest::post_json(&url, body.to_string())
            .with_timeout(Duration::from_secs(endpoint.timeout_secs.max(1)))
            .with_bearer(&endpoint.api_key);
        for (k, v) in &endpoint.headers {
            req = req.with_header(k, v);
        }

        let resp = self.transport.send(&req).map_err(|e| LlmError::Http {
            endpoint: endpoint.name.clone(),
            message: e.to_string(),
        })?;

        if !resp.is_success() {
            // 出错时把服务端给的原因读出来——这对调试多端点调度极其重要
            let hint = extract_error_message(&resp.body)
                .unwrap_or_else(|| resp.body.chars().take(300).collect());
            return Err(LlmError::Status {
                endpoint: endpoint.name.clone(),
                status: resp.status,
                body: hint,
            });
        }

        let v: Value = serde_json::from_str(&resp.body).map_err(|e| {
            LlmError::Decode(format!("{e}；正文：{}", resp.body.chars().take(200).collect::<String>()))
        })?;
        let (text, prompt_tokens, completion_tokens) =
            parse_chat_response(&v, &endpoint.name)?;
        let model = v
            .get("model")
            .and_then(|m| m.as_str())
            .map(String::from)
            .or_else(|| opts.model.clone())
            .unwrap_or_else(|| endpoint.model.clone());

        Ok(Completion {
            text,
            model,
            endpoint: endpoint.name.clone(),
            prompt_tokens,
            completion_tokens,
        })
    }
}

/// 把一条消息编成 JSON。
///
/// **纯文本消息必须编成 `"content": "字符串"`，不能是分段数组。**
/// 不少自建的 OpenAI 兼容端点和本地小模型只认字符串形式，碰上数组会直接
/// 400 或（更糟）静默当成空消息。所以这里只在真的有图时才展开成分段数组，
/// 没有图的时候发出的 JSON 和引入多模态之前逐字节相同。
///
/// 有图时，若正文是空的就**只发图片段**——OpenAI 的接口不接受
/// `{"type":"text","text":""}` 这种空文本段，会报 400。
fn message_value(m: &ChatMessage) -> Value {
    if m.images.is_empty() {
        return json!({ "role": m.role, "content": m.content });
    }

    let mut parts: Vec<Value> = Vec::with_capacity(m.images.len() + 1);
    if !m.content.is_empty() {
        parts.push(json!({ "type": "text", "text": m.content }));
    }
    for url in &m.images {
        // url 原样透传。`data:image/jpeg;base64,...` 已经是目标格式，
        // 这里再去解码再编码一遍只会多一次出错的机会（还得猜 MIME）。
        parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
    }
    json!({ "role": m.role, "content": parts })
}

/// 组装 `/chat/completions` 的请求体。
pub fn build_body(endpoint: &Endpoint, messages: &[ChatMessage], opts: &LlmOptions) -> Value {
    let model = opts
        .model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(&endpoint.model);

    let msgs: Vec<Value> = messages.iter().map(message_value).collect();

    let mut body = json!({
        "model": model,
        "messages": msgs,
        "stream": false,
    });
    let map = body.as_object_mut().expect("刚构造的对象");

    if let Some(t) = opts.temperature {
        map.insert("temperature".into(), json!(round3(t)));
    }
    if let Some(p) = opts.top_p {
        map.insert("top_p".into(), json!(round3(p)));
    }
    if let Some(m) = opts.max_tokens {
        map.insert("max_tokens".into(), json!(m));
    }
    if !opts.stop.is_empty() {
        map.insert("stop".into(), json!(opts.stop));
    }
    if opts.json_mode {
        map.insert("response_format".into(), json!({"type": "json_object"}));
    }
    body
}

/// 解析响应，返回 `(文本, prompt_tokens, completion_tokens)`。
pub fn parse_chat_response(v: &Value, endpoint: &str) -> Result<(String, u64, u64)> {
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Err(LlmError::Status {
            endpoint: endpoint.to_string(),
            status: 200,
            body: msg.to_string(),
        });
    }

    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| LlmError::Decode("响应里没有 choices".into()))?;

    // 三种常见的正文位置：message.content（字符串）、
    // message.content（分段数组，多模态/推理模型会给这种）、text（旧 completions 风格）。
    // 注意每一档都要用 `non_empty` 过滤：`content: ""` 如果被当成"取到了"，
    // 后面的兜底（text / reasoning_content）就永远轮不到。
    let text = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(content_to_text)
        .and_then(non_empty)
        .or_else(|| {
            choice
                .get("text")
                .and_then(|t| t.as_str())
                .map(String::from)
                .and_then(non_empty)
        })
        .or_else(|| {
            // 有些推理模型把正文放在 reasoning_content，只在 content 为空时兜底
            choice
                .get("message")
                .and_then(|m| m.get("reasoning_content"))
                .and_then(|t| t.as_str())
                .map(String::from)
                .and_then(non_empty)
        })
        .unwrap_or_default();

    if text.trim().is_empty() {
        return Err(LlmError::EmptyCompletion {
            endpoint: endpoint.to_string(),
        });
    }

    let usage = v.get("usage");
    let prompt_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    Ok((text, prompt_tokens, completion_tokens))
}

/// 空字符串等同于"没有这个字段"。
fn non_empty(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

fn content_to_text(c: &Value) -> Option<String> {
    match c {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                match p {
                    Value::String(s) => out.push_str(s),
                    Value::Object(o) => {
                        if let Some(t) = o.get("text").and_then(|t| t.as_str()) {
                            out.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

/// 从错误响应体里抠出人类可读的原因。
pub fn extract_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let e = v.get("error")?;
    if let Some(m) = e.get("message").and_then(|m| m.as_str()) {
        return Some(m.to_string());
    }
    if let Some(m) = e.as_str() {
        return Some(m.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use styx_http::{HttpResponse, Result as HttpResult};

    /// 记录请求、返回预设响应的假传输。
    struct FakeTransport {
        status: u16,
        body: String,
        seen: std::sync::Mutex<Vec<HttpRequest>>,
    }

    impl HttpTransport for FakeTransport {
        fn name(&self) -> &str {
            "fake"
        }
        fn send(&self, req: &HttpRequest) -> HttpResult<HttpResponse> {
            self.seen.lock().unwrap().push(req.clone());
            Ok(HttpResponse {
                status: self.status,
                headers: BTreeMap::new(),
                body: self.body.clone(),
            })
        }
    }

    fn backend_with(status: u16, body: &str) -> (OpenAiBackend, Arc<FakeTransport>) {
        let t = Arc::new(FakeTransport {
            status,
            body: body.to_string(),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        (OpenAiBackend::with_transport(t.clone()), t)
    }

    fn msgs() -> Vec<ChatMessage> {
        vec![ChatMessage::system("你是林夏"), ChatMessage::user("在吗")]
    }

    #[test]
    fn happy_path_parses_content_and_usage() {
        let body = r#"{
            "id":"x","model":"gpt-4o-mini",
            "choices":[{"index":0,"message":{"role":"assistant","content":"[说] 不卖。"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}
        }"#;
        let (b, t) = backend_with(200, body);
        let c = b
            .chat(
                &Endpoint::new("ep", "https://api.openai.com/v1", "gpt-4o-mini").with_key("sk-1"),
                &msgs(),
                &LlmOptions::default(),
            )
            .unwrap();
        assert_eq!(c.text, "[说] 不卖。");
        assert_eq!(c.model, "gpt-4o-mini");
        assert_eq!(c.endpoint, "ep");
        assert_eq!(c.prompt_tokens, 42);
        assert_eq!(c.completion_tokens, 7);

        // 请求本身要正确
        let req = &t.seen.lock().unwrap()[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api.openai.com/v1/chat/completions");
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer sk-1"));
        let sent: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(sent["model"], "gpt-4o-mini");
        assert_eq!(sent["stream"], false);
        assert_eq!(sent["messages"][0]["role"], "system");
        assert_eq!(sent["messages"][1]["content"], "在吗");
    }

    /// 回归：**没有图的消息必须还是字符串形式。**
    ///
    /// 很多自建的 OpenAI 兼容端点和本地小模型只认 `"content": "..."`,
    /// 碰上分段数组会 400。这条断言就是防止有人"顺手统一成数组"。
    #[test]
    fn a_text_only_message_stays_a_plain_string() {
        let m = ChatMessage::user("在吗");
        assert!(!m.has_images());
        let body = build_body(&Endpoint::new("ep", "https://x/v1", "m"), &[m], &LlmOptions::default());
        assert!(
            body["messages"][0]["content"].is_string(),
            "纯文本必须编成字符串，实际是 {}",
            body["messages"][0]["content"]
        );
        // 序列化之后也不该冒出 `images` 字段
        let json = serde_json::to_string(&ChatMessage::user("x")).unwrap();
        assert!(!json.contains("images"), "空 images 不该出现在 JSON 里：{json}");
    }

    #[test]
    fn an_image_turns_the_message_into_a_content_array() {
        let m = ChatMessage::user("看看这个").with_image("data:image/jpeg;base64,AAAA");
        let body = build_body(&Endpoint::new("ep", "https://x/v1", "m"), &[m], &LlmOptions::default());
        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "带图必须展开成分段数组");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "看看这个");
        assert_eq!(content[1]["type"], "image_url");
        // data URL 原样透传，不解码不重编码
        assert_eq!(content[1]["image_url"]["url"], "data:image/jpeg;base64,AAAA");
    }

    /// 图在前、话在后也允许；而**正文为空时绝不能发出空的 text 段**——
    /// OpenAI 会对 `{"type":"text","text":""}` 直接报 400。
    #[test]
    fn an_empty_caption_yields_an_image_only_message() {
        let m = ChatMessage::user("").with_image("data:image/png;base64,BBBB");
        let body = build_body(&Endpoint::new("ep", "https://x/v1", "m"), &[m], &LlmOptions::default());
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "空正文不该产生 text 段：{content:?}");
        assert_eq!(content[0]["type"], "image_url");
    }

    #[test]
    fn several_images_keep_their_order() {
        let m = ChatMessage::user("三张").with_images([
            "data:image/png;base64,1",
            "https://example.com/b.jpg",
            "data:image/png;base64,3",
        ]);
        let body = build_body(&Endpoint::new("ep", "https://x/v1", "m"), &[m], &LlmOptions::default());
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 4, "一段文字 + 三张图");
        let urls: Vec<&str> = content[1..]
            .iter()
            .map(|p| p["image_url"]["url"].as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            vec![
                "data:image/png;base64,1",
                "https://example.com/b.jpg",
                "data:image/png;base64,3"
            ]
        );
    }

    /// 序列化往返：`images` 缺席时默认成空，空时又不出现在输出里。
    #[test]
    fn images_round_trip_through_serde() {
        let old = r#"{"role":"user","content":"老的格式"}"#;
        let m: ChatMessage = serde_json::from_str(old).unwrap();
        assert!(!m.has_images(), "旧格式没有 images 字段，应当默认成空");
        assert_eq!(m.content, "老的格式");

        let with = serde_json::to_string(&ChatMessage::user("x").with_image("data:image/png;base64,A")).unwrap();
        let back: ChatMessage = serde_json::from_str(&with).unwrap();
        assert_eq!(back.images, vec!["data:image/png;base64,A".to_string()]);
    }

    #[test]
    fn request_body_carries_generation_options() {
        let (b, t) = backend_with(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let opts = LlmOptions {
            temperature: Some(0.85),
            top_p: Some(0.9),
            max_tokens: Some(900),
            stop: vec!["\n\n".into()],
            json_mode: true,
            model: None,
        };
        b.chat(&Endpoint::new("ep", "http://h/v1", "m"), &msgs(), &opts)
            .unwrap();
        let req = &t.seen.lock().unwrap()[0];
        let sent: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(sent["temperature"], 0.85);
        assert_eq!(sent["top_p"], 0.9);
        assert_eq!(sent["max_tokens"], 900);
        assert_eq!(sent["stop"][0], "\n\n");
        assert_eq!(sent["response_format"]["type"], "json_object");
    }

    #[test]
    fn model_override_wins_over_endpoint_model() {
        let (b, t) = backend_with(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let opts = LlmOptions {
            model: Some("override".into()),
            ..Default::default()
        };
        b.chat(&Endpoint::new("ep", "http://h/v1", "base"), &msgs(), &opts)
            .unwrap();
        let req = &t.seen.lock().unwrap()[0];
        let sent: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(sent["model"], "override");
    }

    #[test]
    fn http_error_surfaces_the_server_reason() {
        let (b, _t) = backend_with(
            429,
            r#"{"error":{"message":"Rate limit reached for gpt-4o-mini","type":"rate_limit"}}"#,
        );
        let err = b
            .chat(&Endpoint::new("ep", "http://h/v1", "m"), &msgs(), &LlmOptions::default())
            .unwrap_err();
        let s = err.to_string();
        assert!(s.contains("429"), "{s}");
        assert!(s.contains("Rate limit reached"), "{s}");
        assert!(s.contains("ep"), "{s}");
    }

    #[test]
    fn error_field_inside_a_200_also_fails() {
        let (b, _t) = backend_with(
            200,
            r#"{"error":{"message":"model not found"}}"#,
        );
        let err = b
            .chat(&Endpoint::new("ep", "http://h/v1", "m"), &msgs(), &LlmOptions::default())
            .unwrap_err();
        assert!(err.to_string().contains("model not found"));
    }

    #[test]
    fn empty_completion_is_an_error() {
        let (b, _t) = backend_with(200, r#"{"choices":[{"message":{"content":"   "}}]}"#);
        let err = b
            .chat(&Endpoint::new("ep", "http://h/v1", "m"), &msgs(), &LlmOptions::default())
            .unwrap_err();
        assert!(matches!(err, LlmError::EmptyCompletion { .. }), "{err:?}");
    }

    #[test]
    fn missing_choices_is_a_decode_error() {
        let (b, _t) = backend_with(200, r#"{"object":"chat.completion"}"#);
        let err = b
            .chat(&Endpoint::new("ep", "http://h/v1", "m"), &msgs(), &LlmOptions::default())
            .unwrap_err();
        assert!(err.to_string().contains("choices"), "{err}");
    }

    #[test]
    fn multipart_content_is_joined() {
        let body = r#"{"choices":[{"message":{"content":[
            {"type":"text","text":"[说] "},{"type":"text","text":"不卖。"}]}}]}"#;
        let (b, _t) = backend_with(200, body);
        let c = b
            .chat(&Endpoint::new("ep", "http://h/v1", "m"), &msgs(), &LlmOptions::default())
            .unwrap();
        assert_eq!(c.text, "[说] 不卖。");
    }

    #[test]
    fn legacy_text_field_is_accepted() {
        let (b, _t) = backend_with(200, r#"{"choices":[{"text":"旧式补全"}]}"#);
        let c = b
            .chat(&Endpoint::new("ep", "http://h/v1", "m"), &msgs(), &LlmOptions::default())
            .unwrap();
        assert_eq!(c.text, "旧式补全");
    }

    #[test]
    fn reasoning_content_is_used_only_as_fallback() {
        let body = r#"{"choices":[{"message":{"content":"","reasoning_content":"思考过程"}}]}"#;
        let (b, _t) = backend_with(200, body);
        let c = b
            .chat(&Endpoint::new("ep", "http://h/v1", "m"), &msgs(), &LlmOptions::default())
            .unwrap();
        assert_eq!(c.text, "思考过程");
    }

    #[test]
    fn invalid_endpoint_is_rejected_before_any_request() {
        let (b, t) = backend_with(200, "{}");
        let err = b
            .chat(
                &Endpoint::new("ep", "not-a-url", "m"),
                &msgs(),
                &LlmOptions::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("base_url"));
        assert!(t.seen.lock().unwrap().is_empty(), "不该发出请求");
    }

    #[test]
    fn extra_headers_are_sent() {
        let (b, t) = backend_with(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let mut ep = Endpoint::new("relay", "http://h/v1", "m");
        ep.headers = vec![("X-Title".into(), "Styx".into())];
        b.chat(&ep, &msgs(), &LlmOptions::default()).unwrap();
        let req = &t.seen.lock().unwrap()[0];
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "X-Title" && v == "Styx"));
    }

    #[test]
    fn extract_error_message_variants() {
        assert_eq!(
            extract_error_message(r#"{"error":{"message":"m1"}}"#).as_deref(),
            Some("m1")
        );
        assert_eq!(
            extract_error_message(r#"{"error":"m2"}"#).as_deref(),
            Some("m2")
        );
        assert_eq!(extract_error_message("<html>500</html>"), None);
    }
}
