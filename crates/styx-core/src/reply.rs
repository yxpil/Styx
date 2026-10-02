//! 结构化输出解析：把模型的自由文本变成可编程的回合结果。
//!
//! # 为什么用标签而不是 JSON
//!
//! 角色扮演的输出天然是"多声道"的：台词、动作、内心、情绪变化必须分开，
//! 否则内核无法把动作写进场景、把情绪写进状态机。但强制 JSON 会显著损害
//! 文学质量（模型会在 JSON 里写散文，还会转义失败）。
//!
//! 折中方案：**行首标签 DSL**。模型照常写散文，只是在行首加一个标签，
//! 解析器只认行首标签，其余一律当台词。好处：
//!
//! - 对模型友好，几乎零格式学习成本；
//! - 解析失败不致命——无法识别的行会退化成台词，最坏情况就是"少几个动作描写"；
//! - 仍然支持 JSON 模式（模型直接吐 JSON 对象时自动识别）。
//!
//! # 标签
//!
//! ```text
//! [说] 台词                → speech
//! [做] 动作 / 场面描写       → action
//! [想] 内心独白              → thought
//! [情] 心情=戒备 效价=-0.2 唤醒=+0.1 精力=-0.1 紧张=+0.3
//! [关系] 陈默 好感=-0.1 信任=-0.05
//! [忆] 值得长期记住的事 | 标签=照片,母亲 | 重要度=0.8
//! [场] 时间=深夜 地点=后巷
//! ```
//!
//! `[说]` / `[做]` / `[想]` 可以省略标签，此时按"无标签行 → 台词"处理。

use std::collections::BTreeMap;

use crate::error::{Result, StyxError};
use crate::ports::MemoryNote;
use crate::state::StateDelta;
use crate::text::summarize;

/// 解析后的一个回复。
#[derive(Debug, Clone, Default)]
pub struct Reply {
    /// 台词（一句一段）。
    pub speech: Vec<String>,
    /// 动作 / 场面描写。
    pub actions: Vec<String>,
    /// 内心独白。
    pub thoughts: Vec<String>,
    /// 状态增量。
    pub state_delta: StateDelta,
    /// 场景变更：`字段名 -> 值`（`时间`/`地点`/`环境`/`局面`/`基调`/`在场`）。
    pub scene_set: BTreeMap<String, String>,
    /// 要写入长期记忆的条目。
    pub memories: Vec<MemoryNote>,
    /// 原始输出。
    pub raw: String,
    /// 是否走了 JSON 解析分支。
    pub from_json: bool,
}

impl Reply {
    /// 解析模型输出。
    ///
    /// 顺序：先试 JSON（模型被要求结构化时），失败再走标签 DSL。
    /// 标签 DSL 本身**永不失败**——最坏情况整段变成一条台词。
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(StyxError::EmptyCompletion);
        }
        if let Some(reply) = Self::try_parse_json(trimmed) {
            return Ok(reply);
        }
        Ok(Self::parse_tagged(trimmed))
    }

    /// 把整段文本原样当作台词（模型完全不配合时的兜底）。
    pub fn as_plain_speech(raw: &str) -> Self {
        Reply {
            speech: vec![raw.trim().to_string()],
            raw: raw.to_string(),
            ..Default::default()
        }
    }

    /// 是否什么都没解析出来。
    pub fn is_empty(&self) -> bool {
        self.speech.is_empty() && self.actions.is_empty() && self.thoughts.is_empty()
    }

    /// 全文本（台词按顺序拼接），用于展示。
    pub fn plain_text(&self) -> String {
        let mut out = Vec::new();
        for a in &self.actions {
            out.push(format!("（{a}）"));
        }
        for s in &self.speech {
            out.push(s.clone());
        }
        for t in &self.thoughts {
            out.push(format!("（心想：{t}）"));
        }
        out.join("\n")
    }

    // ---------------------------------------------------------------- JSON

    fn try_parse_json(s: &str) -> Option<Self> {
        // 容忍 ```json 包裹
        let body = if let Some(rest) = s.strip_prefix("```") {
            let rest = rest
                .strip_prefix("json")
                .or_else(|| rest.strip_prefix("JSON"))
                .unwrap_or(rest);
            rest.trim_start()
                .trim_end_matches("```")
                .trim_end()
        } else {
            s
        };
        if !body.starts_with('{') {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let obj = v.as_object()?;

        let mut reply = Reply {
            from_json: true,
            raw: s.to_string(),
            ..Default::default()
        };
        reply.speech = str_list(obj.get("speech").or_else(|| obj.get("say")));
        reply.actions = str_list(obj.get("actions").or_else(|| obj.get("action")));
        reply.thoughts = str_list(obj.get("thoughts").or_else(|| obj.get("thought")));
        if reply.speech.is_empty()
            && reply.actions.is_empty()
            && reply.thoughts.is_empty()
        {
            return None;
        }

        if let Some(st) = obj.get("state").and_then(|x| x.as_object()) {
            let mut d = StateDelta {
                mood: st.get("mood").and_then(|x| x.as_str()).map(String::from),
                valence: num(st.get("valence")),
                arousal: num(st.get("arousal")),
                energy: num(st.get("energy")),
                tension: num(st.get("tension")),
                ..Default::default()
            };
            if let Some(r) = st.get("relations").and_then(|x| x.as_object()) {
                for (k, v) in r {
                    if let Some(n) = num(Some(v)) {
                        d.relations.insert(k.clone(), n);
                    }
                }
            }
            if let Some(r) = st.get("trust").and_then(|x| x.as_object()) {
                for (k, v) in r {
                    if let Some(n) = num(Some(v)) {
                        d.trust.insert(k.clone(), n);
                    }
                }
            }
            if let Some(a) = st.get("agenda") {
                d.agenda = str_list(Some(a));
            }
            reply.state_delta = d;
        }

        if let Some(s) = obj.get("scene").and_then(|x| x.as_object()) {
            for (k, v) in s {
                if let Some(v) = v.as_str() {
                    reply.scene_set.insert(k.clone(), v.to_string());
                }
            }
        }

        if let Some(ms) = obj.get("memories").or_else(|| obj.get("remember")) {
            for m in json_memories(ms) {
                reply.memories.push(m);
            }
        }
        Some(reply)
    }

    // ------------------------------------------------------------ 标签 DSL

    fn parse_tagged(s: &str) -> Self {
        let mut reply = Reply {
            raw: s.to_string(),
            ..Default::default()
        };
        // 当前正在累积的多行块（用于台词/动作/想的续行）
        let mut last: Option<Event> = None;

        for raw_line in s.lines() {
            let line = raw_line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            let (tag, body) = split_tag(line);
            let body = body.trim();
            match tag {
                Tag::Speech => {
                    push_block(&mut reply, Event::Speech, body);
                    last = Some(Event::Speech);
                }
                Tag::Action => {
                    push_block(&mut reply, Event::Action, body);
                    last = Some(Event::Action);
                }
                Tag::Thought => {
                    push_block(&mut reply, Event::Thought, body);
                    last = Some(Event::Thought);
                }
                Tag::State => {
                    last = None;
                    apply_state_line(&mut reply.state_delta, body);
                }
                Tag::Relation => {
                    last = None;
                    apply_relation_line(&mut reply, body);
                }
                Tag::Memory => {
                    last = None;
                    if let Some(note) = parse_memory_line(body) {
                        reply.memories.push(note);
                    }
                }
                Tag::Scene => {
                    last = None;
                    apply_scene_line(&mut reply, body);
                }
                Tag::None => {
                    // 无标签行并入上一条同类型块；否则视为台词续行
                    if body.is_empty() {
                        continue;
                    }
                    match last {
                        Some(Event::Action) => reply.actions.push(body.to_string()),
                        Some(Event::Thought) => reply.thoughts.push(body.to_string()),
                        _ => {
                            reply.speech.push(body.to_string());
                            last = Some(Event::Speech);
                        }
                    }
                }
            }
        }
        reply
    }

    /// 生成给模型看的格式说明（会被塞进系统提示）。
    pub fn format_instructions() -> &'static str {
        r#"请严格使用以下行首标签输出（不要输出 JSON、不要加别的标题）：

[说] 台词内容
[做] 动作或场面描写
[想] 内心独白
[情] 心情=<标签> 效价=<±小数> 唤醒=<±小数> 精力=<±小数> 紧张=<±小数>
[关系] <对方名字> 好感=<±小数> 信任=<±小数>
[忆] <值得长期记住的事实> | 标签=<a,b> | 重要度=<0~1>
[场] 时间=<...> 地点=<...> 环境=<...> 局面=<...> 基调=<...> 在场=<a、b>

规则：
- 至少输出一条 [说]；[做]、[想] 可选但推荐；
- [情] 只写发生变化的维度，没变就不写，增量范围 -0.3 ~ +0.3；
- [忆] 只写真正值得跨场景记住的事（新的约定、身份揭露、承诺、创伤），一般 ≤ 1 条；
- 所有标签行必须独立成行、行首开始。"#
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Speech,
    Action,
    Thought,
}

fn push_block(reply: &mut Reply, kind: Event, body: &str) {
    if body.is_empty() {
        return;
    }
    match kind {
        Event::Speech => reply.speech.push(body.to_string()),
        Event::Action => reply.actions.push(body.to_string()),
        Event::Thought => reply.thoughts.push(body.to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Speech,
    Action,
    Thought,
    State,
    Relation,
    Memory,
    Scene,
    None,
}

/// 切出行首标签。支持 `[说]`、`【说】`、`说：`、`Say:` 等多种写法。
fn split_tag(line: &str) -> (Tag, &str) {
    let s = line.trim_start();

    // 方括号 / 中文方括号
    for (open, close) in [("[", "]"), ("【", "】")] {
        if let Some(rest) = s.strip_prefix(open) {
            if let Some(idx) = rest.find(close) {
                let name = &rest[..idx];
                let body = &rest[idx + close.len()..];
                if let Some(t) = tag_from_name(name) {
                    return (t, body);
                }
            }
        }
    }

    // `说：` / `Say:`
    for (prefix, t) in [
        ("说：", Tag::Speech),
        ("说:", Tag::Speech),
        ("做：", Tag::Action),
        ("做:", Tag::Action),
        ("想：", Tag::Thought),
        ("想:", Tag::Thought),
        ("情：", Tag::State),
        ("关系：", Tag::Relation),
        ("忆：", Tag::Memory),
        ("记：", Tag::Memory),
        ("场：", Tag::Scene),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return (t, rest);
        }
    }
    (Tag::None, s)
}

fn tag_from_name(name: &str) -> Option<Tag> {
    let n = name.trim().to_lowercase();
    Some(match n.as_str() {
        "说" | "台词" | "对白" | "say" | "speech" | "speak" => Tag::Speech,
        "做" | "动作" | "描写" | "旁白" | "act" | "action" | "narrate" => Tag::Action,
        "想" | "内心" | "心声" | "独白" | "think" | "thought" | "inner" => Tag::Thought,
        "情" | "情绪" | "状态" | "心情" | "emotion" | "state" | "mood" => Tag::State,
        "关系" | "好感" | "relation" | "affinity" | "trust" => Tag::Relation,
        "忆" | "记" | "记忆" | "memory" | "remember" | "memo" => Tag::Memory,
        "场" | "场景" | "scene" => Tag::Scene,
        _ => return None,
    })
}

/// 解析 `心情=戒备 效价=-0.2 唤醒=+0.1` 这类键值对。
/// 把一行里的 `k=v` 拆成表。
///
/// 分隔符刻意**不含逗号**：`[忆] 标签=照片,母亲`、`[场] 在场=a、b` 这类
/// 字段的值本身就是逗号/顿号分隔的列表，一旦把逗号当键值对分隔符，
/// `母亲` 就会变成一个没有 `=` 的孤立片段而被丢掉。
fn parse_kv(body: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for token in body.split([' ', '\t', ';', '；']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        for sep in ['=', '：', ':'] {
            if let Some(idx) = token.find(sep) {
                let k = token[..idx].trim();
                let v = token[idx + sep.len_utf8()..]
                    .trim()
                    .trim_matches(['「', '」', '"', '\'', '(', ')']);
                if !k.is_empty() {
                    map.insert(k.to_string(), v.to_string());
                }
                break;
            }
        }
    }
    map
}

fn num(v: Option<&serde_json::Value>) -> Option<f32> {
    let v = v?;
    if let Some(f) = v.as_f64() {
        return Some(f as f32);
    }
    v.as_str().and_then(|s| s.trim().parse::<f32>().ok())
}

/// 解析数值：容忍 `+0.2`、`-0.2`、`0.2`、`20%`。
fn parse_amount(raw: &str) -> Option<f32> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(p) = s.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|v| v / 100.0);
    }
    s.parse::<f32>().ok()
}

fn apply_state_line(delta: &mut StateDelta, body: &str) {
    // `[情] 戒备` 这种只给一个词的写法
    let kv = parse_kv(body);
    if kv.is_empty() {
        let label = body.trim();
        if !label.is_empty() {
            delta.mood = Some(label.to_string());
        }
        return;
    }
    for (k, v) in kv {
        match k.as_str() {
            "心情" | "情绪" | "mood" => delta.mood = Some(v.clone()),
            "效价" | "valence" => delta.valence = parse_amount(&v),
            "唤醒" | "arousal" => delta.arousal = parse_amount(&v),
            "精力" | "energy" => delta.energy = parse_amount(&v),
            "紧张" | "tension" => delta.tension = parse_amount(&v),
            other => {
                // 未识别的键当作心情标签的补充
                if delta.mood.is_none() {
                    delta.mood = Some(format!("{other}={v}"));
                }
            }
        }
    }
}

fn apply_relation_line(reply: &mut Reply, body: &str) {
    let kv = parse_kv(body);
    let who = body
        .split([' ', '，', ',', '\t'])
        .next()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if who.is_empty() {
        return;
    }
    for (k, v) in kv {
        match k.as_str() {
            "好感" | "affinity" => {
                if let Some(n) = parse_amount(&v) {
                    reply.state_delta.relations.insert(who.clone(), n);
                }
            }
            "信任" | "trust" => {
                if let Some(n) = parse_amount(&v) {
                    reply.state_delta.trust.insert(who.clone(), n);
                }
            }
            _ => {}
        }
    }
}

fn apply_scene_line(reply: &mut Reply, body: &str) {
    for (k, v) in parse_kv(body) {
        let key = match k.as_str() {
            "时间" | "time" => "time",
            "地点" | "位置" | "location" => "location",
            "环境" | "天气" | "weather" => "weather",
            "局面" | "situation" => "situation",
            "基调" | "tone" => "tone",
            "在场" | "present" => "present",
            "世界" | "world" => "world",
            other => other,
        };
        reply.scene_set.insert(key.to_string(), v);
    }
}

/// 解析 `[忆] 内容 | 标签=a,b | 重要度=0.8`
fn parse_memory_line(body: &str) -> Option<MemoryNote> {
    let mut parts = body.split('|').map(|s| s.trim());
    let text = parts.next()?.trim();
    if text.is_empty() {
        return None;
    }
    let mut note = MemoryNote::new(summarize(text, 400));
    for part in parts {
        let kv = parse_kv(part);
        if let Some(tags) = kv
            .get("标签")
            .or_else(|| kv.get("tags"))
            .or_else(|| kv.get("tag"))
        {
            note.tags = tags
                .split([',', '，', '、'])
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
        }
        if let Some(imp) = kv
            .get("重要度")
            .or_else(|| kv.get("importance"))
            .or_else(|| kv.get("imp"))
            .and_then(|v| parse_amount(v))
        {
            note.importance = imp.clamp(0.0, 1.0);
        }
    }
    Some(note)
}

fn str_list(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn json_memories(v: &serde_json::Value) -> Vec<MemoryNote> {
    let mut out = Vec::new();
    match v {
        serde_json::Value::Array(a) => {
            for item in a {
                match item {
                    serde_json::Value::String(s) => out.push(MemoryNote::new(s.clone())),
                    serde_json::Value::Object(_) => {
                        let text = item
                            .get("text")
                            .or_else(|| item.get("content"))
                            .and_then(|x| x.as_str())
                            .unwrap_or_default();
                        if text.is_empty() {
                            continue;
                        }
                        let mut note = MemoryNote::new(text);
                        note.tags = str_list(item.get("tags"));
                        note.importance = num(item.get("importance")).unwrap_or(0.5);
                        out.push(note);
                    }
                    _ => {}
                }
            }
        }
        serde_json::Value::String(s) => out.push(MemoryNote::new(s.clone())),
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_tagged_output() {
        let raw = r#"
[做] 她把书合上，指腹在封面上停了一秒。
[说] 不卖。
[说] 那本书，你翻得太用力了。
[想] 这张照片上的女人，和母亲笑起来的样子一模一样。
[情] 心情=戒备 效价=-0.15 唤醒=+0.20 紧张=+0.25
[关系] 陈默 好感=-0.10 信任=-0.05
[忆] 陈默提起了那张旧照片 | 标签=照片,母亲 | 重要度=0.85
[场] 时间=深夜 地点=拾光旧书店 环境=雨
"#;
        let r = Reply::parse(raw).unwrap();
        assert!(!r.from_json);
        assert_eq!(r.actions.len(), 1);
        assert!(r.actions[0].contains("把书合上"));
        assert_eq!(r.speech.len(), 2);
        assert_eq!(r.speech[0], "不卖。");
        assert_eq!(r.thoughts.len(), 1);
        assert_eq!(r.state_delta.mood.as_deref(), Some("戒备"));
        assert!((r.state_delta.valence.unwrap() + 0.15).abs() < 1e-6);
        assert!((r.state_delta.tension.unwrap() - 0.25).abs() < 1e-6);
        assert!((r.state_delta.relations["陈默"] + 0.10).abs() < 1e-6);
        assert!((r.state_delta.trust["陈默"] + 0.05).abs() < 1e-6);
        assert_eq!(r.memories.len(), 1);
        assert_eq!(r.memories[0].tags, vec!["照片", "母亲"]);
        assert!((r.memories[0].importance - 0.85).abs() < 1e-6);
        assert_eq!(r.scene_set["time"], "深夜");
        assert_eq!(r.scene_set["location"], "拾光旧书店");
    }

    #[test]
    fn untagged_output_becomes_speech() {
        let r = Reply::parse("不卖。").unwrap();
        assert_eq!(r.speech, vec!["不卖。".to_string()]);
        assert!(r.actions.is_empty());
        assert!(r.state_delta.is_empty());
    }

    #[test]
    fn continuation_lines_attach_to_previous_block() {
        let raw = "[做] 她抬起头\n雨还在下\n[说] 你来了";
        let r = Reply::parse(raw).unwrap();
        assert_eq!(r.actions.len(), 2);
        assert_eq!(r.actions[1], "雨还在下");
        assert_eq!(r.speech, vec!["你来了".to_string()]);
    }

    #[test]
    fn parses_chinese_brackets_and_colons() {
        let r = Reply::parse("【说】嗯。\n做：她点头\n想: 又是他").unwrap();
        assert_eq!(r.speech, vec!["嗯。".to_string()]);
        assert_eq!(r.actions, vec!["她点头".to_string()]);
        assert_eq!(r.thoughts, vec!["又是他".to_string()]);
    }

    #[test]
    fn parses_json_mode() {
        let raw = r#"{"speech":["不卖。"],"actions":["合上书"],
            "state":{"mood":"戒备","valence":-0.2,"relations":{"陈默":-0.1}},
            "memories":[{"text":"陈默问起照片","tags":["照片"],"importance":0.9}],
            "scene":{"time":"深夜"}}"#;
        let r = Reply::parse(raw).unwrap();
        assert!(r.from_json);
        assert_eq!(r.speech, vec!["不卖。".to_string()]);
        assert_eq!(r.actions, vec!["合上书".to_string()]);
        assert_eq!(r.state_delta.mood.as_deref(), Some("戒备"));
        assert!((r.state_delta.relations["陈默"] + 0.1).abs() < 1e-6);
        assert_eq!(r.memories.len(), 1);
        assert!((r.memories[0].importance - 0.9).abs() < 1e-6);
        assert_eq!(r.scene_set["time"], "深夜");
    }

    #[test]
    fn json_fenced_block_is_tolerated() {
        let raw = "```json\n{\"speech\":[\"好\"]}\n```";
        let r = Reply::parse(raw).unwrap();
        assert!(r.from_json);
        assert_eq!(r.speech, vec!["好".to_string()]);
    }

    #[test]
    fn empty_input_is_an_error() {
        assert!(Reply::parse("   ").is_err());
    }

    #[test]
    fn percent_amounts_supported() {
        let r = Reply::parse("[情] 紧张=20%").unwrap();
        assert!((r.state_delta.tension.unwrap() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn mood_only_word_line() {
        let r = Reply::parse("[情] 戒备").unwrap();
        assert_eq!(r.state_delta.mood.as_deref(), Some("戒备"));
    }

    #[test]
    fn unknown_tag_degrades_to_speech() {
        // 未识别的标签不能让整个回合崩掉，退化成台词是最安全的选择
        let r = Reply::parse("[议] 这是句没被识别的话").unwrap();
        assert_eq!(r.speech.len(), 1);
        assert!(r.speech[0].contains("这是句没被识别的话"));
    }

    #[test]
    fn plain_text_combines_all_channels() {
        let r = Reply::parse("[做] 合上书\n[说] 不卖。\n[想] 别问了").unwrap();
        let t = r.plain_text();
        assert!(t.contains("（合上书）"));
        assert!(t.contains("不卖。"));
        assert!(t.contains("（心想：别问了）"));
    }
}
