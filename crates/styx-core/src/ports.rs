//! 端口（Port）：内核与外部世界之间的**全部**接触面。
//!
//! 每个端口对应一个外部能力，也对应一个可联动的仓库：
//!
//! | 端口 | 职责 | 默认对接 |
//! |---|---|---|
//! | [`MemoryPort`] | 长期记忆：写入、召回、联想、遗忘 | [Nebula](https://github.com/yxpil/Nebula) |
//! | [`AssocPort`] | 词级联想与可解释推理（带证据与置信度） | [MightBe](https://github.com/yxpil/MightBe) |
//! | [`PoolPort`] | 跨进程/跨会话共享记忆池 | [MemoryPool](https://github.com/yxpil/MemoryPool) |
//! | [`LlmPort`] | 语言模型生成 | OpenAI 兼容端点（多端点加权调度） |
//! | [`ToolPort`] | 工具列举与调用 | 内置工具 + MCP / BIT Remote |
//!
//! **所有端口都是对象安全的**（`dyn` 可用），且都提供 `health()`，
//! 上层据此决定是否降级到内存兜底实现。

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// 一条被召回的记忆。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recalled {
    /// 后端分配的 id。
    pub id: String,
    /// 记忆原文。
    pub text: String,
    /// 相关度分数（不同后端量纲不同，只用于排序与展示）。
    pub score: f32,
    /// 重要度 0..1。
    pub importance: f32,
    /// 标签。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 写入时间（Unix 毫秒）。
    #[serde(default)]
    pub created_at: Option<i64>,
    /// 来自哪个后端，例如 `"nebula"` / `"pool"` / `"memory"`。
    pub origin: String,
}

impl Recalled {
    pub fn new(id: impl Into<String>, text: impl Into<String>, score: f32, origin: &str) -> Self {
        Recalled {
            id: id.into(),
            text: text.into(),
            score,
            importance: 0.5,
            tags: Vec::new(),
            created_at: None,
            origin: origin.to_string(),
        }
    }
}

/// 要写入记忆后端的一条笔记。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryNote {
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 重要度 0..1，影响后续检索排序。
    #[serde(default = "half")]
    pub importance: f32,
    /// 来源标记，例如 `"styx:林夏"`。
    #[serde(default)]
    pub source: String,
}

fn half() -> f32 {
    0.5
}

impl MemoryNote {
    pub fn new(text: impl Into<String>) -> Self {
        MemoryNote {
            text: text.into(),
            tags: Vec::new(),
            importance: 0.5,
            source: String::new(),
        }
    }

    pub fn with_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_importance(mut self, v: f32) -> Self {
        self.importance = v.clamp(0.0, 1.0);
        self
    }

    pub fn with_source(mut self, s: impl Into<String>) -> Self {
        self.source = s.into();
        self
    }
}

/// 长期记忆端口。
///
/// 默认实现对接 Nebula：`remember` → `INSERT INTO memories`，
/// `recall` → `SEARCH`（BM25）或带 `WHERE` 的 `SELECT`，
/// `related` → `RELATED TO id`（共现图跳数扩展）。
pub trait MemoryPort: Send + Sync {
    fn name(&self) -> &str {
        "memory"
    }

    /// 后端是否可用。false 时上层可降级。
    fn health(&self) -> bool {
        true
    }

    /// 写入一条记忆，返回后端 id。
    fn remember(&self, note: &MemoryNote) -> Result<String>;

    /// 按查询串召回。返回按相关度降序排列的结果。
    fn recall(&self, query: &str, limit: usize) -> Result<Vec<Recalled>>;

    /// 与给定记忆 id 联想的其他条目（共现图扩展）。
    fn related(&self, id: &str, limit: usize) -> Result<Vec<Recalled>>;

    /// 遗忘一条记忆。返回是否真的删掉了。
    fn forget(&self, id: &str) -> Result<bool>;

    /// 后端的一行状态描述，用于 `probe` / `health` 展示。
    fn status(&self) -> String {
        format!("{} (health={})", self.name(), self.health())
    }
}

/// 一条联想。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Association {
    /// 联想出的词。
    pub word: String,
    /// 关联强度（越大越强）。
    pub score: f32,
    /// 可解释证据：支撑这条联想的原句/文档片段。
    #[serde(default)]
    pub evidence: Vec<String>,
    /// 置信度 0..1（MightBe 的 confidence）。
    #[serde(default = "one")]
    pub confidence: f32,
}

fn one() -> f32 {
    1.0
}

impl Association {
    pub fn new(word: impl Into<String>, score: f32) -> Self {
        Association {
            word: word.into(),
            score,
            evidence: Vec::new(),
            confidence: 1.0,
        }
    }

    pub fn with_confidence(mut self, c: f32) -> Self {
        self.confidence = c.clamp(0.0, 1.0);
        self
    }

    pub fn with_evidence<I, S>(mut self, ev: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.evidence = ev.into_iter().map(Into::into).collect();
        self
    }
}

/// 词级联想端口。
///
/// 默认实现对接 MightBe：`ASSOCIATE(net, 'word')` 拿到关联词与分数，
/// 再沿推理图取证据与置信度。`confident()` 对应 MightBe 的 **abstain（弃判）**
/// 语义——当最高置信度低于阈值时，角色宁可"想不起来"，也不该胡编。
pub trait AssocPort: Send + Sync {
    fn name(&self) -> &str {
        "assoc"
    }

    fn health(&self) -> bool {
        true
    }

    /// 以一个词/短语为种子发散。
    fn associate(&self, seed: &str, limit: usize) -> Result<Vec<Association>>;

    /// 是否有足够把握给出联想（MightBe 弃判语义的薄封装）。
    fn confident(&self, seed: &str) -> Result<bool> {
        Ok(!self.associate(seed, 1)?.is_empty())
    }

    fn status(&self) -> String {
        format!("{} (health={})", self.name(), self.health())
    }
}

/// 共享记忆池的统计信息。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PoolStats {
    /// 条目数。
    pub count: usize,
    /// 后端给的自由描述。
    #[serde(default)]
    pub detail: String,
}

/// 跨进程共享记忆池端口。
///
/// 默认实现对接 MemoryPool（HTTP / BIT Remote 协议）。
/// 与 [`MemoryPort`] 的分工：`MemoryPort` 存**这个角色经历的细节**，
/// `PoolPort` 存**多个 agent 之间需要共享的事实**（谁在做什么、约定、结论）。
pub trait PoolPort: Send + Sync {
    fn name(&self) -> &str {
        "pool"
    }

    fn health(&self) -> bool {
        true
    }

    fn remember(&self, note: &MemoryNote) -> Result<String>;

    fn recall(&self, query: &str, limit: usize) -> Result<Vec<Recalled>>;

    fn stats(&self) -> Result<PoolStats>;

    fn status(&self) -> String {
        format!("{} (health={})", self.name(), self.health())
    }
}

/// 对话消息（OpenAI 风格）。
///
/// ## 为什么图片是**并列的一个字段**，而不是把 `content` 换成枚举
///
/// OpenAI 的多模态格式把 `content` 变成一个分段数组：
///
/// ```json
/// {"role":"user","content":[
///   {"type":"text","text":"看看这个"},
///   {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,..."}}
/// ]}
/// ```
///
/// 但把 `content` 改成 `enum { Text(String), Parts(Vec<Part>) }` 会波及每一个
/// 用到它的地方——记忆库、联想服务、混合池上游收发的都是纯字符串，它们根本
/// 不关心图。所以这里改成**加一个可选的 `images`**：
///
/// - `content` 保持 `String`，所有既有调用点一个字都不用改；
/// - `images` 是 `data:` URL 或 http(s) URL 的列表，空的时候序列化器会**整个
///   省掉它**，于是纯文本请求发出的 JSON 和从前逐字节相同；
/// - 只有真的要带图时，序列化层才把这一条消息展开成分段数组。
///
/// 这个形状也让"降级"变得自然：模型端点不支持图片时，把 `images` 丢掉、
/// 只发文本，回合照常进行——而那段文本里已经有本地视觉分析写好的描述。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system` / `user` / `assistant`。
    pub role: String,
    pub content: String,
    /// 随消息一起发的图（`data:` URL 或 http(s) URL）。空 = 纯文本消息。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        ChatMessage {
            role: "system".into(),
            content: content.into(),
            images: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage {
            role: "user".into(),
            content: content.into(),
            images: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        ChatMessage {
            role: "assistant".into(),
            content: content.into(),
            images: Vec::new(),
        }
    }

    /// 给这条消息挂一张图（`data:` URL 或 http(s) URL）。
    pub fn with_image(mut self, url: impl Into<String>) -> Self {
        self.images.push(url.into());
        self
    }

    /// 给这条消息挂多张图。
    pub fn with_images<I, S>(mut self, urls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.images.extend(urls.into_iter().map(Into::into));
        self
    }

    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }
}

/// 生成参数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmOptions {
    /// 覆盖默认模型名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 停用序列。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// 期望的 JSON 输出（部分端点支持 `response_format`）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub json_mode: bool,
}

/// 一次生成的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub text: String,
    /// 实际使用的模型。
    pub model: String,
    /// 实际命中的端点（多端点调度下很有用）。
    pub endpoint: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Completion {
    pub fn new(
        text: impl Into<String>,
        model: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Completion {
            text: text.into(),
            model: model.into(),
            endpoint: endpoint.into(),
            prompt_tokens: 0,
            completion_tokens: 0,
        }
    }
}

/// 语言模型端口。
pub trait LlmPort: Send + Sync {
    fn name(&self) -> &str {
        "llm"
    }

    fn health(&self) -> bool {
        true
    }

    /// 生成一次补全。
    fn complete(&self, messages: &[ChatMessage], opts: &LlmOptions) -> Result<Completion>;

    /// 可用端点数量（单端点实现返回 1）。
    fn endpoint_count(&self) -> usize {
        1
    }

    fn status(&self) -> String {
        format!("{} (health={})", self.name(), self.health())
    }
}

/// 一个可被模型调用的工具。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema 描述的入参。
    #[serde(default)]
    pub schema: serde_json::Value,
    /// 工具来源，例如 `"builtin"` / `"mcp:panoptes"` / `"bit"`。
    #[serde(default)]
    pub origin: String,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        ToolSpec {
            name: name.into(),
            description: description.into(),
            schema: serde_json::json!({"type": "object", "properties": {}}),
            origin: "builtin".into(),
        }
    }

    pub fn with_schema(mut self, schema: serde_json::Value) -> Self {
        self.schema = schema;
        self
    }

    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = origin.into();
        self
    }
}

/// 工具端口。
pub trait ToolPort: Send + Sync {
    fn name(&self) -> &str {
        "tools"
    }

    fn list(&self) -> Vec<ToolSpec>;

    fn invoke(&self, tool: &str, args: serde_json::Value) -> Result<serde_json::Value>;

    fn status(&self) -> String {
        format!("{} ({} 个工具)", self.name(), self.list().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_note_builder_clamps_importance() {
        let n = MemoryNote::new("x")
            .with_tags(["a", "b"])
            .with_importance(9.0)
            .with_source("styx:林夏");
        assert_eq!(n.tags, vec!["a", "b"]);
        assert_eq!(n.importance, 1.0);
        assert_eq!(n.source, "styx:林夏");
    }

    #[test]
    fn chat_message_roles() {
        assert_eq!(ChatMessage::system("s").role, "system");
        assert_eq!(ChatMessage::user("u").role, "user");
        assert_eq!(ChatMessage::assistant("a").role, "assistant");
    }

    #[test]
    fn tool_spec_defaults() {
        let t = ToolSpec::new("dice", "掷骰");
        assert_eq!(t.origin, "builtin");
        assert_eq!(t.schema["type"], "object");
    }
}
