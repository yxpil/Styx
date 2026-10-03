//! 动态状态：角色**此刻**的心理与关系状态。
//!
//! 设计要点：
//!
//! 1. **所有维度都有界**（`-1..1` 或 `0..1`），模型输出的增量会被 clamp，
//!    防止某一回合的极端输出把角色永久带偏；
//! 2. **每回合衰减**：好感/紧张/精力朝基线回落。这模拟"情绪会平复"，
//!    也让状态不会单调累积到饱和；
//! 3. **意图有寿命**：`agenda` 是短期意图，`decay()` 会逐步淘汰它们。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 心情：一个带标签的二维情绪（效价 / 唤醒度）。
///
/// 用 valence-arousal 而不是离散情绪表，是因为模型很难稳定地在
/// "愤怒/恼怒/不悦"之间做出可靠选择，但很容易判断"更糟还是更好、更激动还是更平静"。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mood {
    /// 人类可读标签："平静"、"戒备"、"雀跃"…
    pub label: String,
    /// 效价 -1.0（极差）~ 1.0（极好）。
    pub valence: f32,
    /// 唤醒度 0.0（昏沉）~ 1.0（亢奋）。
    pub arousal: f32,
}

impl Default for Mood {
    fn default() -> Self {
        Mood {
            label: "平静".into(),
            valence: 0.0,
            arousal: 0.35,
        }
    }
}

impl Mood {
    /// 由效价/唤醒度反推一个默认标签（模型没给标签时用）。
    pub fn label_for(valence: f32, arousal: f32) -> &'static str {
        match (valence, arousal) {
            (v, _) if v > 0.5 => "雀跃",
            (v, a) if v > 0.15 && a > 0.5 => "兴奋",
            (v, _) if v > 0.15 => "愉快",
            (v, a) if v < -0.5 && a > 0.6 => "暴怒",
            (v, _) if v < -0.5 => "低落",
            (v, a) if v < -0.15 && a > 0.5 => "戒备",
            (v, _) if v < -0.15 => "不悦",
            (_, a) if a > 0.7 => "紧绷",
            (_, a) if a < 0.2 => "倦怠",
            _ => "平静",
        }
    }

    fn normalize(&mut self) {
        self.valence = self.valence.clamp(-1.0, 1.0);
        self.arousal = self.arousal.clamp(0.0, 1.0);
        if self.label.trim().is_empty() {
            self.label = Self::label_for(self.valence, self.arousal).to_string();
        }
    }
}

/// 角色的动态状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicState {
    /// 心情。
    pub mood: Mood,
    /// 精力 0..1。
    pub energy: f32,
    /// 紧张度 0..1。
    pub tension: f32,
    /// 对某人的好感 -1..1（键是对方名字）。
    pub affinity: BTreeMap<String, f32>,
    /// 对某人的信任 0..1。
    pub trust: BTreeMap<String, f32>,
    /// 当前短期意图 / 待办（会衰减淘汰）。
    pub agenda: Vec<String>,
    /// 任意状态标记：`"知道秘密X" -> "true"`。
    pub flags: BTreeMap<String, String>,
    /// 已进行的回合数。
    pub turn: u64,
}

impl Default for DynamicState {
    fn default() -> Self {
        DynamicState {
            mood: Mood::default(),
            energy: 0.7,
            tension: 0.2,
            affinity: BTreeMap::new(),
            trust: BTreeMap::new(),
            agenda: Vec::new(),
            flags: BTreeMap::new(),
            turn: 0,
        }
    }
}

impl DynamicState {
    /// 按角色卡的初始关系初始化好感与信任。
    pub fn from_card(card: &crate::character::CharacterCard) -> Self {
        let mut st = DynamicState::default();
        for r in &card.relations {
            st.affinity.insert(r.target.clone(), r.affinity.clamp(-1.0, 1.0));
            // 好感为正 → 初始信任偏高；为负 → 偏低
            let base = 0.5 + r.affinity * 0.4;
            st.trust.insert(r.target.clone(), base.clamp(0.0, 1.0));
        }
        st
    }

    /// 应用一次增量。
    pub fn apply(&mut self, delta: &StateDelta) {
        if let Some(v) = delta.valence {
            self.mood.valence = (self.mood.valence + v).clamp(-1.0, 1.0);
        }
        if let Some(a) = delta.arousal {
            self.mood.arousal = (self.mood.arousal + a).clamp(0.0, 1.0);
        }
        if let Some(e) = delta.energy {
            self.energy = (self.energy + e).clamp(0.0, 1.0);
        }
        if let Some(t) = delta.tension {
            self.tension = (self.tension + t).clamp(0.0, 1.0);
        }
        if let Some(label) = &delta.mood {
            if !label.trim().is_empty() {
                self.mood.label = label.clone();
            }
        }
        for (who, d) in &delta.relations {
            let e = self.affinity.entry(who.clone()).or_insert(0.0);
            *e = (*e + d).clamp(-1.0, 1.0);
            if let Some(td) = delta.trust.get(who) {
                let t = self.trust.entry(who.clone()).or_insert(0.5);
                *t = (*t + td).clamp(0.0, 1.0);
            }
        }
        for (k, v) in &delta.flags {
            self.flags.insert(k.clone(), v.clone());
        }
        for a in &delta.agenda {
            let a = a.trim();
            if a.is_empty() {
                continue;
            }
            if let Some(rest) = a.strip_prefix('-') {
                let rest = rest.trim();
                self.agenda.retain(|x| x != rest);
            } else if !self.agenda.iter().any(|x| x == a) {
                self.agenda.push(a.to_string());
                if self.agenda.len() > 8 {
                    self.agenda.remove(0);
                }
            }
        }
        // 模型给了标签：以显式标签为准；否则按效价/唤醒度反推
        if delta.mood.is_none() && (delta.valence.is_some() || delta.arousal.is_some()) {
            self.mood.label = Mood::label_for(self.mood.valence, self.mood.arousal).to_string();
        }
        self.mood.normalize();
    }

    /// 每回合的回落：情绪平复、精力恢复、紧张消退。
    ///
    /// `rate` 建议 0.06 ~ 0.12：太小则情绪粘住不走，太大则角色没有记忆。
    pub fn decay(&mut self, rate: f32) {
        let rate = rate.clamp(0.0, 1.0);
        self.mood.valence *= 1.0 - rate;
        // 唤醒度与精力朝"中性基线"回落
        self.mood.arousal += (0.35 - self.mood.arousal) * rate;
        self.energy += (0.7 - self.energy) * rate;
        self.tension *= 1.0 - rate;
        self.mood.normalize();
    }

    /// 推进一个回合。
    pub fn tick(&mut self) {
        self.turn += 1;
        self.decay(0.08);
    }

    /// 取对某人的好感（缺省 0）。
    pub fn affinity_to(&self, who: &str) -> f32 {
        self.affinity.get(who).copied().unwrap_or(0.0)
    }

    /// 取对某人的信任（缺省 0.5）。
    pub fn trust_to(&self, who: &str) -> f32 {
        self.trust.get(who).copied().unwrap_or(0.5)
    }

    /// 渲染为提示词里的一段紧凑描述。
    pub fn render(&self) -> String {
        let mut parts = vec![format!(
            "心情：{}（效价{:+.2} 唤醒{:.2}）",
            self.mood.label, self.mood.valence, self.mood.arousal
        )];
        parts.push(format!("精力：{:.2}  紧张：{:.2}", self.energy, self.tension));
        if !self.affinity.is_empty() {
            let rel: Vec<String> = self
                .affinity
                .iter()
                .map(|(k, v)| {
                    let t = self.trust_to(k);
                    format!("{k}(好感{v:+.2}/信任{t:.2})")
                })
                .collect();
            parts.push(format!("关系：{}", rel.join("、")));
        }
        if !self.agenda.is_empty() {
            parts.push(format!("当前意图：{}", self.agenda.join("；")));
        }
        if !self.flags.is_empty() {
            let f: Vec<String> = self.flags.iter().map(|(k, v)| format!("{k}={v}")).collect();
            parts.push(format!("已知状态：{}", f.join("、")));
        }
        parts.push(format!("已进行 {} 个回合", self.turn));
        parts.join("\n")
    }
}

/// 两个可选数值相加：任一侧缺失就取另一侧。
fn add_opt(a: Option<f32>, b: Option<f32>) -> Option<f32> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(x + y),
    }
}

/// 一次状态增量（由模型输出解析而来，或由程序直接构造）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StateDelta {
    /// 心情标签（显式指定）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arousal: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tension: Option<f32>,
    /// 对某人的好感增量。
    #[serde(default)]
    pub relations: BTreeMap<String, f32>,
    /// 对某人的信任增量。
    #[serde(default)]
    pub trust: BTreeMap<String, f32>,
    /// 意图：普通项 = 添加，`-xxx` = 移除。
    #[serde(default)]
    pub agenda: Vec<String>,
    /// 状态标记。
    #[serde(default)]
    pub flags: BTreeMap<String, String>,
}

impl StateDelta {
    /// 是否为空增量。
    pub fn is_empty(&self) -> bool {
        self.mood.is_none()
            && self.valence.is_none()
            && self.arousal.is_none()
            && self.energy.is_none()
            && self.tension.is_none()
            && self.relations.is_empty()
            && self.trust.is_empty()
            && self.agenda.is_empty()
            && self.flags.is_empty()
    }

    /// 把另一份增量叠加进来，得到一份新增量。
    ///
    /// 数值字段**相加**，映射与列表取并集；`mood` 以 `other` 为准
    /// （它是"更晚发生"的那一份，更能代表此刻）。
    ///
    /// 之所以要能在结算前合并，是因为同一回合可能有多个增量来源
    /// （模型的 `[情]` 标签、对方发来的表情包带来的情绪传染……）。
    /// 若分两次 `apply`，每次都会被 `clamp` 到合法区间一次，
    /// 结果会依赖调用顺序——合并后再夹取才是确定的。
    pub fn merged(&self, other: &StateDelta) -> StateDelta {
        let mut out = self.clone();
        if other.mood.is_some() {
            out.mood = other.mood.clone();
        }
        out.valence = add_opt(out.valence, other.valence);
        out.arousal = add_opt(out.arousal, other.arousal);
        out.energy = add_opt(out.energy, other.energy);
        out.tension = add_opt(out.tension, other.tension);
        for (k, v) in &other.relations {
            *out.relations.entry(k.clone()).or_insert(0.0) += *v;
        }
        for (k, v) in &other.trust {
            *out.trust.entry(k.clone()).or_insert(0.0) += *v;
        }
        for a in &other.agenda {
            if !out.agenda.contains(a) {
                out.agenda.push(a.clone());
            }
        }
        for (k, v) in &other.flags {
            out.flags.insert(k.clone(), v.clone());
        }
        out
    }

    /// 渲染为一行摘要（打印在回合报告里）。
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = &self.mood {
            parts.push(format!("心情={m}"));
        }
        if let Some(v) = self.valence {
            parts.push(format!("效价{v:+.2}"));
        }
        if let Some(a) = self.arousal {
            parts.push(format!("唤醒{a:+.2}"));
        }
        if let Some(e) = self.energy {
            parts.push(format!("精力{e:+.2}"));
        }
        if let Some(t) = self.tension {
            parts.push(format!("紧张{t:+.2}"));
        }
        for (k, v) in &self.relations {
            parts.push(format!("{k}好感{v:+.2}"));
        }
        for (k, v) in &self.trust {
            parts.push(format!("{k}信任{v:+.2}"));
        }
        for a in &self.agenda {
            parts.push(format!("意图「{a}」"));
        }
        for (k, v) in &self.flags {
            parts.push(format!("标记{k}={v}"));
        }
        if parts.is_empty() {
            "（无状态变化）".to_string()
        } else {
            parts.join("  ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::character::CharacterCard;

    #[test]
    fn apply_clamps_to_bounds() {
        let mut st = DynamicState::default();
        let d = StateDelta {
            valence: Some(5.0),
            arousal: Some(5.0),
            tension: Some(5.0),
            ..Default::default()
        };
        st.apply(&d);
        assert_eq!(st.mood.valence, 1.0);
        assert_eq!(st.mood.arousal, 1.0);
        assert_eq!(st.tension, 1.0);
        // 效价拉满 + 唤醒拉满 → 雀跃
        assert_eq!(st.mood.label, "雀跃");

        // 极端负效价 + 仍处高唤醒 → 暴怒（不是低落：低落要求唤醒也低）
        let d2 = StateDelta {
            valence: Some(-10.0),
            ..Default::default()
        };
        st.apply(&d2);
        assert_eq!(st.mood.valence, -1.0);
        assert_eq!(st.mood.label, "暴怒");

        // 同样负效价，把唤醒也压到底（唤醒取值域 [0,1]）→ 低落
        let d3 = StateDelta {
            arousal: Some(-10.0),
            ..Default::default()
        };
        st.apply(&d3);
        assert_eq!(st.mood.arousal, 0.0);
        assert_eq!(st.mood.label, "低落");
    }

    #[test]
    fn relations_and_trust_accumulate() {
        let mut st = DynamicState::default();
        let mut d = StateDelta::default();
        d.relations.insert("陈默".into(), 0.3);
        d.trust.insert("陈默".into(), 0.2);
        st.apply(&d);
        assert!((st.affinity_to("陈默") - 0.3).abs() < 1e-6);
        assert!((st.trust_to("陈默") - 0.7).abs() < 1e-6);
        // 未记录的默认值
        assert_eq!(st.affinity_to("无名"), 0.0);
        assert_eq!(st.trust_to("无名"), 0.5);
    }

    #[test]
    fn agenda_add_and_remove() {
        let mut st = DynamicState::default();
        let d = StateDelta {
            agenda: vec!["问清照片来历".into(), "把门锁上".into()],
            ..Default::default()
        };
        st.apply(&d);
        assert_eq!(st.agenda.len(), 2);

        let d2 = StateDelta {
            agenda: vec!["-问清照片来历".into()],
            ..Default::default()
        };
        st.apply(&d2);
        assert_eq!(st.agenda, vec!["把门锁上".to_string()]);
    }

    #[test]
    fn decay_pulls_toward_baseline() {
        let mut st = DynamicState {
            mood: Mood {
                label: "雀跃".into(),
                valence: 1.0,
                arousal: 1.0,
            },
            tension: 1.0,
            energy: 0.0,
            ..Default::default()
        };
        for _ in 0..30 {
            st.decay(0.2);
        }
        assert!(st.mood.valence < 0.1, "valence={}", st.mood.valence);
        assert!(st.tension < 0.1, "tension={}", st.tension);
        assert!((st.energy - 0.7).abs() < 0.1, "energy={}", st.energy);
    }

    #[test]
    fn from_card_seeds_relations() {
        let mut card = CharacterCard::new("林夏");
        card.persona = "x".into();
        card.relations = vec![
            crate::character::Relation::new("陈默", "旧识").with_affinity(-0.3),
            crate::character::Relation::new("母亲", "亲缘").with_affinity(0.9),
        ];
        let st = DynamicState::from_card(&card);
        assert!((st.affinity_to("陈默") + 0.3).abs() < 1e-6);
        assert!(st.trust_to("陈默") < 0.5);
        assert!(st.trust_to("母亲") > 0.5);
    }

    #[test]
    fn empty_delta_detected() {
        assert!(StateDelta::default().is_empty());
        let d = StateDelta {
            valence: Some(0.0),
            ..Default::default()
        };
        assert!(!d.is_empty());
    }

    #[test]
    fn deltas_merge_before_settling() {
        let a = StateDelta {
            valence: Some(-0.2),
            relations: BTreeMap::from([("陈默".to_string(), -0.1)]),
            agenda: vec!["守住店".into()],
            ..Default::default()
        };
        let b = StateDelta {
            mood: Some("戒备".into()),
            valence: Some(0.05),
            arousal: Some(0.3),
            relations: BTreeMap::from([("陈默".to_string(), -0.05)]),
            agenda: vec!["守住店".into(), "问清照片".into()],
            ..Default::default()
        };
        let m = a.merged(&b);
        assert!((m.valence.unwrap() + 0.15).abs() < 1e-6);
        assert_eq!(m.mood.as_deref(), Some("戒备"));
        assert!((m.arousal.unwrap() - 0.3).abs() < 1e-6);
        assert!((m.relations["陈默"] + 0.15).abs() < 1e-6);
        assert_eq!(m.agenda.len(), 2, "重复的意图不该出现两次");
        // 合并必须无副作用：两份原增量都不能被改动
        assert!((a.valence.unwrap() + 0.2).abs() < 1e-6);
        assert!(b.mood.is_some());
    }

    #[test]
    fn render_is_compact_and_informative() {
        let mut st = DynamicState::default();
        st.affinity.insert("陈默".into(), -0.3);
        st.agenda.push("守住店".into());
        let r = st.render();
        assert!(r.contains("心情："));
        assert!(r.contains("陈默(好感-0.30/信任0.50)"));
        assert!(r.contains("守住店"));
    }}
