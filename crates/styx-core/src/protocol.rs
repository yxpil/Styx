//! # 文本约定协议（Text Convention Protocol）
//!
//! Styx 让模型操纵内核**不依赖 function calling**。这既是有意的，也是必需的：
//!
//! - 大量能跑在本地的小模型（phi4、qwen2.5 的纯 completion 版本、各类 7B 量化
//!   权重）根本没有 tools 通道，一旦把能力挂在工具调用上，它们就退化成"只会说话"；
//! - 就算模型支持工具调用，多轮 tool loop 也会显著拉长延迟，而角色扮演对
//!   "一句话回得快"的体感要求比"一次调用做三件事"高得多。
//!
//! 于是能力被拆成**两条通道**，都用行首标签这种模型几乎零学习成本的形式表达：
//!
//! | 通道 | 标签 | 语义 | 谁消费 |
//! |---|---|---|---|
//! | 输出通道 | `[说] [做] [想] [情] [关系] [忆] [场]` | 角色"表演"了什么 | 直接落成事件 |
//! | 输出通道 | `[表情] <编号>` / `[图片] <编号>` | 角色"发"了什么素材 | 内核裁决素材合法性 |
//! | **控制通道** | `[回忆] [联想] [查阅]` | 角色**申请**一份资料 | 内核执行后**再生成一次** |
//!
//! 控制通道是本模块的主角。它带来一个**回合内的两阶段生成**：
//!
//! ```text
//!   ┌─ 阶段 1 ────────────────────────────────────────────┐
//!   │ 模型输出: [回忆] 旧照片                              │
//!   │           （只要出现控制指令，本阶段的表演一律作废） │
//!   └──────────────────────┬───────────────────────────────┘
//!                          ▼
//!        内核执行指令 → 拿到资料 → 写一条 Recall 事件（前端可见）
//!                          ▼
//!   ┌─ 阶段 2 ────────────────────────────────────────────┐
//!   │ 把资料作为"内核返回"塞回消息序列，重新生成；         │
//!   │ 这一阶段才产出真正给用户看的台词 / 动作 / 表情。     │
//!   └─────────────────────────────────────────────────────┘
//!
//! ## 为什么阶段 1 的表演要作废
//!
//! 如果保留，模型会倾向于"先编一句、再补一次检索"，检索回来的事实反而变成
//! 事后装饰——这与"记不清就去查"的意图正好相反。作废保证了检索结果**先于**
//! 表演进入上下文，模型是在知道事实之后才开口的。
//!
//! ## 跳数上限
//!
//! `protocol_hops` 默认 1（即最多一次"取资料 → 重说"）。之所以不默认放开：
//! 每一跳都是一次完整的模型调用，2 跳意味着三倍延迟；而绝大多数回合只需要
//! 一次检索。真的需要连续查两次的场合（先想起人名、再查这个人做过什么）
//! 可以由使用者在配置里调到 2。

use serde::{Deserialize, Serialize};

/// 控制指令的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveKind {
    /// 从长期记忆里取回相关片段。
    Recall,
    /// 让联想后端从某个词发散。
    Associate,
    /// 查角色卡里的世界书条目。
    Lore,
}

impl DirectiveKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DirectiveKind::Recall => "recall",
            DirectiveKind::Associate => "associate",
            DirectiveKind::Lore => "lore",
        }
    }

    /// 中文短名，用于事件流与前端展示。
    pub fn label(&self) -> &'static str {
        match self {
            DirectiveKind::Recall => "回忆",
            DirectiveKind::Associate => "联想",
            DirectiveKind::Lore => "查阅",
        }
    }
}

/// 模型发出的一条控制指令。
///
/// 解析器只负责把"查什么"记下来，**不校验任何后端是否存在**——
/// 那属于内核的执行阶段。这样解析器可以完全离线单测。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directive {
    pub kind: DirectiveKind,
    /// 查询词 / 种子词。
    pub query: String,
    /// 想要几条（模型可指定，内核会夹取到合理范围）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// 原始文本，便于前端回显"它到底请求了什么"。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw: String,
}

impl Directive {
    pub fn new(kind: DirectiveKind, query: impl Into<String>) -> Self {
        let query = query.into();
        Directive {
            kind,
            raw: format!("[{}] {}", kind.label(), query),
            query,
            limit: None,
        }
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// 角色要发出去的一张图片 / 照片。
///
/// 与 [`crate::reply::Reply::stickers`] 分开：表情包是"情绪"，
/// 走情绪词典；图片是"内容"，走素材库。两者的裁决规则完全不同，
/// 混成一类会让两边都变模糊。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ImageRequest {
    /// 素材编号 / 文件名（解析器只记录，合法性由内核裁决）。
    pub key: String,
    /// 配文，可以是空的。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caption: String,
}

/// 一条控制指令的执行结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectiveOutcome {
    pub kind: DirectiveKind,
    pub query: String,
    /// 人类可读的一句话摘要（进事件流）。
    pub summary: String,
    /// 命中的条目。
    #[serde(default)]
    pub items: Vec<String>,
    /// 执行失败的原因；成功时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DirectiveOutcome {
    /// 一次成功但**没有命中**的查询。
    ///
    /// 刻意与"失败"分开：`[回忆] 火星殖民地` 查不到东西是正常结果，
    /// 模型应当据此"我确实想不起来了"来表演，而不是收到一条错误。
    pub fn empty(kind: DirectiveKind, query: impl Into<String>) -> Self {
        let query = query.into();
        DirectiveOutcome {
            summary: format!("[{}] {} → 没有找到相关记录", kind.label(), query),
            kind,
            query,
            items: Vec::new(),
            error: None,
        }
    }

    pub fn hit(kind: DirectiveKind, query: impl Into<String>, items: Vec<String>) -> Self {
        let query = query.into();
        DirectiveOutcome {
            summary: format!("[{}] {} → {} 条", kind.label(), query, items.len()),
            kind,
            query,
            items,
            error: None,
        }
    }

    pub fn failed(kind: DirectiveKind, query: impl Into<String>, error: impl Into<String>) -> Self {
        let query = query.into();
        let error = error.into();
        DirectiveOutcome {
            summary: format!("[{}] {} → 查询失败：{}", kind.label(), query, error),
            kind,
            query,
            items: Vec::new(),
            error: Some(error),
        }
    }

    /// 是否真的取到了东西。
    pub fn has_items(&self) -> bool {
        !self.items.is_empty()
    }
}

/// 生成"内核返回"续写提示。
///
/// 这段文字是两阶段生成的关键接缝：它必须做到三件事——
/// 告诉模型资料到了、明确禁止重复请求同一份资料、允许它老实承认没查到。
pub fn continuation_prompt(outcomes: &[DirectiveOutcome]) -> String {
    let mut out = String::from(
        "【内核返回】你刚才请求查询的资料如下。现在请**直接正式表演**（[说]/[做]/[想] 等），\
         不要重复输出已经查过的 [回忆]/[联想]/[查阅] 指令。\n",
    );
    for (i, o) in outcomes.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n", i + 1, o.summary));
        for item in &o.items {
            out.push_str(&format!("   - {item}\n"));
        }
    }
    if outcomes.iter().all(|o| !o.has_items()) {
        out.push_str(
            "\n一份都没查到。请按「确实想不起来 / 不知道」来演，\
             **不要编造**查询结果里不存在的事实。\n",
        );
    }
    out.push_str(
        "\n注意：以上资料是你「想起来」的内容，可以自然引用，但不要逐字复述成一长串清单。",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_keeps_kind_query_and_limit() {
        let d = Directive::new(DirectiveKind::Recall, "旧照片").with_limit(3);
        assert_eq!(d.kind, DirectiveKind::Recall);
        assert_eq!(d.query, "旧照片");
        assert_eq!(d.limit, Some(3));
        assert_eq!(d.raw, "[回忆] 旧照片");
        assert_eq!(DirectiveKind::Recall.as_str(), "recall");
        assert_eq!(DirectiveKind::Recall.label(), "回忆");
    }

    #[test]
    fn outcome_shapes_are_distinguishable() {
        let none = DirectiveOutcome::empty(DirectiveKind::Recall, "火星殖民地");
        assert!(!none.has_items());
        assert!(none.error.is_none());
        assert!(none.summary.contains("没有找到"));

        let hit = DirectiveOutcome::hit(DirectiveKind::Recall, "照片", vec!["甲".into()]);
        assert!(hit.has_items());
        assert!(hit.summary.contains("1 条"));

        let bad = DirectiveOutcome::failed(DirectiveKind::Associate, "母亲", "后端离线");
        assert!(!bad.has_items());
        assert_eq!(bad.error.as_deref(), Some("后端离线"));
        assert!(bad.summary.contains("失败"));
    }

    #[test]
    fn continuation_prompt_lists_results() {
        let outcomes = vec![
            DirectiveOutcome::hit(DirectiveKind::Recall, "照片", vec!["母亲的照片".into()]),
            DirectiveOutcome::empty(DirectiveKind::Associate, "旧书"),
        ];
        let p = continuation_prompt(&outcomes);
        assert!(p.contains("内核返回"));
        assert!(p.contains("母亲的照片"));
        assert!(p.contains("没有找到"));
        // 有命中时不该说"一份都没查到"
        assert!(!p.contains("一份都没查到"));
    }

    #[test]
    fn continuation_prompt_forbids_fabrication_when_all_missed() {
        let outcomes = vec![DirectiveOutcome::empty(DirectiveKind::Recall, "不存在的事")];
        let p = continuation_prompt(&outcomes);
        assert!(p.contains("一份都没查到"));
        assert!(p.contains("不要编造"));
    }

    #[test]
    fn image_request_defaults_to_empty_caption() {
        let r = ImageRequest {
            key: "photo_01".into(),
            ..Default::default()
        };
        assert!(r.caption.is_empty());
        assert_eq!(r.key, "photo_01");
    }
}
