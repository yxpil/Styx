//! 会话：内核里**所有可变数据**的容器，以及一致性守护。
//!
//! 把"存什么"（[`Session`]）与"怎么编排"（[`crate::kernel::Kernel`]）分开，
//! 是为了让会话可以整体快照 / 恢复 / 序列化落盘，而编排逻辑保持无状态。

use serde::{Deserialize, Serialize};

use crate::character::CharacterCard;
use crate::event::{now_millis, Event, EventKind};
use crate::reply::Reply;
use crate::scene::Scene;
use crate::state::{DynamicState, StateDelta};
use crate::text::{estimate_tokens, truncate_tail_to_tokens};

/// 一次会话的完整可变状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// 会话 id。
    pub id: String,
    /// 角色卡（运行期不变，但会被快照）。
    pub card: CharacterCard,
    /// 当前场景。
    pub scene: Scene,
    /// 动态状态。
    pub state: DynamicState,
    /// 事件流（事实来源）。
    pub transcript: Vec<Event>,
    /// 事件序号游标。
    pub seq: u64,
    /// 会话开始时间。
    pub started_at: i64,
}

impl Session {
    /// 新建会话（用角色卡的初始关系初始化动态状态）。
    pub fn new(card: CharacterCard, scene: Scene) -> Self {
        let mut state = DynamicState::from_card(&card);
        state.mood.label = "平静".into();
        Session {
            id: format!("{}-{}", card.namespace(), now_millis()),
            card,
            scene,
            state,
            transcript: Vec::new(),
            seq: 0,
            started_at: now_millis(),
        }
    }

    /// 追加一条事件（自动分配序号与时间戳）。
    pub fn push(&mut self, kind: EventKind, actor: impl Into<String>, text: impl Into<String>) -> u64 {
        let mut e = Event::new(kind, actor, text);
        self.seq += 1;
        e.seq = self.seq;
        let seq = e.seq;
        self.transcript.push(e);
        seq
    }

    /// 追加一条已构造的事件。
    pub fn push_event(&mut self, mut e: Event) -> u64 {
        self.seq += 1;
        e.seq = self.seq;
        if e.at == 0 {
            e.at = now_millis();
        }
        let seq = e.seq;
        self.transcript.push(e);
        seq
    }

    /// 最近 `n` 条"表演类"事件（进提示词的对话历史）。
    pub fn recent_performance(&self, n: usize) -> Vec<&Event> {
        let mut out: Vec<&Event> = self
            .transcript
            .iter()
            .filter(|e| e.kind.is_performance())
            .collect();
        if out.len() > n {
            out.drain(..out.len() - n);
        }
        out
    }

    /// 把最近的历史渲染成一段受 token 预算约束的文本。
    ///
    /// 预算不足时**从最旧的开始丢**（新的对话更重要）。
    pub fn render_history(&self, max_events: usize, max_tokens: usize) -> String {
        let events = self.recent_performance(max_events);
        let mut lines: Vec<String> = events.iter().map(|e| e.render_line()).collect();

        // 逐条从头部丢弃，直到放进预算
        loop {
            let joined = lines.join("\n");
            if lines.is_empty() || estimate_tokens(&joined) <= max_tokens {
                return joined;
            }
            lines.remove(0);
        }
    }

    /// 应用一次状态增量并推进回合。
    pub fn settle(&mut self, delta: &StateDelta, decay_rate: f32) {
        self.state.apply(delta);
        self.state.turn += 1;
        self.state.decay(decay_rate);
    }

    /// 把场景变更应用到当前场景。
    pub fn apply_scene(&mut self, set: &std::collections::BTreeMap<String, String>) -> Vec<String> {
        let mut changed = Vec::new();
        for (k, v) in set {
            let slot = match k.as_str() {
                "world" => &mut self.scene.world,
                "location" => &mut self.scene.location,
                "time" => &mut self.scene.time,
                "weather" => &mut self.scene.weather,
                "situation" => &mut self.scene.situation,
                "tone" => &mut self.scene.tone,
                "present" => {
                    let names: Vec<String> = v
                        .split(['、', ',', '，'])
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if self.scene.present != names {
                        self.scene.present = names;
                        changed.push(format!("在场={v}"));
                    }
                    continue;
                }
                "user_role" => &mut self.scene.user_role,
                _ => continue,
            };
            if *slot != *v {
                *slot = v.clone();
                changed.push(format!("{k}={v}"));
            }
        }
        if !changed.is_empty() {
            self.push(EventKind::SceneChange, "system", changed.join("，"));
        }
        changed
    }

    /// 导出快照（可直接落盘）。
    pub fn snapshot_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// 从快照恢复。
    pub fn from_snapshot_json(src: &str) -> crate::error::Result<Self> {
        Ok(serde_json::from_str(src)?)
    }

    /// 内存占用粗估（字符数），用于观测。
    pub fn approx_chars(&self) -> usize {
        self.transcript.iter().map(|e| e.text.chars().count() + 32).sum()
    }
}

/// 一致性审计结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Audit {
    /// 硬违规（必须重试）。
    pub violations: Vec<String>,
    /// 软提醒（下次提示里强化，但不重试）。
    pub warnings: Vec<String>,
}

impl Audit {
    /// 是否需要重试生成。
    pub fn needs_retry(&self) -> bool {
        !self.violations.is_empty()
    }

    /// 渲染成给模型看的修正指令。
    pub fn retry_instruction(&self) -> String {
        let mut s = String::from("上一轮输出违反了角色设定，请重写：\n");
        for (i, v) in self.violations.iter().enumerate() {
            s.push_str(&format!("{}. {}\n", i + 1, v));
        }
        s.push_str("保持同样的剧情走向与情绪，只修正上述问题。");
        s
    }
}

/// 一致性守护：轻量、可解释、可测试的"人设漂移"检查。
///
/// 刻意**不做**语义判断（那需要另一个模型，会引入不确定性与成本），
/// 只做三类确定性检查：
///
/// 1. **禁用词**：出现了直接判违规；
/// 2. **口吻漂移**：台词长度显著偏离角色卡里的示例台词（例如示例都是短句，
///    却输出了三百字独白）；
/// 3. **禁则提醒**：把角色卡的禁则原样带进重试指令，让模型自己约束。
pub struct Guard;

impl Guard {
    /// 审计一次回复。
    pub fn audit(card: &CharacterCard, reply: &Reply) -> Audit {
        let mut audit = Audit::default();
        let speech_all = reply.speech.join("\n");

        // 1) 禁用词
        for phrase in &card.banned_phrases {
            let p = phrase.trim();
            if p.is_empty() {
                continue;
            }
            if speech_all.contains(p) {
                audit
                    .violations
                    .push(format!("出现了禁用词/句式「{p}」，必须去掉。"));
            }
        }

        // 2) 口吻漂移：台词长度
        if let Some(sample_avg) = average_speech_len(&card.speech_samples) {
            let long_ones: Vec<usize> = reply
                .speech
                .iter()
                .map(|s| s.chars().count())
                .filter(|n| *n as f32 > sample_avg * 4.0 && *n > 60)
                .collect();
            if !long_ones.is_empty() {
                audit.warnings.push(format!(
                    "台词长度（{} 字）明显长于角色示例（约 {:.0} 字/句），注意保持简洁的口吻。",
                    long_ones[0], sample_avg
                ));
            }
        }

        // 3) 完全没有可看的东西：发一张表情包或一张图片而不说话都是**合法表达**，
        //    真正要拦的是"既没有说话、也没有图"的空回合
        if reply.speech.is_empty() && reply.stickers.is_empty() && reply.images.is_empty() {
            audit
                .violations
                .push("缺少 [说] 台词，请至少给出一句对话。".to_string());
        }

        // 4) 禁则：作为提醒带进重试指令（不做自动判定）
        if !card.boundaries.is_empty() {
            audit.warnings.push(format!(
                "仍然必须遵守禁则：{}。",
                card.boundaries.join("；")
            ));
        }
        audit
    }

    /// 把审计出的硬违规与禁则拼成一段"下一回合的系统提醒"。
    pub fn reminder(card: &CharacterCard, audit: &Audit) -> Option<String> {
        if audit.violations.is_empty() && card.boundaries.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        if !card.boundaries.is_empty() {
            parts.push(format!("【禁则】{}", card.boundaries.join("；")));
        }
        if !card.banned_phrases.is_empty() {
            parts.push(format!("【禁用词】{}", card.banned_phrases.join("、")));
        }
        // 禁则已经在上面单独成段（【禁则】），warnings 里那条只是
        // 复述同一段文本，这里跳过，避免提醒里出现两遍一模一样的禁则。
        for v in &audit.warnings {
            if card.boundaries.iter().any(|b| v.contains(b)) {
                continue;
            }
            parts.push(format!("【提醒】{v}"));
        }
        Some(parts.join("\n"))
    }
}

fn average_speech_len(samples: &[String]) -> Option<f32> {
    let lens: Vec<usize> = samples
        .iter()
        .map(|s| s.chars().count())
        .filter(|n| *n > 0)
        .collect();
    if lens.is_empty() {
        return None;
    }
    Some(lens.iter().sum::<usize>() as f32 / lens.len() as f32)
}

/// 把长文本压进预算（供 kernel 组装共享记忆段使用）。
pub fn fit(text: &str, tokens: usize) -> String {
    truncate_tail_to_tokens(text, tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;

    fn card() -> CharacterCard {
        CharacterCard::parse_markdown(
            r#"
# 林夏

## 定位
外冷内热的旧书店主

## 口吻
短句。不轻易用感叹号。

## 示例台词
- 不卖。
- 那本书，你翻得太用力了。

## 禁则
- 绝不承认自己害怕孤独

## 禁用词
- 绝绝子
"#,
        )
        .unwrap()
    }

    #[test]
    fn session_assigns_seq_and_orders_history() {
        let mut s = Session::new(card(), Scene::new("书店"));
        s.push(EventKind::UserInput, "陈默", "我想买书");
        s.push(EventKind::Speech, "林夏", "不卖。");
        s.push(EventKind::ToolCall, "dice", "roll");
        assert_eq!(s.seq, 3);
        let perf = s.recent_performance(10);
        // 工具事件不是"表演"，不进历史
        assert_eq!(perf.len(), 2);
        assert_eq!(perf[0].seq, 1);
        let h = s.render_history(10, 500);
        assert!(h.contains("陈默：我想买书"));
        assert!(h.contains("林夏：「不卖。」"));
        assert!(!h.contains("roll"));
    }

    #[test]
    fn history_trims_from_oldest() {
        let mut s = Session::new(card(), Scene::new("书店"));
        for i in 0..40 {
            s.push(EventKind::UserInput, "陈默", format!("第{i}句话，内容还挺长的用来把预算撑满"));
        }
        let h = s.render_history(40, 60);
        assert!(estimate_tokens(&h) <= 60, "got {} tokens", estimate_tokens(&h));
        assert!(h.contains("第39句话"));
        assert!(!h.contains("第0句话"));
    }

    #[test]
    fn settle_advances_turn_and_decays() {
        let mut s = Session::new(card(), Scene::new("书店"));
        s.state.mood.valence = 1.0;
        s.settle(&StateDelta::default(), 0.5);
        assert_eq!(s.state.turn, 1);
        assert!(s.state.mood.valence < 1.0);
    }

    #[test]
    fn scene_changes_are_recorded() {
        let mut s = Session::new(card(), Scene::new("书店"));
        let mut set = std::collections::BTreeMap::new();
        set.insert("time".to_string(), "深夜".to_string());
        set.insert("present".to_string(), "林夏、陈默".to_string());
        let changed = s.apply_scene(&set);
        assert_eq!(changed.len(), 2);
        assert_eq!(s.scene.time, "深夜");
        assert_eq!(s.scene.present, vec!["林夏", "陈默"]);
        assert!(s.transcript.iter().any(|e| e.kind == EventKind::SceneChange));
        // 重复设置同样的值不应再产生事件
        let before = s.transcript.len();
        s.apply_scene(&set);
        assert_eq!(s.transcript.len(), before);
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut s = Session::new(card(), Scene::new("书店"));
        s.push(EventKind::UserInput, "陈默", "在吗");
        let json = s.snapshot_json();
        let back = Session::from_snapshot_json(&json).unwrap();
        assert_eq!(back.seq, s.seq);
        assert_eq!(back.transcript.len(), s.transcript.len());
        assert_eq!(back.card.name, "林夏");
    }

    #[test]
    fn guard_catches_banned_phrase() {
        let r = Reply::parse("[说] 绝绝子，太好吃了吧").unwrap();
        let audit = Guard::audit(&card(), &r);
        assert!(audit.needs_retry());
        assert!(audit.violations[0].contains("绝绝子"));
        let instr = audit.retry_instruction();
        assert!(instr.contains("绝绝子"));
    }

    #[test]
    fn guard_requires_speech() {
        let r = Reply::parse("[做] 她把书合上").unwrap();
        let audit = Guard::audit(&card(), &r);
        assert!(audit.needs_retry());
        assert!(audit.violations[0].contains("缺少"));
    }

    #[test]
    fn a_sticker_reply_is_a_valid_way_to_answer() {
        // 有些时刻一张图就够了。Guard 不该逼角色为表情包配一句台词。
        let r = Reply::parse("[表情] happy_01").unwrap();
        let audit = Guard::audit(&card(), &r);
        assert!(!audit.needs_retry(), "{:?}", audit.violations);
    }

    #[test]
    fn guard_flags_long_speech_as_warning_only() {
        let long = "关".repeat(300);
        let r = Reply::parse(&format!("[说] {long}")).unwrap();
        let audit = Guard::audit(&card(), &r);
        assert!(!audit.needs_retry());
        assert!(audit.warnings.iter().any(|w| w.contains("长于角色示例")));
    }

    #[test]
    fn guard_reminder_contains_boundaries() {
        let audit = Audit::default();
        let rem = Guard::reminder(&card(), &audit).unwrap();
        assert!(rem.contains("禁则"));
        assert!(rem.contains("绝不承认自己害怕孤独"));
        assert!(rem.contains("绝绝子"));
    }

    #[test]
    fn fit_truncates() {
        let s = "a".repeat(1000);
        let cut = fit(&s, 20);
        assert!(cut.starts_with('…'));
        assert!(cut.chars().count() <= 80, "got {}", cut.chars().count());
    }
}
