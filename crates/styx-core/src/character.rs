//! 角色卡：一个角色**不随时间改变**的那部分。
//!
//! 与 [`crate::state::DynamicState`] 的分工：
//! - 角色卡 = 我是谁（人格、口吻、禁则、背景）
//! - 动态状态 = 我此刻怎么样（心情、好感、紧张、意图）
//!
//! 把两者分开是防"人设漂移"的关键：角色卡每回合**原样**进系统提示，
//! 动态状态则作为可变的"当下"注入，模型不容易把情绪波动误当成性格改变。

use serde::{Deserialize, Serialize};

use crate::error::{Result, StyxError};

/// 与另一个角色的关系。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    /// 对方名字。
    pub target: String,
    /// 关系类型：旧识 / 债主 / 姐弟 / 对手 …
    pub kind: String,
    /// 备注：这段关系里发生过什么。
    #[serde(default)]
    pub note: String,
    /// 初始好感 -1.0 ~ 1.0。
    #[serde(default)]
    pub affinity: f32,
}

impl Relation {
    pub fn new(target: impl Into<String>, kind: impl Into<String>) -> Self {
        Relation {
            target: target.into(),
            kind: kind.into(),
            note: String::new(),
            affinity: 0.0,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    pub fn with_affinity(mut self, affinity: f32) -> Self {
        self.affinity = affinity;
        self
    }
}

/// 角色卡。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CharacterCard {
    /// 稳定标识（用于落库命名空间）。
    #[serde(default)]
    pub id: String,
    /// 正式名。
    pub name: String,
    /// 别名 / 别人怎么称呼。
    #[serde(default)]
    pub aliases: Vec<String>,
    /// 一句话定位，例如"外冷内热的旧书店主"。
    #[serde(default)]
    pub archetype: String,
    /// 人格设定（自由文本）。
    #[serde(default)]
    pub persona: String,
    /// 说话风格：句长、用词、语气、口头禅。
    #[serde(default)]
    pub speech_style: String,
    /// 示例台词，作为 few-shot 锚点，是防漂移最有效的一招。
    #[serde(default)]
    pub speech_samples: Vec<String>,
    /// 性格特质。
    #[serde(default)]
    pub traits: Vec<String>,
    /// 目标 / 动机。
    #[serde(default)]
    pub goals: Vec<String>,
    /// 恐惧 / 弱点。
    #[serde(default)]
    pub fears: Vec<String>,
    /// 禁则：绝不违反的边界（硬约束）。
    #[serde(default)]
    pub boundaries: Vec<String>,
    /// 禁用词 / 禁用句式（风格硬约束）。
    #[serde(default)]
    pub banned_phrases: Vec<String>,
    /// 初始关系。
    #[serde(default)]
    pub relations: Vec<Relation>,
    /// 背景 / 世界观。
    #[serde(default)]
    pub background: String,
    /// 附加的世界书条目：`触发关键词 -> 设定片段`。
    ///
    /// 当输入命中关键词时才注入，用来低成本地扩展世界观而不撑爆预算。
    #[serde(default)]
    pub lore: Vec<LoreEntry>,
    /// 额外任意字段，方便用户自定义而不改代码。
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
}

/// 世界书条目（按关键词触发的设定片段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoreEntry {
    pub keys: Vec<String>,
    pub text: String,
    #[serde(default)]
    pub priority: i32,
}

impl CharacterCard {
    /// 只填名字的最小角色卡。
    pub fn new(name: impl Into<String>) -> Self {
        CharacterCard {
            name: name.into(),
            ..Default::default()
        }
    }

    /// 落库时使用的命名空间（id 为空时退化为名字）。
    pub fn namespace(&self) -> &str {
        if self.id.is_empty() {
            &self.name
        } else {
            &self.id
        }
    }

    /// 校验角色卡是否可用。
    ///
    /// 规则很宽松——角色扮演本来就该允许奇怪的人设——但有几条必须成立：
    /// 名字非空、禁则不能为空（否则一致性无从守护）。
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(StyxError::InvalidCharacter("角色名字不能为空".into()));
        }
        if self.persona.trim().is_empty() && self.archetype.trim().is_empty() {
            return Err(StyxError::InvalidCharacter(
                "至少需要填写 `定位` 或 `人格` 之一，否则模型无从扮演".into(),
            ));
        }
        for (i, r) in self.relations.iter().enumerate() {
            if r.target.trim().is_empty() {
                return Err(StyxError::InvalidCharacter(format!(
                    "第 {} 条关系缺少对方名字",
                    i + 1
                )));
            }
        }
        Ok(())
    }

    /// 从角色卡 Markdown 解析。
    ///
    /// 支持中英文小标题，见 `examples/cards/` 下的示例。
    /// 这是主要的人写入口；程序化构造请直接用结构体或 JSON。
    pub fn parse_markdown(src: &str) -> Result<Self> {
        let mut card = CharacterCard::default();
        let mut section = String::from("__root__");
        let mut buf: Vec<String> = Vec::new();
        let flush = |section: &str, buf: &mut Vec<String>, card: &mut CharacterCard| {
            let lines: Vec<String> = buf
                .drain(..)
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            if lines.is_empty() {
                return;
            }
            let joined = lines.join("\n");
            let items: Vec<String> = lines
                .iter()
                .filter_map(|l| {
                    l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")).map(|s| {
                        s.trim().to_string()
                    })
                })
                .collect();
            match normalize_section(section).as_str() {
                "archetype" => card.archetype = joined,
                "persona" => card.persona = joined,
                "speech_style" => card.speech_style = joined,
                "speech_samples" => card.speech_samples = items_or_lines(&lines, &items),
                "traits" => card.traits = items_or_lines(&lines, &items),
                "goals" => card.goals = items_or_lines(&lines, &items),
                "fears" => card.fears = items_or_lines(&lines, &items),
                "boundaries" => card.boundaries = items_or_lines(&lines, &items),
                "banned_phrases" => card.banned_phrases = items_or_lines(&lines, &items),
                "aliases" => card.aliases = items_or_lines(&lines, &items),
                "background" => card.background = joined,
                "relations" => card.relations = parse_relations(&lines),
                "lore" => card.lore = parse_lore(&lines),
                "id" => card.id = joined,
                _ => {}
            }
        };

        for raw in src.lines() {
            let line = raw.trim_end();
            if let Some(rest) = line.strip_prefix("# ") {
                if card.name.is_empty() {
                    card.name = rest.trim().to_string();
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("## ") {
                flush(&section, &mut buf, &mut card);
                section = rest.trim().to_string();
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            buf.push(line.to_string());
        }
        flush(&section, &mut buf, &mut card);

        if card.id.is_empty() {
            card.id = card.name.clone();
        }
        Ok(card)
    }

    /// 渲染回 Markdown（与 [`Self::parse_markdown`] 往返一致）。
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# {}\n\n", self.name));
        if !self.id.is_empty() && self.id != self.name {
            out.push_str(&format!("## 标识\n\n{}\n\n", self.id));
        }
        let push_text = |out: &mut String, title: &str, v: &str| {
            if !v.trim().is_empty() {
                out.push_str(&format!("## {title}\n\n{v}\n\n"));
            }
        };
        let push_list = |out: &mut String, title: &str, v: &[String]| {
            if !v.is_empty() {
                out.push_str(&format!("## {title}\n\n"));
                for item in v {
                    out.push_str(&format!("- {item}\n"));
                }
                out.push('\n');
            }
        };
        push_text(&mut out, "定位", &self.archetype);
        push_text(&mut out, "人格", &self.persona);
        push_text(&mut out, "口吻", &self.speech_style);
        push_list(&mut out, "示例台词", &self.speech_samples);
        push_list(&mut out, "特质", &self.traits);
        push_list(&mut out, "目标", &self.goals);
        push_list(&mut out, "恐惧", &self.fears);
        push_list(&mut out, "禁则", &self.boundaries);
        push_list(&mut out, "禁用词", &self.banned_phrases);
        push_list(&mut out, "别名", &self.aliases);
        if !self.relations.is_empty() {
            out.push_str("## 关系\n\n");
            for r in &self.relations {
                out.push_str(&format!(
                    "- {} | {} | {} | {:.2}\n",
                    r.target, r.kind, r.note, r.affinity
                ));
            }
            out.push('\n');
        }
        if !self.lore.is_empty() {
            out.push_str("## 世界书\n\n");
            for l in &self.lore {
                out.push_str(&format!(
                    "- {} :: {} :: {}\n",
                    l.keys.join(","),
                    l.text,
                    l.priority
                ));
            }
            out.push('\n');
        }
        push_text(&mut out, "背景", &self.background);
        out
    }

    /// 命中关键词的世界书条目（按优先级降序）。
    pub fn triggered_lore(&self, input: &str) -> Vec<&LoreEntry> {
        let lower = input.to_lowercase();
        let mut hits: Vec<&LoreEntry> = self
            .lore
            .iter()
            .filter(|l| {
                l.keys
                    .iter()
                    .any(|k| !k.is_empty() && lower.contains(&k.to_lowercase()))
            })
            .collect();
        hits.sort_by_key(|l| std::cmp::Reverse(l.priority));
        hits
    }

    /// 从 JSON 解析。
    pub fn from_json(src: &str) -> Result<Self> {
        Ok(serde_json::from_str(src)?)
    }

    /// 序列化为格式化 JSON。
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// 按内容自动识别格式加载（`.json` 走 JSON，否则走 Markdown）。
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let src = std::fs::read_to_string(path)?;
        let is_json = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("json"))
            .unwrap_or(false)
            || src.trim_start().starts_with('{');
        let card = if is_json {
            Self::from_json(&src)?
        } else {
            Self::parse_markdown(&src)?
        };
        card.validate()?;
        Ok(card)
    }
}

fn items_or_lines(lines: &[String], items: &[String]) -> Vec<String> {
    if items.is_empty() {
        lines.to_vec()
    } else {
        items.to_vec()
    }
}

fn normalize_section(s: &str) -> String {
    let s = s.trim().to_lowercase();
    match s.as_str() {
        "定位" | "一句话" | "archetype" | "tagline" => "archetype",
        "人格" | "性格设定" | "persona" | "personality" => "persona",
        "口吻" | "语气" | "说话风格" | "语言风格" | "speech" | "style" | "speech_style" => {
            "speech_style"
        }
        "台词" | "示例台词" | "例句" | "samples" | "speech_samples" | "examples" => {
            "speech_samples"
        }
        "特质" | "性格" | "traits" => "traits",
        "目标" | "动机" | "goals" | "motivation" => "goals",
        "恐惧" | "弱点" | "fears" | "weakness" => "fears",
        "禁则" | "边界" | "底线" | "boundaries" | "rules" => "boundaries",
        "禁用词" | "禁忌词" | "banned" | "banned_phrases" => "banned_phrases",
        "别名" | "称呼" | "aliases" => "aliases",
        "关系" | "人物关系" | "relations" => "relations",
        "世界书" | "lore" => "lore",
        "背景" | "世界观" | "background" => "background",
        "标识" | "id" => "id",
        _ => "",
    }
    .to_string()
}

/// `- 目标 | 关系 | 备注 | 好感度`
fn parse_relations(lines: &[String]) -> Vec<Relation> {
    let mut out = Vec::new();
    for line in lines {
        let body = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line);
        let parts: Vec<&str> = body.split('|').map(|p| p.trim()).collect();
        if parts.is_empty() || parts[0].is_empty() {
            continue;
        }
        out.push(Relation {
            target: parts[0].to_string(),
            kind: parts.get(1).unwrap_or(&"").to_string(),
            note: parts.get(2).unwrap_or(&"").to_string(),
            affinity: parts
                .get(3)
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(0.0),
        });
    }
    out
}

/// `- 关键词1,关键词2 :: 设定片段 :: 优先级`
fn parse_lore(lines: &[String]) -> Vec<LoreEntry> {
    let mut out = Vec::new();
    for line in lines {
        let body = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line);
        let mut parts = body.split("::").map(|p| p.trim());
        let Some(keys) = parts.next() else { continue };
        let Some(text) = parts.next() else { continue };
        let priority = parts.next().and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
        let keys: Vec<String> = keys
            .split([',', '，'])
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        if keys.is_empty() || text.is_empty() {
            continue;
        }
        out.push(LoreEntry {
            keys,
            text: text.to_string(),
            priority,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# 林夏

## 定位

外冷内热的旧书店主

## 人格

话少，但对书极有耐心。习惯先观察再开口。

## 口吻

短句。陈述句为主。不轻易用感叹号。

## 示例台词

- 不卖。
- 那本书，你翻得太用力了。
- 你要找的东西不在这里。

## 特质

- 警惕
- 念旧

## 目标

- 守住这家店

## 禁则

- 绝不承认自己害怕孤独
- 不使用网络流行语

## 关系

- 陈默 | 旧识 | 三年前的债主 | -0.30
- 母亲 | 亲缘 | 已故，留下旧照片 | 0.90

## 世界书

- 旧书店,书店 :: 店里有一只老猫，从不叫。 :: 5

## 背景

城南老街，开了三十年的"拾光"旧书店。
"#;

    #[test]
    fn markdown_roundtrip() {
        let card = CharacterCard::parse_markdown(SAMPLE).unwrap();
        assert_eq!(card.name, "林夏");
        assert_eq!(card.archetype, "外冷内热的旧书店主");
        assert_eq!(card.speech_samples.len(), 3);
        assert_eq!(card.traits, vec!["警惕", "念旧"]);
        assert_eq!(card.boundaries.len(), 2);
        assert_eq!(card.relations.len(), 2);
        assert_eq!(card.relations[0].target, "陈默");
        assert!((card.relations[0].affinity + 0.30).abs() < 1e-6);
        assert_eq!(card.lore.len(), 1);
        assert!(card.validate().is_ok());

        // 往返：渲染后再解析，关键字段一致
        let again = CharacterCard::parse_markdown(&card.to_markdown()).unwrap();
        assert_eq!(again.name, card.name);
        assert_eq!(again.traits, card.traits);
        assert_eq!(again.boundaries, card.boundaries);
        assert_eq!(again.relations.len(), card.relations.len());
        assert_eq!(again.speech_samples.len(), card.speech_samples.len());
    }

    #[test]
    fn json_roundtrip() {
        let card = CharacterCard::parse_markdown(SAMPLE).unwrap();
        let back = CharacterCard::from_json(&card.to_json()).unwrap();
        assert_eq!(back.name, card.name);
        assert_eq!(back.goals, card.goals);
    }

    #[test]
    fn validate_rejects_empty_persona() {
        let card = CharacterCard::new("张三");
        assert!(card.validate().is_err());

        let mut card = CharacterCard::new("  ");
        card.persona = "x".into();
        assert!(card.validate().is_err());
    }

    #[test]
    fn lore_triggers_on_keywords() {
        let card = CharacterCard::parse_markdown(SAMPLE).unwrap();
        assert_eq!(card.triggered_lore("我们去旧书店看看吧").len(), 1);
        assert_eq!(card.triggered_lore("今天天气不错").len(), 0);
    }
}
