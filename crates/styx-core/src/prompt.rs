//! 提示词装配：把散落的状态拼成一次可发送的请求，并且**不超预算**。
//!
//! # 为什么要显式管预算
//!
//! 角色扮演的上下文是竞争性的：角色卡不能删、场景不能删，但记忆和历史可以。
//! 如果不显式分配预算，通常的结局是——长跑几十个回合后，历史把记忆挤没了，
//! 角色开始"忘事"。所以这里把预算切成若干段，**每段独立裁剪**，
//! 并且把裁剪结果写进 [`PromptReport`] 供观测。
//!
//! 段的优先级（从高到低）：
//! `角色卡 > 场景 > 当下状态 > 对话历史 > 长期记忆 > 联想 > 共享记忆`
//!
//! 历史不足预算时会把剩余额度**让给记忆**，反之亦然——这是唯一一处"动态借额"，
//! 因为它显著改善了长会话的体验。

use std::collections::BTreeMap;

use crate::character::CharacterCard;
use crate::event::{Event, EventKind};
use crate::ports::{Association, ChatMessage, Recalled};
use crate::reply::Reply;
use crate::scene::Scene;
use crate::state::DynamicState;
use crate::text::{estimate_tokens, summarize, truncate_to_tokens};

/// 提示词预算（单位：估算 token）。
#[derive(Debug, Clone)]
pub struct PromptBudget {
    /// 总预算。
    pub total: usize,
    /// 长期记忆段。
    pub memory: usize,
    /// 联想段。
    pub assoc: usize,
    /// 共享记忆池段。
    pub pool: usize,
    /// 对话历史段。
    pub history: usize,
    /// 对话历史最多保留的事件条数。
    pub history_events: usize,
}

impl Default for PromptBudget {
    fn default() -> Self {
        PromptBudget {
            total: 6000,
            memory: 1200,
            assoc: 400,
            pool: 400,
            history: 2400,
            history_events: 40,
        }
    }
}

impl PromptBudget {
    /// 小窗口模型的紧凑预算。
    pub fn compact() -> Self {
        PromptBudget {
            total: 3000,
            memory: 600,
            assoc: 200,
            pool: 200,
            history: 1000,
            history_events: 16,
        }
    }

    /// 大窗口模型的宽裕预算。
    pub fn large() -> Self {
        PromptBudget {
            total: 16000,
            memory: 4000,
            assoc: 1000,
            pool: 1000,
            history: 7000,
            history_events: 80,
        }
    }
}

/// 装配结果。
#[derive(Debug, Clone)]
pub struct PromptPlan {
    /// 系统提示。
    pub system: String,
    /// 完整消息序列。
    pub messages: Vec<ChatMessage>,
    /// 装配报告。
    pub report: PromptReport,
}

/// 装配报告：说明"这次到底喂了什么进去"。
#[derive(Debug, Clone, Default)]
pub struct PromptReport {
    pub estimated_tokens: usize,
    pub budget_total: usize,
    /// 各段实际占用。
    pub sections: BTreeMap<String, usize>,
    /// 进了提示词的记忆条数。
    pub memories_used: usize,
    /// 因预算被丢弃的记忆条数。
    pub memories_dropped: usize,
    /// 进了提示词的联想条数。
    pub associations_used: usize,
    /// 进了提示词的共享记忆条数。
    pub pool_used: usize,
    /// 进了提示词的历史事件条数。
    pub history_events: usize,
    /// 命中的世界书条目数。
    pub lore_hits: usize,
    /// 降级 / 告警（例如"记忆后端不可用"）。
    pub notices: Vec<String>,
    /// 是否真的超了总预算（正常情况下不会）。
    pub over_budget: bool,
}

impl PromptReport {
    /// 一行摘要。
    pub fn render(&self) -> String {
        let mut parts = vec![format!(
            "提示≈{}/{} tok",
            self.estimated_tokens, self.budget_total
        )];
        parts.push(format!("记忆 {}条", self.memories_used));
        if self.memories_dropped > 0 {
            parts.push(format!("丢弃 {}条", self.memories_dropped));
        }
        parts.push(format!("联想 {}条", self.associations_used));
        parts.push(format!("共享 {}条", self.pool_used));
        parts.push(format!("历史 {}条", self.history_events));
        if self.lore_hits > 0 {
            parts.push(format!("世界书 {}条", self.lore_hits));
        }
        let mut s = parts.join(" · ");
        if !self.notices.is_empty() {
            s.push_str("  ⚠ ");
            s.push_str(&self.notices.join("；"));
        }
        s
    }
}

/// 提示词装配器。
pub struct PromptBuilder<'a> {
    pub card: &'a CharacterCard,
    pub scene: &'a Scene,
    pub state: &'a DynamicState,
    pub budget: &'a PromptBudget,
    /// 对话历史事件（应已按时间正序）。
    pub history: &'a [Event],
    /// 用户在故事里的身份名（用于提示中区分"谁在说话"）。
    pub user_role: &'a str,
    /// 本轮用户输入。
    pub user_input: &'a str,
}

impl<'a> PromptBuilder<'a> {
    /// 组装提示词。
    pub fn build(
        &self,
        memories: &[Recalled],
        associations: &[Association],
        pool: &[Recalled],
        notices: &[String],
    ) -> PromptPlan {
        let mut report = PromptReport {
            budget_total: self.budget.total,
            notices: notices.to_vec(),
            ..Default::default()
        };
        let mut system = String::with_capacity(2048);

        // ---- 1. 总纲（固定，不可裁剪）----
        system.push_str(&self.preamble());
        report
            .sections
            .insert("preamble".into(), estimate_tokens(&self.preamble()));

        // ---- 2. 角色卡（固定）----
        let card_block = self.character_block();
        let card_tokens = estimate_tokens(&card_block);
        report.sections.insert("character".into(), card_tokens);
        system.push_str(&card_block);

        // ---- 3. 世界书（按关键词触发，按优先级塞到上限）----
        let (lore_block, lore_hits) = self.lore_block();
        report.lore_hits = lore_hits;
        if !lore_block.is_empty() {
            report
                .sections
                .insert("lore".into(), estimate_tokens(&lore_block));
            system.push_str(&lore_block);
        }

        // ---- 4. 场景（固定）----
        let scene_block = format!("\n## 场景\n{}\n", self.scene.render());
        report
            .sections
            .insert("scene".into(), estimate_tokens(&scene_block));
        system.push_str(&scene_block);

        // ---- 5. 当下状态（固定）----
        let state_block = format!("\n## 我此刻的状态\n{}\n", self.state.render());
        report
            .sections
            .insert("state".into(), estimate_tokens(&state_block));
        system.push_str(&state_block);

        // ---- 6. 长期记忆（可裁剪）----
        let (mem_block, used, dropped) = self.memory_block(memories);
        report.memories_used = used;
        report.memories_dropped = dropped;
        if !mem_block.is_empty() {
            report
                .sections
                .insert("memory".into(), estimate_tokens(&mem_block));
            system.push_str(&mem_block);
        }

        // ---- 7. 联想（可裁剪）----
        let (assoc_block, assoc_used) = self.assoc_block(associations);
        report.associations_used = assoc_used;
        if !assoc_block.is_empty() {
            report
                .sections
                .insert("assoc".into(), estimate_tokens(&assoc_block));
            system.push_str(&assoc_block);
        }

        // ---- 8. 共享记忆（可裁剪）----
        let (pool_block, pool_used) = self.pool_block(pool);
        report.pool_used = pool_used;
        if !pool_block.is_empty() {
            report
                .sections
                .insert("pool".into(), estimate_tokens(&pool_block));
            system.push_str(&pool_block);
        }

        // ---- 9. 提醒（一致性守护）----
        if !notices.is_empty() {
            let mut n = String::from("\n## 必须遵守\n");
            for x in notices {
                n.push_str(&format!("- {x}\n"));
            }
            report
                .sections
                .insert("notices".into(), estimate_tokens(&n));
            system.push_str(&n);
        }

        // ---- 10. 输出格式（固定）----
        let fmt = format!("\n## 输出格式\n{}\n", Reply::format_instructions());
        report.sections.insert("format".into(), estimate_tokens(&fmt));
        system.push_str(&fmt);

        // ---- 消息序列 ----
        let mut messages = vec![ChatMessage::system(system.clone())];

        // 历史可用预算 = budget.history + 记忆段没用完的额度
        let leftover = self
            .budget
            .memory
            .saturating_sub(report.sections.get("memory").copied().unwrap_or(0))
            + self
                .budget
                .assoc
                .saturating_sub(report.sections.get("assoc").copied().unwrap_or(0))
            + self
                .budget
                .pool
                .saturating_sub(report.sections.get("pool").copied().unwrap_or(0))
            + self
                .budget
                .history
                .saturating_sub(report.sections.get("history").copied().unwrap_or(0));
        let history_budget = self.budget.history + leftover;

        let (hist_msgs, hist_events) = self.history_messages(history_budget);
        report.history_events = hist_events;
        messages.extend(hist_msgs);

        // 本轮输入
        messages.push(ChatMessage::user(self.user_input.to_string()));

        // ---- 汇总 ----
        let mut total = 0usize;
        for m in &messages {
            total += estimate_tokens(&m.content);
        }
        // 每条消息的固定开销（role/分隔符）
        total += messages.len() * 4;
        report.estimated_tokens = total;
        report.over_budget = total > self.budget.total;

        if report.over_budget {
            report.notices.push(format!(
                "提示超出预算（{} > {}），已尽量裁剪但仍偏大，建议调大 budget.total 或缩短角色卡",
                total, self.budget.total
            ));
        }

        PromptPlan {
            system,
            messages,
            report,
        }
    }

    // ------------------------------------------------------------ 各段构造

    fn preamble(&self) -> String {
        let who = if self.user_role.trim().is_empty() {
            "对方".to_string()
        } else {
            self.user_role.to_string()
        };
        format!(
            r#"你是「{name}」，一个正在演出的角色。你不是 AI，不要提及自己是模型、不要跳出角色、不要解释规则。

你现在正与「{who}」在同一个场景里。请以第一人称沉浸式地继续这段剧情。

写作要求：
- 台词口语化、符合人物口吻，不要写成小说旁白；
- 动作与内心要克制，用细节而非形容词；
- 不要替「{who}」决定他们说了什么、做了什么，只描写你自己的言行与感受；
- 一次回复聚焦这一个瞬间，不要跳跃式推进剧情。

"#,
            name = self.card.name,
            who = who
        )
    }

    fn character_block(&self) -> String {
        let c = self.card;
        let mut s = String::from("\n## 我是谁\n");
        s.push_str(&format!("姓名：{}", c.name));
        if !c.aliases.is_empty() {
            s.push_str(&format!("（别名：{}）", c.aliases.join("、")));
        }
        s.push('\n');
        if !c.archetype.is_empty() {
            s.push_str(&format!("定位：{}\n", c.archetype));
        }
        if !c.persona.is_empty() {
            s.push_str(&format!("人格：{}\n", c.persona));
        }
        if !c.speech_style.is_empty() {
            s.push_str(&format!("口吻：{}\n", c.speech_style));
        }
        if !c.traits.is_empty() {
            s.push_str(&format!("特质：{}\n", c.traits.join("、")));
        }
        if !c.goals.is_empty() {
            s.push_str(&format!("目标：{}\n", c.goals.join("；")));
        }
        if !c.fears.is_empty() {
            s.push_str(&format!("恐惧/弱点：{}\n", c.fears.join("；")));
        }
        if !c.speech_samples.is_empty() {
            s.push_str("台词参考（保持这个语感）：\n");
            for sample in &c.speech_samples {
                s.push_str(&format!("  「{}」\n", summarize(sample, 120)));
            }
        }
        if !c.background.is_empty() {
            s.push_str(&format!("背景：{}\n", c.background));
        }
        s
    }

    fn lore_block(&self) -> (String, usize) {
        let hits = self.card.triggered_lore(self.user_input);
        if hits.is_empty() {
            return (String::new(), 0);
        }
        // 世界书段独占预算：320 token
        let cap = 320usize;
        let mut s = String::from("\n## 设定（本轮相关）\n");
        let mut used = 0usize;
        let mut count = 0usize;
        for entry in &hits {
            let body = truncate_to_tokens(&entry.text, cap.saturating_sub(used).min(160));
            if body.is_empty() {
                break;
            }
            let line = format!("- {body}\n");
            used += estimate_tokens(&line);
            if used > cap {
                break;
            }
            s.push_str(&line);
            count += 1;
        }
        if count == 0 {
            return (String::new(), 0);
        }
        (s, count)
    }

    fn memory_block(&self, memories: &[Recalled]) -> (String, usize, usize) {
        if memories.is_empty() {
            return (String::new(), 0, 0);
        }
        let mut sorted: Vec<&Recalled> = memories.iter().collect();
        sorted.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut s = String::from("\n## 我还记得的事\n");
        let mut used = estimate_tokens(&s);
        let mut count = 0usize;
        let mut dropped = 0usize;
        for m in sorted {
            // 重要度高的记忆给更长额度
            let quota = if m.importance >= 0.7 { 120 } else { 64 };
            let text = summarize(&m.text, quota * 2);
            let line = format!("- {text}\n");
            let cost = estimate_tokens(&line);
            if used + cost > self.budget.memory {
                dropped += 1;
                continue;
            }
            used += cost;
            s.push_str(&line);
            count += 1;
        }
        if count == 0 {
            return (String::new(), 0, dropped);
        }
        (s, count, dropped)
    }

    fn assoc_block(&self, associations: &[Association]) -> (String, usize) {
        let mut sorted: Vec<&Association> = associations
            .iter()
            // 低置信度的联想不配进提示词（对应 MightBe 的弃判语义）
            .filter(|a| a.confidence >= 0.25)
            .collect();
        if sorted.is_empty() {
            return (String::new(), 0);
        }
        sorted.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut s = String::from("\n## 这个词让我想到了\n");
        let mut used = estimate_tokens(&s);
        let mut count = 0usize;
        for a in sorted {
            let line = format!("- {}（关联{:.2}）\n", a.word, a.score);
            let cost = estimate_tokens(&line);
            if used + cost > self.budget.assoc {
                break;
            }
            used += cost;
            s.push_str(&line);
            count += 1;
        }
        if count == 0 {
            return (String::new(), 0);
        }
        (s, count)
    }

    fn pool_block(&self, pool: &[Recalled]) -> (String, usize) {
        if pool.is_empty() {
            return (String::new(), 0);
        }
        let mut s = String::from("\n## 其他人留下的记录\n");
        let mut used = estimate_tokens(&s);
        let mut count = 0usize;
        for m in pool {
            let line = format!("- {}\n", summarize(&m.text, 120));
            let cost = estimate_tokens(&line);
            if used + cost > self.budget.pool {
                break;
            }
            used += cost;
            s.push_str(&line);
            count += 1;
        }
        if count == 0 {
            return (String::new(), 0);
        }
        (s, count)
    }

    /// 历史 → 消息序列（把连续的表演事件合并成一条 assistant 消息）。
    fn history_messages(&self, budget: usize) -> (Vec<ChatMessage>, usize) {
        let mut events: Vec<&Event> = self
            .history
            .iter()
            .filter(|e| e.kind.is_performance())
            .collect();
        if events.len() > self.budget.history_events {
            events.drain(..events.len() - self.budget.history_events);
        }

        // 从尾部往回取，直到用完预算
        let mut picked: Vec<&Event> = Vec::new();
        let mut used = 0usize;
        for e in events.iter().rev() {
            let cost = estimate_tokens(&e.render_line());
            if used + cost > budget && !picked.is_empty() {
                break;
            }
            used += cost;
            picked.push(e);
        }
        picked.reverse();

        // 合并成消息
        let mut msgs: Vec<ChatMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();
        let mut pending_is_user = false;
        let flush = |msgs: &mut Vec<ChatMessage>, pending: &mut Vec<String>, is_user: bool| {
            if pending.is_empty() {
                return;
            }
            let body = pending.join("\n");
            msgs.push(if is_user {
                ChatMessage::user(body)
            } else {
                ChatMessage::assistant(body)
            });
            pending.clear();
        };

        for e in &picked {
            let is_user = e.kind == EventKind::UserInput;
            if is_user != pending_is_user && !pending.is_empty() {
                flush(&mut msgs, &mut pending, pending_is_user);
            }
            pending_is_user = is_user;
            pending.push(e.render_line());
        }
        flush(&mut msgs, &mut pending, pending_is_user);

        (msgs, picked.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use crate::state::DynamicState;

    fn card() -> CharacterCard {
        CharacterCard::parse_markdown(
            r#"
# 林夏

## 定位
外冷内热的旧书店主

## 人格
话少，先观察再开口。

## 口吻
短句。

## 示例台词
- 不卖。

## 禁则
- 绝不承认自己害怕孤独

## 世界书
- 旧书店,书店 :: 店里有一只老猫，从不叫。 :: 5

## 背景
城南老街。
"#,
        )
        .unwrap()
    }

    fn events() -> Vec<Event> {
        let mut v = Vec::new();
        let mut e = Event::new(EventKind::UserInput, "陈默", "我想买那本旧相册。");
        e.seq = 1;
        v.push(e);
        let mut e = Event::new(EventKind::Speech, "林夏", "不卖。");
        e.seq = 2;
        v.push(e);
        v
    }

    fn build(
        budget: &PromptBudget,
        input: &str,
        memories: &[Recalled],
        assoc: &[Association],
    ) -> PromptPlan {
        let card = card();
        let scene = Scene::new("拾光旧书店");
        let state = DynamicState::from_card(&card);
        let hist = events();
        let builder = PromptBuilder {
            card: &card,
            scene: &scene,
            state: &state,
            budget,
            history: &hist,
            user_role: "陈默",
            user_input: input,
        };
        builder.build(memories, assoc, &[], &[])
    }

    #[test]
    fn assembles_all_sections() {
        let mem = vec![
            Recalled::new("1", "陈默三年前借走了一本《海边的卡夫卡》", 0.9, "nebula"),
            Recalled::new("2", "母亲留下了一张旧照片", 0.7, "nebula"),
        ];
        let assoc = vec![Association::new("相册", 0.8).with_confidence(0.9)];
        let plan = build(&PromptBudget::default(), "我们去旧书店看看那本相册", &mem, &assoc);

        assert!(plan.system.contains("你是「林夏」"));
        assert!(plan.system.contains("外冷内热的旧书店主"));
        assert!(plan.system.contains("拾光旧书店"));
        assert!(plan.system.contains("我还记得的事"));
        assert!(plan.system.contains("海边的卡夫卡"));
        assert!(plan.system.contains("这个词让我想到了"));
        assert!(plan.system.contains("相册"));
        assert!(plan.system.contains("设定（本轮相关）"));
        assert!(plan.system.contains("老猫"));
        assert!(plan.report.memories_used == 2);
        assert_eq!(plan.report.associations_used, 1);
        assert_eq!(plan.report.lore_hits, 1);

        // 消息序列：system + (user,assistant) + user
        assert_eq!(plan.messages[0].role, "system");
        assert_eq!(plan.messages.last().unwrap().role, "user");
        assert!(plan
            .messages
            .last()
            .unwrap()
            .content
            .contains("我们去旧书店"));
    }

    #[test]
    fn low_confidence_associations_are_filtered_out() {
        let assoc = vec![
            Association::new("噪音", 9.0).with_confidence(0.1),
            Association::new("相册", 0.5).with_confidence(0.8),
        ];
        let plan = build(&PromptBudget::default(), "x", &[], &assoc);
        assert!(plan.system.contains("相册"));
        assert!(!plan.system.contains("噪音"));
        assert_eq!(plan.report.associations_used, 1);
    }

    #[test]
    fn memory_budget_is_respected_and_drops_counted() {
        let mut mem = Vec::new();
        for i in 0..200 {
            mem.push(Recalled::new(
                i.to_string(),
                format!("第{i}条记忆：这是一段用来把记忆预算撑爆的长文本内容，重复重复重复重复重复"),
                1.0 - i as f32 * 0.001,
                "nebula",
            ));
        }
        let budget = PromptBudget::default();
        let plan = build(&budget, "x", &mem, &[]);
        let mem_section = plan.report.sections.get("memory").copied().unwrap_or(0);
        assert!(
            mem_section <= budget.memory + 40,
            "memory section used {mem_section} > {}",
            budget.memory
        );
        assert!(plan.report.memories_dropped > 0);
        assert!(plan.report.memories_used > 0);
    }

    #[test]
    fn history_is_trimmed_from_oldest() {
        let card = card();
        let scene = Scene::new("书店");
        let state = DynamicState::default();
        let mut hist = Vec::new();
        for i in 0..60 {
            let mut e = Event::new(
                EventKind::UserInput,
                "陈默",
                format!("第{i}句话，长度足够把历史预算撑开一点"),
            );
            e.seq = i;
            hist.push(e);
        }
        let budget = PromptBudget::default();
        let builder = PromptBuilder {
            card: &card,
            scene: &scene,
            state: &state,
            budget: &budget,
            history: &hist,
            user_role: "陈默",
            user_input: "继续",
        };
        let plan = builder.build(&[], &[], &[], &[]);
        assert!(plan.report.history_events < 60);
        assert!(plan.report.history_events > 0);
        // 最后一句一定在
        let joined: String = plan
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("第59句话"));
        assert!(!joined.contains("第0句话"));
    }

    #[test]
    fn history_merges_consecutive_assistant_events() {
        let plan = build(&PromptBudget::default(), "在吗", &[], &[]);
        // 历史：1 条 user（陈默）+ 1 条 assistant（林夏）
        let roles: Vec<&str> = plan.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
        let assistant = &plan.messages[2];
        assert!(assistant.content.contains("不卖。"));
    }

    #[test]
    fn notices_render_into_prompt_and_report() {
        let card = card();
        let scene = Scene::new("书店");
        let state = DynamicState::default();
        let hist: Vec<Event> = Vec::new();
        let budget = PromptBudget::compact();
        let builder = PromptBuilder {
            card: &card,
            scene: &scene,
            state: &state,
            budget: &budget,
            history: &hist,
            user_role: "陈默",
            user_input: "hi",
        };
        let notices = vec!["长期记忆后端不可用，本回合没有记忆".to_string()];
        let plan = builder.build(&[], &[], &[], &notices);
        assert!(plan.system.contains("必须遵守"));
        assert!(plan.system.contains("长期记忆后端不可用"));
        assert!(plan.report.render().contains("记忆后端不可用"));
    }

    #[test]
    fn no_history_still_produces_valid_messages() {
        let card = card();
        let scene = Scene::new("书店");
        let state = DynamicState::default();
        let hist: Vec<Event> = Vec::new();
        let budget = PromptBudget::default();
        let builder = PromptBuilder {
            card: &card,
            scene: &scene,
            state: &state,
            budget: &budget,
            history: &hist,
            user_role: "陈默",
            user_input: "开门了吗",
        };
        let plan = builder.build(&[], &[], &[], &[]);
        assert_eq!(plan.messages.len(), 2);
        assert_eq!(plan.report.history_events, 0);
        assert!(!plan.report.over_budget);
    }

    #[test]
    fn report_render_mentions_dropped() {
        let mem: Vec<Recalled> = (0..80)
            .map(|i| {
                Recalled::new(
                    i.to_string(),
                    "很长的一条记忆内容用来触发丢弃计数的统计逻辑".repeat(3),
                    1.0,
                    "nebula",
                )
            })
            .collect();
        let plan = build(&PromptBudget::default(), "x", &mem, &[]);
        assert!(plan.report.memories_dropped > 0);
        assert!(plan.report.render().contains("丢弃"));
    }
}
