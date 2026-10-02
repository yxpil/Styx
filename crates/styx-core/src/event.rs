//! 事件日志：回合里的每一次"发生了什么"。
//!
//! 事件是内核唯一的**事实来源**（source of truth）。长期记忆不是凭空产生的，
//! 而是从事件流里筛出重要的那些写进 Nebula / MemoryPool。

use serde::{Deserialize, Serialize};

/// 事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// 用户输入。
    UserInput,
    /// 角色说出的台词。
    Speech,
    /// 角色做出的动作 / 场面描写。
    Action,
    /// 角色内心独白。
    Thought,
    /// 场景变更。
    SceneChange,
    /// 系统提示（提示词报告、降级告警等）。
    System,
    /// 工具调用。
    ToolCall,
    /// 工具返回。
    ToolResult,
    /// 写入长期记忆。
    MemoryWrite,
}

impl EventKind {
    /// 该类型事件是否属于"角色的表演"，需要进对话历史。
    pub fn is_performance(&self) -> bool {
        matches!(
            self,
            EventKind::UserInput | EventKind::Speech | EventKind::Action | EventKind::Thought
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::UserInput => "user_input",
            EventKind::Speech => "speech",
            EventKind::Action => "action",
            EventKind::Thought => "thought",
            EventKind::SceneChange => "scene_change",
            EventKind::System => "system",
            EventKind::ToolCall => "tool_call",
            EventKind::ToolResult => "tool_result",
            EventKind::MemoryWrite => "memory_write",
        }
    }
}

/// 一条事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// 全局递增序号（在一个 Kernel 生命周期内唯一）。
    pub seq: u64,
    /// Unix 毫秒时间戳。
    pub at: i64,
    /// 事件类型。
    pub kind: EventKind,
    /// 谁产生的：角色名 / 用户 / "system" / 工具名。
    pub actor: String,
    /// 事件内容。
    pub text: String,
    /// 重要度 0.0 ~ 1.0。低于阈值的不会进长期记忆。
    #[serde(default)]
    pub importance: f32,
    /// 标签，会透传给 Nebula 的 tags。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 附加字段。
    #[serde(default)]
    pub meta: std::collections::BTreeMap<String, String>,
}

impl Event {
    pub fn new(kind: EventKind, actor: impl Into<String>, text: impl Into<String>) -> Self {
        Event {
            seq: 0,
            at: now_millis(),
            kind,
            actor: actor.into(),
            text: text.into(),
            importance: default_importance(kind),
            tags: Vec::new(),
            meta: Default::default(),
        }
    }

    pub fn with_importance(mut self, importance: f32) -> Self {
        self.importance = importance.clamp(0.0, 1.0);
        self
    }

    pub fn with_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.meta.insert(key.into(), value.into());
        self
    }

    /// 渲染成一行，用于进提示词历史。
    pub fn render_line(&self) -> String {
        match self.kind {
            EventKind::UserInput => format!("{}：{}", self.actor, self.text),
            EventKind::Speech => format!("{}：「{}」", self.actor, self.text),
            EventKind::Action => format!("（{}）", self.text),
            EventKind::Thought => format!("（{}心想：{}）", self.actor, self.text),
            EventKind::SceneChange => format!("【场景】{}", self.text),
            EventKind::System => format!("【系统】{}", self.text),
            EventKind::ToolCall => format!("【调用 {}】{}", self.actor, self.text),
            EventKind::ToolResult => format!("【{} 返回】{}", self.actor, self.text),
            EventKind::MemoryWrite => format!("【记忆】{}", self.text),
        }
    }

    /// 渲染成一行可写入长期记忆的自然语言。
    pub fn render_memory(&self) -> String {
        match self.kind {
            EventKind::UserInput => format!("{}对{}说：{}", self.actor, "我", self.text),
            EventKind::Speech => format!("我说：{}", self.text),
            _ => self.render_line(),
        }
    }
}

fn default_importance(kind: EventKind) -> f32 {
    match kind {
        EventKind::UserInput => 0.4,
        EventKind::Speech => 0.4,
        EventKind::Action => 0.35,
        EventKind::Thought => 0.5,
        EventKind::SceneChange => 0.6,
        EventKind::System => 0.1,
        EventKind::ToolCall => 0.2,
        EventKind::ToolResult => 0.2,
        EventKind::MemoryWrite => 0.7,
    }
}

/// 当前 Unix 毫秒时间戳。
pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shapes() {
        let e = Event::new(EventKind::Speech, "林夏", "不卖。");
        assert_eq!(e.render_line(), "林夏：「不卖。」");
        let u = Event::new(EventKind::UserInput, "陈默", "我想买那本书。");
        assert_eq!(u.render_line(), "陈默：我想买那本书。");
        assert_eq!(u.render_memory(), "陈默对我说：我想买那本书。");
        let a = Event::new(EventKind::Action, "林夏", "把书合上");
        assert_eq!(a.render_line(), "（把书合上）");
    }

    #[test]
    fn default_importance_in_range() {
        for k in [
            EventKind::UserInput,
            EventKind::Speech,
            EventKind::Action,
            EventKind::Thought,
            EventKind::SceneChange,
            EventKind::System,
            EventKind::ToolCall,
            EventKind::ToolResult,
            EventKind::MemoryWrite,
        ] {
            let e = Event::new(k, "x", "y");
            assert!((0.0..=1.0).contains(&e.importance), "{k:?}");
        }
    }

    #[test]
    fn performance_flag() {
        assert!(EventKind::Speech.is_performance());
        assert!(EventKind::UserInput.is_performance());
        assert!(!EventKind::ToolCall.is_performance());
        assert!(!EventKind::System.is_performance());
    }
}
