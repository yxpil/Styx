//! 事件日志：回合里的每一次"发生了什么"。
//!
//! 事件是内核唯一的**事实来源**（source of truth）。长期记忆不是凭空产生的，
//! 而是从事件流里筛出重要的那些写进 Nebula / MemoryPool。

use serde::{Deserialize, Serialize};

use crate::text::strip_quote_wrappers;

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
    /// 发出 / 收到一张表情包。
    ///
    /// 它和台词、动作一样属于"表演"，要进对话历史——否则角色下一轮
    /// 就会忘记自己刚发过一张大哭的贴图，那种失忆比忘记台词更明显。
    Sticker,
    /// 角色发来一张图片 / 照片。
    ///
    /// 与 [`EventKind::Sticker`] 一样属于表演：角色下一轮应当记得
    /// "我刚给他看过那张老照片"，否则会重复发同一张图。
    Image,
    /// 内核按角色的申请取回了一份资料（长期记忆 / 联想 / 世界书）。
    ///
    /// 这是**回合内的中间产物**，不进对话历史：资料的价值已经体现在
    /// 它促成的台词里，再进历史就是重复注入。但它要出现在事件流里，
    /// 因为"这个角色刚才想起了什么"是使用者在界面上最想看的东西之一。
    Recall,
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
            EventKind::UserInput
                | EventKind::Speech
                | EventKind::Action
                | EventKind::Thought
                | EventKind::Sticker
                | EventKind::Image
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::UserInput => "user_input",
            EventKind::Speech => "speech",
            EventKind::Action => "action",
            EventKind::Thought => "thought",
            EventKind::Sticker => "sticker",
            EventKind::Image => "image",
            EventKind::Recall => "recall",
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

    /// 构造一条表情包事件。
    ///
    /// `text` 存人类可读的标签（进提示词），编号存进 `meta.sticker_id`
    /// （前端据此取图）。两者分开是必要的：模型读的是"大哭"，
    /// 浏览器要的是 `wailing_22`。
    pub fn sticker(
        actor: impl Into<String>,
        id: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Event::new(EventKind::Sticker, actor, label).with_meta("sticker_id", id)
    }

    /// 表情包编号（非表情事件返回 `None`）。
    pub fn sticker_id(&self) -> Option<&str> {
        self.meta.get("sticker_id").map(|s| s.as_str())
    }

    /// 构造一条"角色发来一张图片"的事件。
    ///
    /// 与表情包同构：`text` 存描述（进提示词），编号与文件名进 `meta`
    /// （前端据此取图）。多存一个 `image_file` 是因为前端的 `<img>` 需要
    /// 真实文件名（含扩展名），而编号是去扩展名的。
    pub fn image(
        actor: impl Into<String>,
        id: impl Into<String>,
        file: impl Into<String>,
        caption: impl Into<String>,
    ) -> Self {
        let caption = caption.into();
        let text = if caption.trim().is_empty() {
            "一张图片".to_string()
        } else {
            caption
        };
        Event::new(EventKind::Image, actor, text)
            .with_meta("image_id", id)
            .with_meta("image_file", file)
    }

    /// 图片编号（非图片事件返回 `None`）。
    pub fn image_id(&self) -> Option<&str> {
        self.meta.get("image_id").map(|s| s.as_str())
    }

    /// 图片文件名（含扩展名）。
    pub fn image_file(&self) -> Option<&str> {
        self.meta.get("image_file").map(|s| s.as_str())
    }

    /// 构造一条"内核取回资料"的事件。
    ///
    /// `directive` 是协议里的指令种类（`recall` / `associate` / `lore`），
    /// 前端靠它加不同的标记。
    pub fn recall(
        kind: impl Into<String>,
        query: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Event::new(EventKind::Recall, "kernel", summary)
            .with_meta("directive", kind)
            .with_meta("query", query)
    }

    /// 渲染成一行，用于进提示词历史。
    pub fn render_line(&self) -> String {
        match self.kind {
            EventKind::UserInput => format!("{}：{}", self.actor, self.text),
            EventKind::Speech => {
                // 模型经常自己把台词写成 `林夏：「雨还没停。」`——它在模仿剧本排版。
                // 这时再套一层就得到 `林夏：「林夏：「雨还没停。」」`。实机跑 phi4
                // 时每两三回合就会撞上一次，读起来像解析器坏了。
                //
                // 只有开场名是**角色自己**时才跳过包装。以别人的名字开场是
                // "角色在转述别人说过的话"，那层引号带着真实信息，必须留着。
                let text = self.text.trim();
                let named = !self.actor.is_empty()
                    && (text.starts_with(&format!("{}：", self.actor))
                        || text.starts_with(&format!("{}:", self.actor)));
                if named {
                    text.to_string()
                } else {
                    // 模型**只带引号、不带说话人名**时（`「伞？我倒是记得。」`）
                    // 上面那个 named 判断帮不上忙——文本里没有名字可认。
                    // 解析层的 push_block 已经剥过一次，这里再剥一次是因为
                    // `Event` 也可能从别处构造（会话恢复、外部注入），
                    // 渲染器不能假设上游一定干净。剥是幂等的，重复调用无害。
                    format!("{}：「{}」", self.actor, strip_quote_wrappers(text))
                }
            }
            EventKind::Action => format!("（{}）", self.text),
            EventKind::Thought => format!("（{}心想：{}）", self.actor, self.text),
            EventKind::Sticker => format!("{}：（发来一张表情包：{}）", self.actor, self.text),
            EventKind::Image => format!("{}：（发来一张图片：{}）", self.actor, self.text),
            EventKind::Recall => format!("【回忆】{}", self.text),
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
        // 略高于台词：用表情回应往往比说一句话更能标记关系的转折
        EventKind::Sticker => 0.45,
        // 与表情同理，而且"给你看一张照片"通常是个更重的事件
        EventKind::Image => 0.5,
        // 回合内的中间产物，本身不该被写进长期记忆
        EventKind::Recall => 0.15,
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

    /// phi4 实测：模型有时**只写引号、不写说话人名**——整行就是
    /// `「伞？我倒是记得。」`。此时 `render_line` 里那个"开场名是不是角色
    /// 自己"的判断帮不上忙（文本里没有名字可认），于是套成
    /// `林夏：「「伞？我倒是记得。」」`。读起来像解析器坏了，实际上每步都对。
    #[test]
    fn speech_that_already_has_quotes_is_not_double_wrapped() {
        let e = Event::new(EventKind::Speech, "林夏", "「伞？我倒是记得。」");
        assert_eq!(e.render_line(), "林夏：「伞？我倒是记得。」");

        // 套两层也要剥干净——只剥一层会剩下 `「你来了。」`，还是双引号
        let e2 = Event::new(EventKind::Speech, "林夏", "「「你来了。」」");
        assert_eq!(e2.render_line(), "林夏：「你来了。」");

        // 转述别人的话必须保留内层引号：剥引号只能作用于**最外层**
        let e3 = Event::new(EventKind::Speech, "林夏", "「「我不去」——他这么说过。」");
        assert_eq!(e3.render_line(), "林夏：「「我不去」——他这么说过。」");
    }

    #[test]
    fn speech_already_wearing_its_own_name_is_not_wrapped_twice() {
        // phi4（本机 Ollama，E2E）会模仿剧本格式自己写成 `林夏：「…」`。
        let e = Event::new(EventKind::Speech, "林夏", "林夏：「雨还没停。」");
        assert_eq!(e.render_line(), "林夏：「雨还没停。」");

        // 半角冒号也要认
        let e = Event::new(EventKind::Speech, "林夏", "林夏: 雨还没停。");
        assert_eq!(e.render_line(), "林夏: 雨还没停。");

        // 但以**别人**的名字开场是角色在转述，那层包装带着信息，必须留着
        let e = Event::new(EventKind::Speech, "林夏", "陈默：「我不去」——他这么说过。");
        assert_eq!(e.render_line(), "林夏：「陈默：「我不去」——他这么说过。」");

        // 正常台词照旧
        let e = Event::new(EventKind::Speech, "林夏", "不卖。");
        assert_eq!(e.render_line(), "林夏：「不卖。」");
    }

    #[test]
    fn default_importance_in_range() {
        for k in [
            EventKind::UserInput,
            EventKind::Speech,
            EventKind::Action,
            EventKind::Thought,
            EventKind::Sticker,
            EventKind::Image,
            EventKind::Recall,
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
        assert!(EventKind::Sticker.is_performance());
        assert!(EventKind::Image.is_performance());
        // 取回的资料是回合内中间产物，不进历史（否则每轮重复注入）
        assert!(!EventKind::Recall.is_performance());
        assert!(!EventKind::ToolCall.is_performance());
        assert!(!EventKind::System.is_performance());
    }

    #[test]
    fn sticker_event_keeps_id_and_readable_label_apart() {
        let e = Event::sticker("林夏", "wailing_22", "大哭");
        assert_eq!(e.kind, EventKind::Sticker);
        assert_eq!(e.sticker_id(), Some("wailing_22"));
        assert_eq!(e.text, "大哭", "进提示词的是人话，不是编号");
        assert_eq!(e.render_line(), "林夏：（发来一张表情包：大哭）");
        assert_eq!(EventKind::Sticker.as_str(), "sticker");

        // 非表情事件不该凭空长出一个编号
        let s = Event::new(EventKind::Speech, "林夏", "不卖。");
        assert!(s.sticker_id().is_none());
    }

    #[test]
    fn image_event_keeps_id_file_and_caption_apart() {
        let e = Event::image("林夏", "old_photo", "old_photo.png", "这是母亲留下的照片");
        assert_eq!(e.kind, EventKind::Image);
        assert_eq!(e.image_id(), Some("old_photo"));
        assert_eq!(e.image_file(), Some("old_photo.png"));
        assert_eq!(e.text, "这是母亲留下的照片");
        assert_eq!(
            e.render_line(),
            "林夏：（发来一张图片：这是母亲留下的照片）"
        );
        assert_eq!(EventKind::Image.as_str(), "image");
    }

    #[test]
    fn image_event_without_a_caption_still_reads_sensibly() {
        let e = Event::image("林夏", "a", "a.png", "   ");
        assert_eq!(e.text, "一张图片");
        assert!(e.render_line().contains("发来一张图片"));
    }

    #[test]
    fn recall_event_carries_the_directive_metadata() {
        let e = Event::recall("recall", "旧照片", "[回忆] 旧照片 → 2 条");
        assert_eq!(e.kind, EventKind::Recall);
        assert_eq!(e.actor, "kernel");
        assert_eq!(e.meta.get("directive").map(String::as_str), Some("recall"));
        assert_eq!(e.meta.get("query").map(String::as_str), Some("旧照片"));
        assert_eq!(e.render_line(), "【回忆】[回忆] 旧照片 → 2 条");
        assert_eq!(EventKind::Recall.as_str(), "recall");
    }
}
