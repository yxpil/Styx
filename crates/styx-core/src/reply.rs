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
//! ── 表演通道 ───────────────────────────────────────────────
//! [说] 台词                → speech
//! [做] 动作 / 场面描写       → action
//! [想] 内心独白              → thought
//! ── 状态通道 ───────────────────────────────────────────────
//! [情] 心情=戒备 效价=-0.2 唤醒=+0.1 精力=-0.1 紧张=+0.3
//! [关系] 陈默 好感=-0.1 信任=-0.05
//! [忆] 值得长期记住的事 | 标签=照片,母亲 | 重要度=0.8
//! [场] 时间=深夜 地点=后巷
//! ── 素材通道 ───────────────────────────────────────────────
//! [表情] happy_01          ← 发一张表情包（编号来自角色可用的表情包目录）
//! [图片] old_photo ｜ 你看这个   ← 给对方看一张图片 / 照片
//! ── 控制通道（两阶段生成的入口）────────────────────────────
//! [回忆] 母亲留下的那张照片   ← 申请：从长期记忆里取回相关片段
//! [联想] 旧书店             ← 申请：让联想后端发散
//! [查阅] 拾光旧书店          ← 申请：查角色卡世界书
//! ```
//!
//! `[说]` / `[做]` / `[想]` 可以省略标签，此时按"无标签行 → 台词"处理。
//!
//! ## 解析器不知道任何后端
//!
//! `[表情]`、`[图片]` 只负责**把编号原样记下来**，不校验编号是否存在；
//! `[回忆]` / `[联想]` / `[查阅]` 只负责**把查询词记下来**，不去真的查。
//! 合法性由内核在拿到 [`StickerCatalog`](crate::sticker::StickerCatalog) /
//! [`MediaLibrary`](crate::media::MediaLibrary) / 记忆端口时统一裁决。
//!
//! 这条边界是刻意的：解析器一旦能够访问后端，它就再也没法脱离集群单测了。

use std::collections::BTreeMap;

use crate::error::{Result, StyxError};
use crate::ports::MemoryNote;
use crate::protocol::{Directive, DirectiveKind, ImageRequest};
use crate::state::StateDelta;
use crate::text::{strip_one_quote_layer, strip_quote_wrappers, summarize};

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
    /// 要发出的表情包编号（按出现顺序，已去重）。
    pub stickers: Vec<String>,
    /// 要发出的图片 / 照片（按出现顺序）。
    pub images: Vec<ImageRequest>,
    /// 控制指令：请内核去取资料（取到之后要再生成一次）。
    pub directives: Vec<Directive>,
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
    ///
    /// 注意**不算 `directives`**：一次只发了"我要查资料"的输出不是对用户的
    /// 回答，它是回合内部的中间态。若把指令算成内容，模型就会靠反复发指令
    /// 绕过一致性守护。
    pub fn is_empty(&self) -> bool {
        self.speech.is_empty()
            && self.actions.is_empty()
            && self.thoughts.is_empty()
            && self.stickers.is_empty()
            && self.images.is_empty()
    }

    /// 这个回复是否在申请资料（即需要两阶段生成）。
    pub fn has_directives(&self) -> bool {
        !self.directives.is_empty()
    }

    /// 这个回复有没有"给人看的东西"。
    pub fn has_performance(&self) -> bool {
        !self.speech.is_empty()
            || !self.actions.is_empty()
            || !self.thoughts.is_empty()
            || !self.stickers.is_empty()
            || !self.images.is_empty()
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
        if !self.stickers.is_empty() {
            // 纯文本通道里没有图，用一句可读的标注代替，至少别丢信息
            out.push(format!("［表情：{}］", self.stickers.join("、")));
        }
        for img in &self.images {
            if img.caption.is_empty() {
                out.push(format!("［图片：{}］", img.key));
            } else {
                out.push(format!("［图片：{} · {}］", img.key, img.caption));
            }
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
            rest.trim_start().trim_end_matches("```").trim_end()
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
        reply.stickers = str_list(obj.get("stickers").or_else(|| obj.get("sticker")))
            .into_iter()
            .filter_map(|s| parse_sticker_id(&s))
            .collect();
        reply.images = json_images(obj.get("images").or_else(|| obj.get("photos")));
        reply.directives = json_directives(obj);
        if !reply.has_performance() && !reply.has_directives() {
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
        // 有些模型把标签写在**单独一行**、内容写在下一行：
        //
        //     [忆]
        //     相册最上面那一格夹着一张旧照片 | 标签=a | 重要度=0.9
        //
        // "小标题 + 正文"是极自然的写法（实测 phi4 每隔几轮就这么写）。
        // 不处理的话，结构化数据会**既漏进台词、又永远落不了库**——
        // 而这两个症状都不指向"标签换到了下一行"这个真因，很难查。
        let mut pending: Option<Tag> = None;

        for raw_line in s.lines() {
            let line = raw_line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            // 模型偶尔把**格式说明本身**抄进回复。实机证据（phi4，本机 Ollama）：
            // 某一轮它把 `规则：` 连同后面四条列表项一字不差地复述了出来，
            // 10 行说明全部落进台词、直接显示给用户——用户看到的是规则文本。
            //
            // 这里能精确拦住的底气在于：说明文本由 [`Reply::format_instructions`]
            // 生成，解析器**逐行知道**它长什么样，不需要猜。改说明时指纹自动跟着变，
            // 不会像硬编码名单那样悄悄腐烂。
            if is_instruction_echo(line) {
                // 说明行不是任何挂起标签的正文；不清的话，`[忆] <...>` 这种
                // 占位行会把 pending 一直挂着，等下一句真台词来"认领"。
                pending = None;
                continue;
            }
            let (mut tag, body) = split_tag(line);
            let body = body.trim();

            // 上一行留了个空标签，这一行就是它的正文。
            // 只在下一行**自己不带标签**、且正文形状对得上时才认领——
            // 否则会把别人的内容吞掉。
            if let Some(held) = pending.take() {
                if tag == Tag::None && pending_accepts(held, body) {
                    tag = held;
                }
            }

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
                Tag::State
                | Tag::Relation
                | Tag::Memory
                | Tag::Scene
                | Tag::Sticker
                | Tag::Image
                | Tag::Recall
                | Tag::Associate
                | Tag::Lore => {
                    // 旁路通道一律**不参与续行**：紧跟其后的无标签行若不
                    // 当台词处理，就会静默消失——那是最难查的一类 bug。
                    //
                    // 实现走 `apply_side_channel` 而不是就地展开，是因为
                    // "说话人 + 引号"壳里也可能装着一个真标签
                    // （`林夏：「[表情] 无语」`），那条路径必须用同一套逻辑。
                    // 两处各写一份，必然会漂移成"同一个标签时灵时不灵"。
                    last = None;
                    apply_side_channel(&mut reply, tag, body);
                }
                Tag::Header => {
                    // 分节标题是排版，不是内容，也不是续行：
                    // 必须把 last 清掉，否则标题后面的第一句话会被挂到
                    // 标题之前的那个动作/内心里去（那是最难查的一类错位）。
                    last = None;
                }
                Tag::None => {
                    if body.is_empty() {
                        continue;
                    }
                    // 先试剧本排版：模型没写标签、但模仿了它在历史里看到的写法。
                    // 必须放在"并入上一块"之前，否则动作/内心会被并进台词。
                    //
                    // 可能一次拿到多条：一对括号里可以同时装着动作和一个旁路标签
                    // （`（…挡在身前。\n[表情] 心意_11）`），所以这里是循环。
                    if let Some(parts) = screenplay_form(body) {
                        for (t, text) in parts {
                            match t {
                                Tag::Action => {
                                    reply.actions.push(text);
                                    last = Some(Event::Action);
                                }
                                Tag::Thought => {
                                    reply.thoughts.push(text);
                                    last = Some(Event::Thought);
                                }
                                Tag::Speech => {
                                    // 统一走 push_block：台词的"剥引号壳"只在那一个
                                    // 地方做，这里再手工 push 就会漏掉它。
                                    push_block(&mut reply, Event::Speech, &text);
                                    last = Some(Event::Speech);
                                }
                                // 壳里装的是旁路通道（表情 / 图片 / 记忆 / 状态 /
                                // 场景 / 控制指令）：走和行首标签**同一条**路径。
                                other => {
                                    last = None;
                                    apply_side_channel(&mut reply, other, &text);
                                }
                            }
                        }
                        continue;
                    }
                    // 模型有时把 `[情]` 的**标签写丢了**，只剩正文：
                    //
                    //     林夏暗想：别提这样的话题。
                    //     心情=平静 效价=-0.10 唤醒=-0.05 精力=0.78 紧张=0.17
                    //
                    // 第二行没有标签，于是被"无标签行并入上一块"并进了上一条
                    // 内心——用户在独白末尾看到一串机器参数（实测 5 轮里出现 2 次）。
                    // 停下来先看它像不像状态行，是唯一能在"并入上一块"之前
                    // 拦住它的时机。
                    if looks_like_state_line(body) {
                        apply_state_line(&mut reply.state_delta, body);
                        last = None;
                        continue;
                    }
                    // 无标签行并入上一条同类型块；否则视为台词续行
                    match last {
                        Some(Event::Action) => reply.actions.push(body.to_string()),
                        Some(Event::Thought) => reply.thoughts.push(body.to_string()),
                        _ => {
                            push_block(&mut reply, Event::Speech, body);
                            last = Some(Event::Speech);
                        }
                    }
                }
            }

            // 结构化标签后面空着：把标签挂起来，等下一行的正文。
            // 表演通道（说/做/想）不在此列——它本来就走"并入上一块"的
            // 续行逻辑，再挂一个 pending 只会两套机制互相打架。
            if body.is_empty() && holds_content_next_line(tag) {
                pending = Some(tag);
            }
        }
        reply
    }

    /// 生成给模型看的格式说明（会被塞进系统提示）。
    ///
    /// 这里是**文本约定协议**对模型暴露的全部内容。写得啰嗦一点是有意的：
    /// 这段话是模型唯一能看到的"接口文档"，而一旦模型学会了，整个内核的
    /// 记忆、联想、素材能力就都对它打开了——比教它 function calling 便宜得多。
    ///
    /// 注意：**素材通道（`[表情]` / `[图片]`）不在这里**。
    /// 它们由 [`crate::prompt::PromptBuilder`] 在确认目录非空之后追加
    /// （见 `StickerCatalog::format_hint` / `MediaLibrary::format_hint`）。
    /// 没有素材却告诉模型"你可以发图"，它一定会开始编造编号。
    pub fn format_instructions() -> &'static str {
        r#"下面是你每次回复的**格式说明**——按它写，但不要把说明本身抄进回复。

【表演】
[说] 台词内容
[做] 动作或场面描写
[想] 内心独白

【状态】
[情] 心情=<标签> 效价=<±小数> 唤醒=<±小数> 精力=<±小数> 紧张=<±小数>
[关系] <对方名字> 好感=<±小数> 信任=<±小数>
[忆] <值得长期记住的事实> | 标签=<a,b> | 重要度=<0~1>
[场] 时间=<...> 地点=<...> 环境=<...> 局面=<...> 基调=<...> 在场=<a、b>

写的时候，至少给一条 [说]，[做] 和 [想] 可选但推荐。[情] 只写确实发生变化的
维度，没变就整行不写，增量在 -0.3 到 +0.3 之间。[忆] 只写真正值得跨场景记住的
事（新的约定、身份揭露、承诺、创伤），一般不超过 1 条。所有标签行都要独立成行、
从行首开始写。

确实想不起某件事的时候，改用 [回忆] / [联想] / [查阅] 这三个申请标签，让系统把
资料交给你。这一次输出里只写这些申请标签，不要同时写台词；系统会把查到的资料交给
你，然后请你重新正式表演。申请要具体（`[回忆] 母亲留下的那张照片` 远好于
`[回忆] 事情`）；能凭现有信息回答时就别查，平时完全不写这三个标签。

只使用上面列出的标签，不要自己发明新的。想写动作、神态、注视、沉默这类描写，
一律用 [做]。"#
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Speech,
    Action,
    Thought,
}

/// 把一条**旁路通道**的标签应用到回复上。
///
/// 旁路 = 不是"表演"的那几路：状态 / 关系 / 记忆 / 场景 / 表情 / 图片 /
/// 控制指令。它们都由内核在拿到回复后统一裁决，解析器只负责记下来。
///
/// 抽成函数是因为有两条入口：行首标签，以及"说话人 + 引号"壳里藏着的标签
/// （`林夏：「[表情] 无语」`）。两处各写一份必然漂移。
fn apply_side_channel(reply: &mut Reply, tag: Tag, body: &str) {
    match tag {
        Tag::State => apply_state_line(&mut reply.state_delta, body),
        Tag::Relation => apply_relation_line(reply, body),
        Tag::Memory => {
            if let Some(note) = parse_memory_line(body) {
                reply.memories.push(note);
            }
        }
        Tag::Scene => apply_scene_line(reply, body),
        Tag::Sticker => {
            if let Some(id) = parse_sticker_id(body) {
                if !reply.stickers.contains(&id) {
                    reply.stickers.push(id);
                }
            }
        }
        Tag::Image => {
            if let Some(req) = parse_image_request(body) {
                reply.images.push(req);
            }
        }
        Tag::Recall | Tag::Associate | Tag::Lore => {
            if let Some(d) = parse_directive(tag, body) {
                reply.directives.push(d);
            }
        }
        // 表演通道与标题不走这里；真被传进来也不必报错——静默忽略即可，
        // 解析器对模型的怪输出一向是"尽量不失败"。
        _ => {}
    }
}

/// 哪些标签的正文**允许写在下一行**。
///
/// 只对"结构化"标签开放：它们的正文有明确形状（`key=value`、`编号`），
/// 下一行是不是它的正文比较好判断。
fn holds_content_next_line(tag: Tag) -> bool {
    matches!(
        tag,
        Tag::State | Tag::Relation | Tag::Memory | Tag::Scene | Tag::Sticker | Tag::Image
    )
}

/// 挂起的标签能不能认领下一行。
///
/// 这是安全阀。`[情]` 后面如果跟的其实是一句台词，直接当成状态正文
/// 会把那句话静默吃掉——比"没识别出状态"更难发现。所以要求正文符合
/// 那个标签的形状：状态/关系/场景必须是 `key=value`，素材必须是单 token。
///
/// `[忆]` 是唯一的例外：它的正文本来就是一句自由文本，没法形状校验。
/// 这是有意接受的权衡——实测里模型写的就是
/// `【忆】` 换行接记忆正文，不认领的代价是记忆**直接丢失**。
fn pending_accepts(tag: Tag, body: &str) -> bool {
    if body.is_empty() {
        return false;
    }
    match tag {
        Tag::State | Tag::Relation | Tag::Scene => body.contains('='),
        Tag::Sticker => !body.contains(char::is_whitespace),
        Tag::Image => !body.contains(char::is_whitespace) || body.contains(['|', '｜']),
        Tag::Memory => true,
        _ => false,
    }
}

fn push_block(reply: &mut Reply, kind: Event, body: &str) {
    if body.is_empty() {
        return;
    }
    match kind {
        // 台词剥掉模型自带的引号壳，让 `speech` 里存的是**干净文本**。
        //
        // 为什么在解析层剥而不只在渲染层剥：`reply.speech` 是走线的数据，
        // web 前端直接把它显示出来。同一句台词若只在渲染层剥，就会变成
        // "命令行干净、网页多一层引号"——同一条数据在两个前端长得不一样，
        // 是最难归因的一类不一致。
        Event::Speech => reply.speech.push(strip_quote_wrappers(body).to_string()),
        Event::Action => reply.actions.push(body.to_string()),
        Event::Thought => reply.thoughts.push(body.to_string()),
    }
}

/// 无标签行看起来是不是"忘了写 `[情]` 的状态增量"？
///
/// 判据只用**项目专有的维度名**（`效价` / `唤醒`）：它们是心理学术语，
/// 正常台词里不会出现，所以误伤面几乎为零。比"看它像不像 key=value"
/// 这种形状判断稳得多——`他说：一加一等于二，x=y` 这类台词不该被吃掉。
fn looks_like_state_line(line: &str) -> bool {
    line.contains("效价=") || line.contains("唤醒=")
}

/// 这一行是不是**格式说明的原文**？
///
/// 模型偶尔会把说明整段抄进回复。实测 phi4（本机 Ollama）某一轮把 `规则：`
/// 连同后面四条列表项一字不差地复述了出来，10 行说明全部落进台词——用户
/// 看到的就是规则文本。这类污染很难在前端归因：结构化字段只说明"它在台词里"，
/// 而看起来又确实像台词。
///
/// 只认**逐字相同**（忽略首尾空白），不用前缀或包含匹配：说明里有
/// `[说] 台词内容` 这类占位行，一旦放宽成"包含"，`[说] 台词内容真不错。`
/// 这种真台词也会被吃掉。**误删真内容比漏拦几行说明更难发现**，
/// 所以这里宁可判得紧一点。
fn is_instruction_echo(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && Reply::format_instructions().lines().any(|l| l.trim() == t)
}

/// 心理动词。**只能有这一份名单**——它被两处用到：
///
/// - [`screenplay_form`]：判定 `X暗想：独白`（不带括号的写法）
/// - [`parenthesised`]：切出 `（X心想：独白）` 里冒号之后的正文
///
/// 拆成两份的代价已经实机付过一次：`parenthesised` 原来的名单只有
/// `心想：` / `内心：` / `独白：`，模型某轮全改成写 `心里想：`，
/// 一个词对不上，整段独白就落进动作（回合统计 `thought 0 / action 12`）。
/// 两份名单看起来"差不多"，但差的那一个词就是整条通道的存亡。
const MIND_VERBS: &[&str] = &[
    "暗想",
    "心想",
    "心里想",
    "默默想",
    "独白",
    "心道",
    "暗道",
    "内心",
    "想",
];

/// 在 `s` 里找"心理动词 + 冒号"**最早**的切点，返回冒号之后的位置。
///
/// 取最早而不是最晚：切出来的是"说话人 + 动词 + 正文"，万一正文里又出现
/// `心想`，不会被再切一次（那样会把独白腰斩）。
fn mind_marker_end(s: &str) -> Option<usize> {
    // `联想` 是本项目的**通道名**（正确写法 `[联想] 种子词`），漏写方括号时
    // 就是 `联想：xxx`。它恰好以"想"结尾，不排掉就会被当成独白——把一条
    // 控制指令的残骸塞进内心通道，比单纯漏识别更难查。
    let probe = s.trim_start();
    if probe.starts_with("联想：") || probe.starts_with("联想:") {
        return None;
    }
    let mut cut: Option<usize> = None;
    for verb in MIND_VERBS {
        for colon in ['：', ':'] {
            let pat = format!("{verb}{colon}");
            if let Some(idx) = s.find(&pat) {
                let end = idx + pat.len();
                cut = Some(match cut {
                    Some(prev) => prev.min(end),
                    None => end,
                });
            }
        }
    }
    cut
}

/// 识别**剧本排版**：模型没写标签，但用了 Styx 渲染给它的那种写法。
///
/// 这不是纵容模型偷懒，而是闭合一个环：[`Event::render_line`] 会把历史
/// 渲染成 `林夏：「不卖。」` / `（把书合上）` / `（林夏心想：又是他。）`
/// 再喂回提示词。模型照着这个格式写是完全自然的——而如果解析器不认，
/// 它就会因为"模仿自己看到过的转录"而受罚：整段退化成台词，
/// 动作与内心双双丢失。实测 phi4 每隔几轮就会这样写一整段。
///
/// 五种形态与渲染器一一对应：
///
/// | 写法 | 解析成 |
/// |---|---|
/// | `X：「台词」` | 台词 |
/// | `（动作）` | 动作 |
/// | `（X心想：独白）` | 内心 |
/// | `X：（动作）` | 动作 |
/// | `X暗想：独白` | 内心 |
///
/// 第四种是模型把两种记法混着用（实测 phi4 会写
/// `林夏：（轻轻转动柜台上的玻璃杯…）`），光认整行括号会漏掉这一半。
///
/// 第五种**不带括号**，是最隐蔽的一种：`林夏暗想：她的笑法让我觉得不太习惯。`
/// 长得像 `X：「台词」`，区别只在冒号后不是引号而是自由文本。不认它就会落到
/// "无标签行并入上一块"，被并进它前面那条动作里——回合统计里 thought 只剩
/// 1 条而 action 有 10 条，**整段内心独白静默变成动作描写**。
///
/// 只认**整行**符合形态的行。半途出现的括号（比如一句台词里夹着
/// `（笑）`）仍然走原来的"并入上一块"逻辑，不会被误切。
///
/// 返回 `Vec` 而不是单条：一对括号里可能**同时**装着动作和一个旁路标签
/// （见 [`parenthesised`]），这时要拆成两条分别投递。
fn screenplay_form(line: &str) -> Option<Vec<(Tag, String)>> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }

    // ── `X：「台词」` ──────────────────────────────────────────
    // 说话人标签必须短且不含空白，否则 `时间：傍晚` 这类正常的
    // "字段：值"行会被误判成台词。
    //
    // 还要求**整行真的被引号包住**：`陈默：「我不去」——他这么说过。`
    // 是"引语 + 补述"，拦腰截断会丢掉后半句，所以它保持原样进台词。
    if let Some((head, rest)) = t.split_once(['：', ':']) {
        let head = head.trim();
        let head_ok =
            !head.is_empty() && !head.contains(char::is_whitespace) && head.chars().count() <= 8;
        if head_ok {
            if let Some(unwrapped) = strip_one_quote_layer(rest) {
                let inner = strip_quote_wrappers(unwrapped);
                if !inner.is_empty() {
                    // `林夏：「[表情] 无语」`——说话人和引号只是壳，壳里装的是
                    // 一个**真标签**。不往里看的话，素材/状态/记忆这些通道
                    // 会全部塌成台词（实测 phi4 就这么写，表情包一次都发不出去，
                    // 而它明明写了编号）。
                    let (inner_tag, inner_body) = split_tag(inner);
                    if inner_tag != Tag::None {
                        return Some(vec![(inner_tag, inner_body.trim().to_string())]);
                    }
                    return Some(vec![(Tag::Speech, inner.to_string())]);
                }
            }
            // `林夏：（轻轻转动柜台上的玻璃杯…）` —— 说话人前缀 + 括号动作。
            // 实测 phi4 会混着用两种记法，光认整行括号会漏掉这一半。
            if let Some(hit) = parenthesised(rest) {
                return Some(hit);
            }
            // `林夏暗想：她的笑法让我觉得不太习惯。` —— **不带括号的内心**。
            //
            // 判定靠"说话人尾部是不是心理动词"，而不是靠冒号后的内容：
            // 内容是完全自由的散文，没有可校验的形状；而心理动词是一个
            // 封闭小集合（[`MIND_VERBS`]），误判面可控。必须是 `ends_with`
            // （`林夏暗想` 的关键信息在词尾），用 contains 会把
            // `我想起一件事：…` 这种叙述也拉进来。
            let rest_t = rest.trim();
            // `head` 以 `（` 开头要让路：整行被括号包住时（`（林夏心想：…）`），
            // `split_once` 一样会切出一个 head，但那种写法的正文在括号**内**、
            // 尾部还挂着右括号，只有 `parenthesised(t)` 切得对。在这里抢先返回，
            // 独白就会拖着 `）` 走——那是实机测试里真报出来过的一种脏数据。
            // （`联想` 的排除在 [`mind_marker_end`] 里统一做了。）
            let is_mind = !head.starts_with(['（', '('])
                && MIND_VERBS.iter().any(|verb| head.ends_with(verb));
            if is_mind
                && !rest_t.is_empty()
                && !matches!(rest_t.chars().next(), Some('「' | '『' | '“'))
            {
                return Some(vec![(Tag::Thought, rest_t.to_string())]);
            }
        }
    }

    // ── 整行被圆括号包住：`（动作）` / `（X心想：独白）` ────────
    parenthesised(t)
}

/// 整段被圆括号包住时，切成动作还是内心。
///
/// `（林夏心想：…）` 是内心，`（她抬起头。）` 是动作。
///
/// 返回 `Vec` 而不是单条，因为括号里可能**同时**装着动作和一个旁路标签：
///
/// ```text
/// （林夏注视着陈默，…让一张书板挡在身前。
/// [表情] 心意_11）
/// ```
///
/// 这是实测形状（phi4，本机 Ollama）。只切出单条时，`[表情] 心意_11`
/// 会作为普通文本留在动作里——用户看到带方括号的机器痕迹，而素材通道
/// 一次都触发不了，尽管模型明明写了编号。所以这里先按行扫一遍，
/// 把带标签的行摘出去，剩下的连续散文再按"动作 / 内心"判定。
fn parenthesised(s: &str) -> Option<Vec<(Tag, String)>> {
    let t = s.trim();
    for (open, close) in [('（', '）'), ('(', ')')] {
        if let Some(inner) = t.strip_prefix(open).and_then(|r| r.strip_suffix(close)) {
            let inner = inner.trim();
            if inner.is_empty() {
                return None;
            }
            let mut out: Vec<(Tag, String)> = Vec::new();
            // 尚未定性的连续散文（可能跨多行，最后整体判成动作或内心）
            let mut prose: Vec<&str> = Vec::new();
            for line in inner.lines() {
                let (tag, body) = split_tag(line);
                let body = body.trim();
                if tag != Tag::None && !body.is_empty() {
                    // 遇到标签行：先把前面攒的散文结掉，别让它们黏在一起
                    flush_prose(&mut out, &mut prose);
                    out.push((tag, body.to_string()));
                } else if !line.trim().is_empty() {
                    prose.push(line);
                }
            }
            flush_prose(&mut out, &mut prose);
            if out.is_empty() {
                return None;
            }
            return Some(out);
        }
    }
    None
}

/// 把攒下的散文块判成动作或内心，追加到输出里。
///
/// 判定走 [`mind_marker_end`]——和 `screenplay_form` 用的是同一份心理动词
/// 名单。这里原先另有一份自己的短名单（只有 `心想：` / `内心：` / `独白：`），
/// 模型改写成 `心里想：` 之后匹配不上，整段独白就被当成动作描写
/// （实机：`thought 0 / action 12`）。
fn flush_prose(out: &mut Vec<(Tag, String)>, prose: &mut Vec<&str>) {
    if prose.is_empty() {
        return;
    }
    let text = prose.join("\n").trim().to_string();
    prose.clear();
    if text.is_empty() {
        return;
    }
    match mind_marker_end(&text) {
        Some(start) => {
            let body = text[start..].trim().to_string();
            if body.is_empty() {
                out.push((Tag::Action, text));
            } else {
                out.push((Tag::Thought, body));
            }
        }
        None => out.push((Tag::Action, text)),
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
    Sticker,
    Image,
    Recall,
    Associate,
    Lore,
    /// 分节标题（`【表演】` / `【状态】`）。
    ///
    /// 这类行不是内容，是模型**回显了格式说明里的节名**。它必须被吞掉：
    /// 当成台词会念给用户听，当成续行会把下一句话挂到上一块上去。
    Header,
    None,
}

/// 是否是格式说明里用过的**分节标题**名。
///
/// 这份名单之所以必须存在：[`Reply::format_instructions`] 里明明白白写着
/// `【表演】`、`【状态】` 这些节名，模型照着回显是天经地义的事。
/// 但 `【表演】` 不在 [`tag_from_name`] 的标签表里（它不是标签，是节名），
/// 于是早期的解析器把它当成了"无标签行"，原样念给用户听。
///
/// 实机验证：phi4 每一回合都回显节名——不是偶发，是每次必现。
///
/// **这份名单和格式说明必须同步**。漏登记一个的代价是实机跑出脏台词，
/// 而这种错很难一眼归因。所以下面有一条
/// `every_section_header_in_the_instructions_is_recognised` 专门盯这件事：
/// 往说明里加了新标题却忘了登记，它会立刻失败。
///
/// 注：`想不起事情时，先申请查资料` 曾经也是说明里的节名。实机发现两件事：
/// 一是模型会把它当台词念出来（就是这条名单要解决的问题）；二是它读起来像
/// "必须填的一节"，模型会回一句 `无需再查资料` 当台词（E2E 实测）。
/// 所以说明里已改成不带括号的散文引导句——**减少独立标记行**本身就是
/// 一种防御，因为模型只会回显标记，不会回显散文。这里保留登记，
/// 是为了兼容仍在用旧说明的场景。
const SECTION_NAMES: &[&str] = &[
    // 说明里出现的
    "表演",
    "状态",
    // 旧说明里的（兼容保留）
    "想不起事情时，先申请查资料",
    // 同义 / 英文写法：模型会自行改写节名，漏一个就漏一类
    "演出",
    "正文",
    "格式",
    "格式说明",
    "输出",
    "performance",
    "state",
    "format",
    "output",
];

/// 比较时忽略空白：模型不一定把节名一字不差地抄下来。
fn is_section_name(name: &str) -> bool {
    fn squeeze(s: &str) -> String {
        s.trim()
            .to_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }
    let n = squeeze(name);
    SECTION_NAMES.iter().any(|s| squeeze(s) == n)
}

/// 切出行首标签。支持 `[说]`、`【说】`、`说：`、`Say:` 等多种写法。
fn split_tag(line: &str) -> (Tag, &str) {
    let s = line.trim_start();

    // 方括号 / 中文方括号
    for (open, close) in [("[", "]"), ("【", "】")] {
        if let Some(rest) = s.strip_prefix(open) {
            if let Some(idx) = rest.find(close) {
                let name = &rest[..idx];
                let body = rest[idx + close.len()..].trim();

                // 顺序要紧：先当标签试。
                // `【状态】心情=戒备` 是状态行，`【状态】` 单独成行是节标题——
                // 两者只差"后面有没有内容"，所以必须看 body 再决定。
                if let Some(t) = tag_from_name(name) {
                    if body.is_empty() && is_section_name(name) {
                        return (Tag::Header, "");
                    }
                    return (t, body);
                }

                // 不是标签名，但是节名（典型就是 `【表演】`）：这行是排版，不是内容。
                if is_section_name(name) {
                    return if body.is_empty() {
                        (Tag::Header, "")
                    } else {
                        // 标题后面还写了内容：吞掉标题、留下内容。
                        // 宁可留下多余的一句话，不可静默吃掉模型真的说了的话。
                        (Tag::Speech, body)
                    };
                }

                // 既不是标签名、也不是节名——模型**自创了一个标签**。
                //
                // 实机证据（phi4，本机 Ollama）：
                // `[看] 陈默似乎不知该怎么继续，眼睛里藏着另一样话语。`
                // 它想写一句神态描写，却不肯用 `[做]`。旧行为是把整行（连方括号）
                // 一起退回，于是台词里出现 `[看] 陈默似乎…`——既不是台词也不是
                // 动作，是纯机器痕迹，而且**会直接显示给用户**。
                //
                // 这里只剥壳，**不改通道归属**（仍然进台词），这是有意的取舍：
                // 自创标签的语义只有模型自己知道，`[看]` 像动作、`[惊]` 更像状态，
                // 猜错会把内容放错通道——放错比"留在台词里"更难发现，因为
                // 每个通道看起来都有输出。治本靠格式说明里那句
                // 「不要自己发明新的标签」。
                return if body.is_empty() {
                    (Tag::None, "")
                } else {
                    (Tag::Speech, body)
                };
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
        ("表情：", Tag::Sticker),
        ("贴图：", Tag::Sticker),
        ("图片：", Tag::Image),
        ("照片：", Tag::Image),
        ("图：", Tag::Image),
        ("回忆：", Tag::Recall),
        ("取忆：", Tag::Recall),
        ("回想：", Tag::Recall),
        ("联想：", Tag::Associate),
        ("取联：", Tag::Associate),
        ("查阅：", Tag::Lore),
        ("查书：", Tag::Lore),
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
        // 表情包：按情绪组织的那一类
        "表情" | "表情包" | "贴图" | "sticker" | "stickers" | "emoji" => Tag::Sticker,
        // 图片 / 照片：按内容组织的那一类
        "图片" | "照片" | "图" | "图像" | "photo" | "photos" | "image" | "images" | "picture"
        | "pic" => Tag::Image,
        // 控制通道
        "回忆" | "取忆" | "回想" | "想起来" | "recall" | "recollect" => Tag::Recall,
        "联想" | "取联" | "发散" | "associate" | "assoc" | "diverge" => Tag::Associate,
        "查阅" | "查书" | "查询" | "设定" | "lore" | "lookup" | "reference" => Tag::Lore,
        _ => return None,
    })
}

/// 从 `[表情]` 的正文里抠出编号。
///
/// 模型很难每次都只写一个干净的编号，常见写法有：
/// `happy_01` / `happy_01.png` / `happy_01（开心）` / `"happy_01"` /
/// `happy_01 开心地笑`。这里统一取"第一个词"，再剥掉包裹符号与扩展名。
fn parse_sticker_id(body: &str) -> Option<String> {
    let raw = body
        .split([
            ' ', '\t', '，', ',', '；', ';', '（', '(', '【', '[', '、', '\n',
        ])
        .map(|s| s.trim())
        .find(|s| !s.is_empty())?;
    let mut id = raw.trim_matches(|c: char| {
        matches!(
            c,
            '「' | '」'
                | '"'
                | '\''
                | '“'
                | '”'
                | '（'
                | '）'
                | '('
                | ')'
                | '。'
                | '：'
                | ':'
                | '['
                | ']'
                | '【'
                | '】'
                | '='
                | '-'
        )
    });
    if id.is_empty() {
        return None;
    }
    // 模型偶尔会带上扩展名，统一去掉，保证和目录里的 id 对得上
    if let Some((stem, ext)) = id.rsplit_once('.') {
        if crate::sticker::IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
            id = stem;
        }
    }
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// 解析 `[图片] <编号> ｜ <配文>`。
///
/// 编号与配文的切分点有三类：空白、全/半角竖线、全/半角冒号。
/// 之所以连竖线切分都要支持：模型很爱写 `[图片] old_photo | 你看这个`，
/// 而竖线在中文输入法下经常是全角的 `｜`。
fn parse_image_request(body: &str) -> Option<ImageRequest> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let cut = body
        .find(|c: char| c.is_whitespace() || matches!(c, '｜' | '|' | '：' | ':'))
        .unwrap_or(body.len());
    let (key_raw, rest) = body.split_at(cut);
    let key = parse_sticker_id(key_raw)?;
    let caption = rest
        .trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, '｜' | '|' | '：' | ':' | '-' | '—' | '，' | ',')
        })
        .trim()
        .to_string();
    Some(ImageRequest { key, caption })
}

/// 解析 `[回忆] <查询词> | 条数=<n>`。
///
/// 查询词里出现竖线是很常见的（模型抄了格式说明），所以只把**最后**一段
/// 当参数表：先按竖线切，第一段必是查询词，其余段落里能解析出 `条数=` 的才算参数。
fn parse_directive(tag: Tag, body: &str) -> Option<Directive> {
    let kind = match tag {
        Tag::Recall => DirectiveKind::Recall,
        Tag::Associate => DirectiveKind::Associate,
        Tag::Lore => DirectiveKind::Lore,
        _ => return None,
    };
    let body = body.trim().trim_matches(['"', '\'', '「', '」']);
    if body.is_empty() {
        return None;
    }
    let mut parts = body.split('|').map(|s| s.trim());
    let query = parts.next().unwrap_or_default().trim().to_string();
    let query = query
        .trim_end_matches(['。', '？', '！', '?', '!', ',', '，'])
        .to_string();
    if query.is_empty() {
        return None;
    }
    let mut directive = Directive::new(kind, query);
    for part in parts {
        let kv = parse_kv(part);
        let raw = kv
            .get("条数")
            .or_else(|| kv.get("数量"))
            .or_else(|| kv.get("limit"))
            .or_else(|| kv.get("top"));
        if let Some(n) = raw.and_then(|v| v.trim().parse::<usize>().ok()) {
            directive.limit = Some(n);
        }
    }
    Some(directive)
}

/// 从 JSON 里读图片列表。元素可以是字符串（`"old_photo"`）或对象。
fn json_images(v: Option<&serde_json::Value>) -> Vec<ImageRequest> {
    let mut out = Vec::new();
    match v {
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                collect_image(item, &mut out);
            }
        }
        Some(other) => collect_image(other, &mut out),
        None => {}
    }
    out
}

fn collect_image(v: &serde_json::Value, out: &mut Vec<ImageRequest>) {
    match v {
        serde_json::Value::String(s) => {
            if let Some(r) = parse_image_request(s) {
                out.push(r);
            }
        }
        serde_json::Value::Object(o) => {
            let key = o
                .get("key")
                .or_else(|| o.get("id"))
                .or_else(|| o.get("file"))
                .or_else(|| o.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or_default();
            if key.trim().is_empty() {
                return;
            }
            let caption = o
                .get("caption")
                .or_else(|| o.get("text"))
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            out.push(ImageRequest {
                key: key.trim().to_string(),
                caption,
            });
        }
        _ => {}
    }
}

/// 从 JSON 顶层读控制指令。
fn json_directives(obj: &serde_json::Map<String, serde_json::Value>) -> Vec<Directive> {
    let mut out = Vec::new();
    for (field, kind) in [
        ("recall", DirectiveKind::Recall),
        ("remember_query", DirectiveKind::Recall),
        ("associate", DirectiveKind::Associate),
        ("assoc", DirectiveKind::Associate),
        ("lore", DirectiveKind::Lore),
        ("lookup", DirectiveKind::Lore),
    ] {
        let Some(v) = obj.get(field) else { continue };
        for d in collect_directives(v, kind) {
            out.push(d);
        }
    }
    // 也接受显式的 `"directives": [{"kind":"recall","query":"..."}]`
    if let Some(serde_json::Value::Array(items)) = obj.get("directives") {
        for item in items {
            let Some(o) = item.as_object() else { continue };
            let kind = match o.get("kind").and_then(|x| x.as_str()) {
                Some("recall") => DirectiveKind::Recall,
                Some("associate") => DirectiveKind::Associate,
                Some("lore") => DirectiveKind::Lore,
                _ => continue,
            };
            let query = o.get("query").and_then(|x| x.as_str()).unwrap_or_default();
            if query.trim().is_empty() {
                continue;
            }
            let mut d = Directive::new(kind, query.trim());
            if let Some(n) = o.get("limit").and_then(|x| x.as_u64()) {
                d.limit = Some(n as usize);
            }
            out.push(d);
        }
    }
    out
}

fn collect_directives(v: &serde_json::Value, kind: DirectiveKind) -> Vec<Directive> {
    let mut out = Vec::new();
    match v {
        serde_json::Value::String(s) => {
            let q = s.trim();
            if !q.is_empty() {
                out.push(Directive::new(kind, q));
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    serde_json::Value::String(s) => {
                        let q = s.trim();
                        if !q.is_empty() {
                            out.push(Directive::new(kind, q));
                        }
                    }
                    serde_json::Value::Object(o) => {
                        let q = o
                            .get("query")
                            .or_else(|| o.get("q"))
                            .or_else(|| o.get("text"))
                            .and_then(|x| x.as_str())
                            .unwrap_or_default();
                        if q.trim().is_empty() {
                            continue;
                        }
                        let mut d = Directive::new(kind, q.trim());
                        if let Some(n) = o.get("limit").and_then(|x| x.as_u64()) {
                            d.limit = Some(n as usize);
                        }
                        out.push(d);
                    }
                    _ => {}
                }
            }
        }
        serde_json::Value::Object(o) => {
            // 单对象写法：`"associate": {"query":"旧书店","limit":2}`
            let q = o
                .get("query")
                .or_else(|| o.get("q"))
                .or_else(|| o.get("text"))
                .and_then(|x| x.as_str())
                .unwrap_or_default();
            if q.trim().is_empty() {
                return out;
            }
            let mut d = Directive::new(kind, q.trim());
            if let Some(n) = o.get("limit").and_then(|x| x.as_u64()) {
                d.limit = Some(n as usize);
            }
            out.push(d);
        }
        _ => {}
    }
    out
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
    fn section_headers_do_not_leak_into_speech() {
        // 这段是 phi4 实机跑出来的**原文**（本机 Ollama，E2E）。
        // 它几乎完美遵守了格式说明——问题在于说明里的分节标题
        // `【表演】` / `【状态】` 是 Styx 自己写的，解析器却只认 `[说]`，
        // 于是 `【表演】` 掉进"无标签行 → 台词"的兜底，原样念给用户听。
        let raw = "【表演】  \n\
                   [做] 换了根干燥的手帕擦了一下桌上的雨水痕迹。\n\
                   \n\
                   [想] 她不是不想再看那些照片，只是总觉得再翻一遍就是最后一遍。\n\
                   \n\
                   【状态】  \n\
                   [情] 精力=0.60  紧张=0.20  \n\
                   [场] 时间=傍晚 地点=拾光旧书店";
        let r = Reply::parse(raw).unwrap();
        assert_eq!(r.actions.len(), 1, "动作该被识别");
        assert_eq!(r.thoughts.len(), 1, "内心该被识别");
        assert_eq!(
            r.speech,
            Vec::<String>::new(),
            "不该有任何台词：模型一个字都没说"
        );
        assert_eq!(
            r.scene_set.get("location").map(String::as_str),
            Some("拾光旧书店")
        );
    }

    #[test]
    fn the_long_control_section_header_does_not_leak_either() {
        // 格式说明里第三个标题是一整句话，比 `【表演】` 更容易被当成内容漏出去。
        // 实机跑 phi4 时它确实漏了，把"先申请查资料"念成了台词。
        let raw = "【想不起事情时，先申请查资料】\n[回忆] 伞的颜色";
        let r = Reply::parse(raw).unwrap();
        assert!(r.speech.is_empty(), "标题不该出现在台词里：{:?}", r.speech);
        assert_eq!(r.directives.len(), 1, "控制指令该被识别");
        assert_eq!(r.directives[0].query, "伞的颜色");
    }

    #[test]
    fn section_header_is_not_confused_with_a_state_line() {
        // `【状态】` 单独成行是标题；`【状态】心情=戒备` 才是状态行。
        // 两者只差"后面有没有内容"，不能一刀切。
        let r = Reply::parse("【状态】\n[情] 心情=戒备").unwrap();
        assert_eq!(r.state_delta.mood.as_deref(), Some("戒备"));
        assert!(r.speech.is_empty());

        let r = Reply::parse("【状态】心情=戒备").unwrap();
        assert_eq!(r.state_delta.mood.as_deref(), Some("戒备"));
        assert!(r.speech.is_empty());
    }

    #[test]
    fn header_with_trailing_prose_keeps_the_prose() {
        // 模型偶尔把标题和内容写在一行。宁可留内容，不可丢内容——
        // 但标题本身必须是吞掉的。
        let r = Reply::parse("【表演】她笑了笑").unwrap();
        assert_eq!(r.speech, vec!["她笑了笑".to_string()]);
    }

    #[test]
    fn header_ends_the_previous_block() {
        // 标题是排版，不是内容：它不该被当作上一块的续行，
        // 否则标题后面的第一句话会挂到上一个动作上去。
        let r = Reply::parse("[做] 她抬起头\n【表演】\n[说] 你来了").unwrap();
        assert_eq!(r.actions, vec!["她抬起头".to_string()]);
        assert_eq!(r.speech, vec!["你来了".to_string()]);
    }

    #[test]
    fn every_section_header_in_the_instructions_is_recognised() {
        // 格式说明是模型唯一能看到的"接口文档"。里面凡是**整行只有一对括号**
        // 的，都是分节标题，模型会照着回显——解析器必须认得它们。
        //
        // 这条测试专门防漂移：以后往说明里加了新标题却忘了登记进
        // SECTION_NAMES，这里会立刻失败，而不是等到实机跑出脏台词才发现。
        let mut checked = 0;
        for line in Reply::format_instructions().lines() {
            let t = line.trim();
            let (open, close) = if t.starts_with('【') {
                ('【', '】')
            } else if t.starts_with('[') {
                ('[', ']')
            } else {
                continue;
            };
            let Some(idx) = t.find(close) else { continue };
            // 只看"整行就是一对括号"的行；带内容的（如 `[说] 台词内容`）不是标题
            if !t[idx + close.len_utf8()..].trim().is_empty() {
                continue;
            }
            let name = &t[open.len_utf8()..idx];
            let (tag, _) = split_tag(t);
            assert!(
                tag == Tag::Header || tag_from_name(name).is_some(),
                "格式说明里的分节标题 `{t}` 没被解析器识别，它会漏进台词当台词念出去。\
                 要么把它加进 SECTION_NAMES，要么别在说明里用这种写法"
            );
            checked += 1;
        }
        assert_eq!(
            checked, 2,
            "说明里应当有 2 个分节标题（表演 / 状态）。数量变了说明说明改过，\
             这条测试也该跟着复核——别让它退化成空转"
        );
    }

    #[test]
    fn screenplay_notation_is_understood() {
        // 这段是 phi4（本机 Ollama，web E2E）某一轮的**原文**：它完全没用
        // 标签，改用剧本排版。而这套排版正是 Styx 自己渲染给模型看的历史格式
        // （见 Event::render_line），模型是在模仿它看到过的转录。
        // 解析器必须认自己的输出格式，否则模型会"因为模仿而被罚"：
        // 整段退化成台词，动作与内心双双丢失。
        let raw = "（林夏脑海中的情绪忽然感受到了不协调。）\n\
                   （她淡淡地扫了一眼那张表情包。）\n\
                   林夏：「雨停了吧？」\n\
                   （林夏心想：哭着也好，笑着也好，总是在书里找到了心安处。）";
        let r = Reply::parse(raw).unwrap();
        assert_eq!(
            r.speech,
            vec!["雨停了吧？".to_string()],
            "只有 `X：「…」` 那一行是台词，且引号该被剥掉"
        );
        assert_eq!(r.actions.len(), 2, "两条圆括号是动作");
        assert_eq!(r.actions[0], "林夏脑海中的情绪忽然感受到了不协调。");
        assert_eq!(r.thoughts.len(), 1, "`（X心想：…）` 是内心");
        assert_eq!(
            r.thoughts[0], "哭着也好，笑着也好，总是在书里找到了心安处。",
            "独白正文里不该残留 `林夏心想：`"
        );
    }

    #[test]
    fn screenplay_notation_strips_nested_quotes() {
        // 模型会套两层引号（`林夏：「「你来了。」」`），剥一层不够：
        // 剩下那个 `「你来了。」` 渲染出去还是双层引号。
        let r = Reply::parse("林夏：「「你来了。」」").unwrap();
        assert_eq!(r.speech, vec!["你来了。".to_string()]);
    }

    #[test]
    fn screenplay_notation_mixed_with_a_speaker_prefix() {
        // phi4（本机 Ollama，web E2E）某一轮的**原文**：它混用了两种记法——
        // `林夏：（…）` 是"说话人 + 括号动作"。旧解析器只认整行括号，
        // 于是这一行漏成了台词，动作通道空着。
        let raw = "[表情] shy_04\n\
                   \n\
                   林夏：（轻轻转动柜台上的玻璃杯，发出沙沙的声音，目光再度落在旧照片上。）\n\
                   \n\
                   （林夏心想：这些照片，每次看到都像一道难过的回忆。）";
        let r = Reply::parse(raw).unwrap();
        assert_eq!(r.stickers, vec!["shy_04".to_string()]);
        assert_eq!(
            r.actions,
            vec!["轻轻转动柜台上的玻璃杯，发出沙沙的声音，目光再度落在旧照片上。".to_string()]
        );
        assert_eq!(
            r.thoughts,
            vec!["这些照片，每次看到都像一道难过的回忆。".to_string()]
        );
        assert!(r.speech.is_empty(), "这里没有一句是台词：{:?}", r.speech);
    }

    #[test]
    fn a_real_tag_hidden_behind_a_speaker_prefix_still_routes() {
        // phi4（本机 Ollama，web E2E）的原文：它把标签塞进"说话人 + 引号"这个
        // 壳里。若只剥壳不看里头，素材通道会整条塌成台词——而模型明明写了编号。
        let r = Reply::parse("林夏：「[表情] 无语」").unwrap();
        assert_eq!(r.stickers, vec!["无语".to_string()]);
        assert!(r.speech.is_empty(), "这不是台词：{:?}", r.speech);

        let r = Reply::parse("林夏：「[说] 不卖。」").unwrap();
        assert_eq!(r.speech, vec!["不卖。".to_string()]);
        assert!(r.stickers.is_empty());

        let r = Reply::parse("林夏：「[忆] 她答应留一本书 | 标签=约定 | 重要度=0.8」").unwrap();
        assert_eq!(r.memories.len(), 1);
        assert!(r.speech.is_empty());
    }

    #[test]
    fn screenplay_detection_does_not_eat_normal_lines() {
        // 一句正常台词里夹着括号，不该被切成动作。
        let r = Reply::parse("他说（笑）就走开了。").unwrap();
        assert_eq!(r.speech, vec!["他说（笑）就走开了。".to_string()]);
        assert!(r.actions.is_empty());

        // `字段：值` 不带引号，不该被当成"说话人：台词"。
        let r = Reply::parse("时间：傍晚 地点：旧书店").unwrap();
        assert_eq!(r.speech, vec!["时间：傍晚 地点：旧书店".to_string()]);

        // 转述别人的话：整行不以 `」` 收尾，保持原样进台词。
        let r = Reply::parse("陈默：「我不去」——他这么说过。").unwrap();
        assert_eq!(r.speech, vec!["陈默：「我不去」——他这么说过。".to_string()]);
    }

    #[test]
    fn tag_on_its_own_line_takes_the_next_line_as_its_body() {
        // 这段是 phi4（本机 Ollama，web E2E）的**原文**：它把 `【忆】` 和 `【场】`
        // 写成独立一行，正文放在下一行——"小标题 + 正文"，markdown 式写法。
        // 旧解析器只认"标签 + 同行正文"，于是记忆与场景双双丢失，
        // 而那两行正文还漏进了台词。
        let raw = "【表演】  \n\
                   [说] 表情包就是些无聊的事情。  \n\
                   [做] 周身稍稍转动，微微低头看向柜台下的那张照片。  \n\
                   \n\
                   【状态】  \n\
                   [情] 心情=沉思 效价=-0.10 唤醒=0.35\n\
                   \n\
                   【忆】  \n\
                   相册最上面那一格夹着一张泛黄的旧照片 | 标签=a | 重要度=0.9  \n\
                   \n\
                   【场】  \n\
                   时间=傍晚 地点=拾光旧书店 环境=灯光昏暗 在场=陈默";
        let r = Reply::parse(raw).unwrap();
        assert_eq!(r.speech, vec!["表情包就是些无聊的事情。".to_string()]);
        assert_eq!(r.actions.len(), 1);
        assert_eq!(r.state_delta.mood.as_deref(), Some("沉思"));
        assert_eq!(r.memories.len(), 1, "记忆必须落库，不能只是漏进台词");
        assert_eq!(r.memories[0].text, "相册最上面那一格夹着一张泛黄的旧照片");
        assert_eq!(r.memories[0].tags, vec!["a".to_string()]);
        assert_eq!(
            r.scene_set.get("location").map(String::as_str),
            Some("拾光旧书店"),
            "场景必须落库"
        );
    }

    #[test]
    fn a_held_tag_does_not_swallow_a_line_that_is_not_its_body() {
        // 安全阀：`[情]` 后面跟的如果是台词而不是 `key=value`，
        // 不能被当成状态正文静默吃掉。
        let r = Reply::parse("[情]\n她抬起头，看了他一眼。").unwrap();
        assert!(r.state_delta.is_empty());
        assert_eq!(
            r.speech,
            vec!["她抬起头，看了他一眼。".to_string()],
            "形状对不上的行必须留在台词里，宁可少一条状态也不能丢话"
        );

        // 下一行自己带标签时，挂起的标签直接作废
        let r = Reply::parse("[情]\n[说] 不卖。").unwrap();
        assert!(r.state_delta.is_empty());
        assert_eq!(r.speech, vec!["不卖。".to_string()]);
    }

    #[test]
    fn tag_on_its_own_line_works_for_material_channels_too() {
        // 实测 phi4 也会这么写素材标签
        let r = Reply::parse("[表情]\nhappy_01").unwrap();
        assert_eq!(r.stickers, vec!["happy_01".to_string()]);

        let r = Reply::parse("[图片]\nold_photo | 你看这个").unwrap();
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].key, "old_photo");
        assert_eq!(r.images[0].caption, "你看这个");
    }

    /// 台词的引号壳必须在**解析层**剥掉，让 `reply.speech` 里存干净文本。
    ///
    /// 只在渲染层剥是不够的：`reply.speech` 是走线数据，web 前端直接把数组
    /// 显示出来。同一句台词若只有命令行干净，就会出现"网页比命令行多一层
    /// 引号"——同一条数据在两个前端长得不一样，是最难归因的一类不一致。
    ///
    /// 四条路径都要覆盖：行首标签、纯引号行（走"台词续行"兜底）、
    /// 剧本排版、以及**不能被剥**的引语+补述。
    #[test]
    fn speech_quotes_are_stripped_at_parse_time() {
        let r = Reply::parse("[说] 「伞？我倒是记得。」").unwrap();
        assert_eq!(r.speech, vec!["伞？我倒是记得。"]);

        let r = Reply::parse("「伞？我倒是记得。」").unwrap();
        assert_eq!(r.speech, vec!["伞？我倒是记得。"]);

        let r = Reply::parse("林夏：「你来了。」").unwrap();
        assert_eq!(r.speech, vec!["你来了。"]);

        // 引语 + 补述：整行不是"被引号包住"，所以一层都不剥。
        // 剥了就会丢掉 `——他这么说过。`，那是最安静的一种信息丢失。
        let r = Reply::parse("林夏：「我不去」——他这么说过。").unwrap();
        assert_eq!(r.speech, vec!["林夏：「我不去」——他这么说过。"]);
    }

    /// 不带括号的内心句必须进 `thoughts`，不能因为"无标签行并入上一块"
    /// 而并进前面的动作里。
    ///
    /// 实测来源：一轮 web E2E 里 phi4 写了三句 `林夏暗想：…` / `林夏想：…`，
    /// 全部并进了动作，回合统计成了 `thought 1 / action 10`——**内心通道
    /// 看起来像坏了，其实是被动作吸走了**。
    #[test]
    fn unparenthesised_mind_line_becomes_thought() {
        let r = Reply::parse("林夏暗想：她的笑法让我觉得不太习惯。").unwrap();
        assert_eq!(r.thoughts, vec!["她的笑法让我觉得不太习惯。"]);
        assert!(r.actions.is_empty(), "不能同时落进动作");
        assert!(r.speech.is_empty(), "更不能漏成台词");

        // 要紧的是**紧跟在动作行后面**也得分得开：这正是实测里的上下文，
        // 靠的就是这条规则抢在"并入上一块"之前。
        let r = Reply::parse(
            "（低头整理书架上那些未打标签的书。）\n林夏暗想：她的笑法让我觉得不太习惯。",
        )
        .unwrap();
        assert_eq!(r.actions, vec!["低头整理书架上那些未打标签的书。"]);
        assert_eq!(r.thoughts, vec!["她的笑法让我觉得不太习惯。"]);

        let r = Reply::parse("林夏想：她可能看出我的心思。").unwrap();
        assert_eq!(r.thoughts, vec!["她可能看出我的心思。"]);
    }

    /// 上一条规则的反例：`X：` 这个形状太常见，凡是往里加条件都必须
    /// 同时钉住"什么不该被吃掉"。
    #[test]
    fn mind_suffix_rule_does_not_eat_neighbouring_shapes() {
        // 冒号后是引号 → 这是台词，只是没写说话人的引语
        let r = Reply::parse("林夏：「我不去」").unwrap();
        assert_eq!(r.speech, vec!["我不去"]);
        assert!(r.thoughts.is_empty());

        // 字段行（无标签，会走 screenplay_form 兜底）：`时间` 不以心理动词结尾
        let r = Reply::parse("时间：傍晚").unwrap();
        assert!(r.thoughts.is_empty(), "字段行不能被当成内心");

        // `联想` 以"想"结尾，但它是通道名。漏写方括号时也不能被吞成内心。
        let r = Reply::parse("联想：雨").unwrap();
        assert!(r.thoughts.is_empty(), "联想 是通道名，不是心理动词");
    }

    /// 模型把格式说明抄进回复时，那些行必须被丢掉——它们不是台词。
    ///
    /// 实测原文（phi4，本机 Ollama，web E2E）：模型写完 `[场]` 之后，
    /// 把说明里 `规则：` 那一段连同列表项一字不差地复述了出来，
    /// 10 行说明全部落进 `speech`，用户会直接看到规则文本。
    #[test]
    fn instruction_text_echoed_by_the_model_is_not_speech() {
        // 从说明里动态取一行，改说明时测试不会腐烂
        let echoed = Reply::format_instructions()
            .lines()
            .find(|l| l.starts_with("写的时候，至少给一条"))
            .expect("说明里应当有那段散文");
        assert!(
            echoed.contains("可选但推荐"),
            "取错了行，这条测试就失去意义了"
        );

        let raw = format!("[说] 母亲去年走了。\n[做] 拿着一本书随意翻着。\n{echoed}\n");
        let r = Reply::parse(&raw).unwrap();
        assert_eq!(r.speech, vec!["母亲去年走了。"], "说明行不该变成台词");
        assert_eq!(r.actions, vec!["拿着一本书随意翻着。"]);
    }

    /// 指纹只认逐字相同，不能顺手吃掉真台词。
    #[test]
    fn instruction_fingerprint_does_not_eat_real_speech() {
        // 说明里有 `[说] 台词内容` 这个占位行。如果比对放宽成"包含"，
        // 下面这句真台词就会被误删。
        let r = Reply::parse("[说] 台词内容真不错。").unwrap();
        assert_eq!(r.speech, vec!["台词内容真不错。"]);

        // 说明里的散文句子被模型当作台词说出来（加了前缀），也不该被吃
        let r = Reply::parse("[说] 我也想知道事情会变成什么样。").unwrap();
        assert_eq!(r.speech, vec!["我也想知道事情会变成什么样。"]);
    }

    /// 模型自创标签时至少要剥掉外壳。
    ///
    /// 实测原文（phi4，本机 Ollama）：`[看] 陈默似乎不知该怎么继续，
    /// 眼睛里藏着另一样话语。` 旧行为把方括号一起退回，台词就成了
    /// `[看] 陈默似乎…`——机器痕迹直接显示给用户。
    #[test]
    fn invented_tag_loses_its_brackets() {
        let r = Reply::parse("[看] 陈默似乎不知该怎么继续，眼睛里藏着另一样话语。").unwrap();
        assert_eq!(
            r.speech,
            vec!["陈默似乎不知该怎么继续，眼睛里藏着另一样话语。"]
        );
        assert!(
            !r.speech[0].contains('['),
            "用户看到的内容里不该有方括号：{:?}",
            r.speech
        );

        // 光秃秃的自创标签行直接忽略，不能变成一条空台词
        let r = Reply::parse("[看]\n[说] 我还好。").unwrap();
        assert_eq!(r.speech, vec!["我还好。"]);
    }

    /// 模型把 `[情]` 的标签写丢时，那行不能被并进上一条内心。
    ///
    /// 实测（phi4，本机 Ollama，5 轮里出现 2 次）：独白末尾挂着一串
    /// `心情=平静 效价=-0.10 唤醒=-0.05 …`，用户直接看到机器参数。
    #[test]
    fn orphaned_state_line_does_not_join_the_thought_above_it() {
        let raw = "林夏暗想：别提这样的话题。\n心情=平静 效价=-0.10 唤醒=-0.05 精力=0.78 紧张=0.17";
        let r = Reply::parse(raw).unwrap();
        assert_eq!(r.thoughts, vec!["别提这样的话题。"]);
        assert!(r.actions.is_empty(), "不该落进动作：{:?}", r.actions);
        assert!(r.speech.is_empty(), "更不该落进台词：{:?}", r.speech);
        // 内容不该被丢掉，而要真的作用到状态上
        assert_eq!(r.state_delta.mood.as_deref(), Some("平静"));
        assert!((r.state_delta.valence.unwrap() + 0.10).abs() < 1e-6);
    }

    /// 上一条规则的边界：判据只认项目专有维度名，不能把普通台词吃掉。
    #[test]
    fn state_line_detection_does_not_eat_ordinary_speech() {
        let r = Reply::parse("[说] 一加一等于二，就这么简单。").unwrap();
        assert_eq!(r.speech, vec!["一加一等于二，就这么简单。"]);
        assert!(r.state_delta.mood.is_none());
    }

    /// 括号里装着旁路标签时，必须拆出来投递给对应通道。
    ///
    /// 实测形状（phi4，本机 Ollama，web E2E）：`（[表情] 心意_11）`——
    /// 模型把表情包也塞进了括号。旧实现整段切给动作，于是动作里出现
    /// 文本 `[表情] 心意_11`（用户直接看到方括号），而素材通道一次都没触发，
    /// 尽管模型明明写了编号。
    #[test]
    fn a_side_channel_tag_inside_parentheses_is_split_out() {
        let r = Reply::parse("（[表情] 心意_11）").unwrap();
        assert_eq!(r.stickers, vec!["心意_11"]);
        assert!(r.actions.is_empty(), "不该同时落进动作：{:?}", r.actions);
        assert!(r.speech.is_empty());

        // 和动作写在相邻两行时也一样
        let r = Reply::parse(
            "（林夏注视着陈默，眼神里带着柔和的关注，稍稍侧过身子，让一张书板挡在身前。）\n\
             （[表情] 心意_11）",
        )
        .unwrap();
        assert_eq!(
            r.actions,
            vec!["林夏注视着陈默，眼神里带着柔和的关注，稍稍侧过身子，让一张书板挡在身前。"]
        );
        assert_eq!(r.stickers, vec!["心意_11"]);
        assert!(
            !r.actions.iter().any(|a| a.contains('[')),
            "用户看不到方括号：{:?}",
            r.actions
        );
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

    #[test]
    fn parses_sticker_tag() {
        let r = Reply::parse("[做] 她愣了一下\n[表情] happy_01\n[说] 嗯。").unwrap();
        assert_eq!(r.stickers, vec!["happy_01".to_string()]);
        assert_eq!(r.speech, vec!["嗯。".to_string()]);
        assert_eq!(r.actions.len(), 1);
        assert!(r.plain_text().contains("［表情：happy_01］"));
    }

    #[test]
    fn sticker_tag_tolerates_messy_writing() {
        let cases = [
            "[表情] happy_01",
            "【贴图】cry_03.png",
            "表情：love_11（喜欢）",
            "[表情] \"shy_04\" 害羞地笑",
            "[sticker] peeking_26",
            "[表情]  thanks_12 ",
        ];
        for raw in cases {
            let r = Reply::parse(raw).unwrap();
            assert_eq!(r.stickers.len(), 1, "{raw} → {:?}", r.stickers);
            assert!(
                r.stickers[0]
                    .chars()
                    .all(|c| c != '.' && c != '"' && c != '（'),
                "{raw} → {:?}",
                r.stickers
            );
        }
    }

    #[test]
    fn sticker_does_not_swallow_the_next_line() {
        // 表情是独立通道，紧跟其后的无标签行仍然是台词
        let r = Reply::parse("[表情] happy_01\n你来了。").unwrap();
        assert_eq!(r.stickers, vec!["happy_01".to_string()]);
        assert_eq!(r.speech, vec!["你来了。".to_string()]);
    }

    #[test]
    fn sticker_only_reply_counts_as_content() {
        let r = Reply::parse("[表情] wailing_22").unwrap();
        assert!(!r.is_empty());
        assert!(r.speech.is_empty());
        assert_eq!(r.stickers.len(), 1);
    }

    #[test]
    fn duplicate_stickers_are_deduped() {
        let r = Reply::parse("[表情] happy_01\n[表情] happy_01\n[表情] cry_03").unwrap();
        assert_eq!(
            r.stickers,
            vec!["happy_01".to_string(), "cry_03".to_string()]
        );
    }

    #[test]
    fn sticker_tag_without_a_body_is_ignored() {
        let r = Reply::parse("[表情]   \n[说] 嗯。").unwrap();
        assert!(r.stickers.is_empty());
        assert_eq!(r.speech, vec!["嗯。".to_string()]);
    }

    #[test]
    fn json_mode_parses_stickers() {
        let r =
            Reply::parse(r#"{"speech":["嗯。"],"stickers":["happy_01","cry_03.png"]}"#).unwrap();
        assert!(r.from_json);
        assert_eq!(
            r.stickers,
            vec!["happy_01".to_string(), "cry_03".to_string()]
        );
    }

    // ---------------------------------------------------------- 图片通道

    #[test]
    fn parses_image_tag_with_caption() {
        let r = Reply::parse("[图片] old_photo 你看这个\n[说] 想起来了吗？").unwrap();
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].key, "old_photo");
        assert_eq!(r.images[0].caption, "你看这个");
        assert_eq!(r.speech, vec!["想起来了吗？".to_string()]);
        assert!(r.plain_text().contains("［图片：old_photo · 你看这个］"));
    }

    #[test]
    fn image_tag_tolerates_separators_and_bare_keys() {
        let cases = [
            ("[图片] a.png", ""),
            ("[图片] a.png ｜ 配文", "配文"),
            ("【照片】b | hello", "hello"),
            ("图片：c：带冒号", "带冒号"),
            ("[photo] d", ""),
        ];
        for (raw, caption) in cases {
            let r = Reply::parse(raw).unwrap();
            assert_eq!(r.images.len(), 1, "{raw}");
            assert_eq!(r.images[0].caption, caption, "{raw}");
            assert!(!r.images[0].key.contains('.'), "{raw} → {:?}", r.images[0]);
        }
    }

    #[test]
    fn image_tag_without_a_key_is_ignored() {
        let r = Reply::parse("[图片]   \n[说] 嗯。").unwrap();
        assert!(r.images.is_empty());
        assert_eq!(r.speech, vec!["嗯。".to_string()]);
    }

    #[test]
    fn image_is_no_longer_an_alias_of_sticker() {
        // 回归：`[图]` 曾经被当成表情包标签，导致"发照片"永远发不出图片
        let r = Reply::parse("[图] old_photo").unwrap();
        assert!(r.stickers.is_empty(), "不该被当成表情");
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].key, "old_photo");

        // 而 `[贴图]` 仍然是表情
        let s = Reply::parse("[贴图] happy_01").unwrap();
        assert_eq!(s.stickers, vec!["happy_01".to_string()]);
        assert!(s.images.is_empty());
    }

    #[test]
    fn image_only_reply_counts_as_content() {
        let r = Reply::parse("[图片] a").unwrap();
        assert!(!r.is_empty());
        assert!(r.has_performance());
    }

    // ---------------------------------------------------------- 控制通道

    #[test]
    fn parses_control_directives() {
        let r =
            Reply::parse("[回忆] 母亲留下的那张照片\n[联想] 旧书店\n[查阅] 拾光旧书店").unwrap();
        assert_eq!(r.directives.len(), 3);
        assert_eq!(r.directives[0].kind, DirectiveKind::Recall);
        assert_eq!(r.directives[0].query, "母亲留下的那张照片");
        assert_eq!(r.directives[1].kind, DirectiveKind::Associate);
        assert_eq!(r.directives[2].kind, DirectiveKind::Lore);
        assert!(r.has_directives());
        // 一条表演都没有：这不是给用户的回答
        assert!(r.is_empty());
        assert!(!r.has_performance());
    }

    #[test]
    fn directive_accepts_english_and_colon_forms() {
        let r = Reply::parse("[recall] photo\n联想：旧书店\n【查阅】设定").unwrap();
        assert_eq!(r.directives.len(), 3);
        assert_eq!(r.directives[0].kind, DirectiveKind::Recall);
        assert_eq!(r.directives[1].kind, DirectiveKind::Associate);
        assert_eq!(r.directives[2].kind, DirectiveKind::Lore);
    }

    #[test]
    fn directive_parses_a_limit_and_strips_punctuation() {
        let r = Reply::parse("[回忆] 照片 | 条数=5").unwrap();
        assert_eq!(r.directives.len(), 1);
        assert_eq!(r.directives[0].query, "照片");
        assert_eq!(r.directives[0].limit, Some(5));

        let p = Reply::parse("[回忆] 那张照片。").unwrap();
        assert_eq!(p.directives[0].query, "那张照片");
    }

    #[test]
    fn directive_without_a_query_is_ignored() {
        let r = Reply::parse("[回忆]\n[说] 嗯。").unwrap();
        assert!(r.directives.is_empty());
        assert_eq!(r.speech, vec!["嗯。".to_string()]);
    }

    #[test]
    fn directives_do_not_swallow_following_lines() {
        // 紧跟控制指令的无标签行必须仍然是台词，否则会静默吞掉一句话
        let r = Reply::parse("[回忆] 照片\n然后她抬起头。").unwrap();
        assert_eq!(r.directives.len(), 1);
        assert_eq!(r.speech, vec!["然后她抬起头。".to_string()]);
    }

    #[test]
    fn one_round_can_ask_for_a_lookup_only() {
        let r = Reply::parse("[回忆] 旧照片\n").unwrap();
        assert!(r.directives[0].query.contains("旧照片"));
        // `raw` 存的是 trim 之后、解析器真正吃进去的那份文本
        assert_eq!(r.raw, "[回忆] 旧照片");
        assert!(!r.from_json);
    }

    #[test]
    fn json_mode_parses_images_and_directives() {
        let raw = r#"{"speech":["嗯。"],
            "images":[{"key":"old_photo","caption":"你看"},"plain_photo"],
            "recall":["母亲的照片"],"associate":{"query":"旧书店","limit":2}}"#;
        let r = Reply::parse(raw).unwrap();
        assert!(r.from_json);
        assert_eq!(r.images.len(), 2);
        assert_eq!(r.images[0].key, "old_photo");
        assert_eq!(r.images[0].caption, "你看");
        assert_eq!(r.images[1].key, "plain_photo");
        assert_eq!(r.directives.len(), 2);
        assert_eq!(r.directives[0].kind, DirectiveKind::Recall);
        assert_eq!(r.directives[1].limit, Some(2));
    }

    #[test]
    fn json_mode_accepts_explicit_directives_array() {
        let raw = r#"{"speech":["嗯。"],"directives":[{"kind":"lore","query":"书店"}]}"#;
        let r = Reply::parse(raw).unwrap();
        assert_eq!(r.directives.len(), 1);
        assert_eq!(r.directives[0].kind, DirectiveKind::Lore);
        assert_eq!(r.directives[0].query, "书店");
    }

    #[test]
    fn json_with_only_directives_survives() {
        let r = Reply::parse(r#"{"recall":"旧照片"}"#).unwrap();
        assert!(r.from_json);
        assert!(r.has_directives());
        assert!(r.is_empty());
    }

    #[test]
    fn format_instructions_document_the_whole_protocol() {
        let text = Reply::format_instructions();
        for tag in [
            "[说]", "[做]", "[想]", "[情]", "[关系]", "[忆]", "[场]", "[回忆]", "[联想]", "[查阅]",
        ] {
            assert!(text.contains(tag), "格式说明里缺少 {tag}");
        }
        // 必须讲清楚"先申请、后表演"这条最容易踩空的规则
        assert!(text.contains("只写这些申请标签"));
        // 素材通道由目录按需追加：没有素材时绝不能宣传这个能力
        assert!(!text.contains("[表情]"));
        assert!(!text.contains("[图片]"));
    }
}
