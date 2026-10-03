//! 回合编排：内核的"主循环"。
//!
//! [`Kernel`] 把五个端口 + 会话状态串成一个闭环。它是**同步**的，
//! 这样既能嵌进 CLI 的 REPL，也能被 TCP 服务端用线程池驱动，
//! 还能被上层包成异步而不必改内核。
//!
//! ```ignore
//! use std::sync::Arc;
//! use styx_core::{CharacterCard, Kernel, KernelConfig, Scene};
//!
//! # fn main() -> styx_core::Result<()> {
//! # let card = CharacterCard::new("林夏");
//! # let llm: Arc<dyn styx_core::LlmPort> = unimplemented!();
//! # let memory: Arc<dyn styx_core::MemoryPort> = unimplemented!();
//! # let assoc: Arc<dyn styx_core::AssocPort> = unimplemented!();
//! let mut kernel = Kernel::builder(card, Scene::new("旧书店"))
//!     .llm(llm)
//!     .memory(memory)
//!     .assoc(assoc)
//!     .config(KernelConfig::default())
//!     .build()?;
//! let outcome = kernel.turn("我想买那本相册。")?;
//! println!("{}", outcome.reply.plain_text());
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use crate::character::CharacterCard;
use crate::error::{Result, StyxError};
use crate::event::{Event, EventKind};
use crate::media::{MediaLibrary, Photo};
use crate::ports::{
    AssocPort, Association, ChatMessage, Completion, LlmOptions, LlmPort, MemoryNote, MemoryPort,
    PoolPort, Recalled, ToolPort,
};
use crate::prompt::{PromptBudget, PromptBuilder, PromptReport};
use crate::protocol::{continuation_prompt, DirectiveOutcome, DirectiveKind};
use crate::reply::Reply;
use crate::scene::Scene;
use crate::session::{Audit, Guard, Session};
use crate::sticker::{Sticker, StickerCatalog};
use crate::text::{keywords, summarize};

/// 内核运行配置。
#[derive(Debug, Clone)]
pub struct KernelConfig {
    /// 用户在故事里的称呼（用于提示词）。
    pub user_name: String,
    /// 提示词预算。
    pub budget: PromptBudget,
    /// 生成参数。
    pub llm: LlmOptions,
    /// 每回合召回的记忆条数。
    pub memory_recall: usize,
    /// 每回合最大联想种子数。
    pub assoc_seeds: usize,
    /// 每个种子的联想条数。
    pub assoc_limit: usize,
    /// 共享记忆池召回条数。
    pub pool_recall: usize,
    /// 是否把事件写入长期记忆。
    pub write_memory: bool,
    /// 是否把高重要度事件写入共享记忆池。
    pub write_pool: bool,
    /// 事件写入长期记忆的重要度门槛。
    pub memory_threshold: f32,
    /// 每回合状态衰减率。
    pub decay_rate: f32,
    /// 一致性守护允许的最大重试次数。
    pub guard_retries: usize,
    /// 是否启用联想。
    pub use_assoc: bool,
    /// 是否启用共享记忆池。
    pub use_pool: bool,
    /// 每回合最多允许角色发几张表情包。
    pub sticker_limit: usize,
    /// 提示词里最多列几张表情包供模型选。
    pub sticker_options: usize,
    /// 对方发来表情包时的"情绪传染"系数（0 = 完全不传染）。
    ///
    /// 默认给一个很小的值：角色看到对方大哭的贴图不该无动于衷，
    /// 但也不该被一张图夺走人设。想完全交给模型决定就设成 0。
    pub sticker_affect: f32,
    /// 提示词里最多列几张图片素材供模型选。
    pub media_options: usize,
    /// 一回合最多允许角色发几张图片。
    pub image_limit: usize,
    /// 文本约定协议里"取资料 → 重新表演"最多允许几跳。
    ///
    /// 每一跳都是一次完整的模型调用，所以默认只给 1 跳。确实需要
    /// "先想起一个人名、再查这个人做过什么"的场合才调到 2。
    pub protocol_hops: usize,
    /// 一条控制指令默认取回几条。
    pub directive_limit: usize,
}

impl Default for KernelConfig {
    fn default() -> Self {
        KernelConfig {
            user_name: "对方".into(),
            budget: PromptBudget::default(),
            // 温度偏高一档：角色扮演要的是"这个人会怎么反应"，不是"最可能的下一句"。
            // 温度低了会明显趋向书面语和固定句式，人物就越演越像同一个人。
            llm: LlmOptions {
                temperature: Some(0.95),
                top_p: Some(0.95),
                max_tokens: Some(900),
                ..Default::default()
            },
            memory_recall: 8,
            // 种子宁多勿漏：关键词抽取没有词典，精确率天然有限，而联想端口
            // 自己会排序合并，多出来的种子只是多几次查询，漏掉却让特性空转
            assoc_seeds: 6,
            assoc_limit: 6,
            pool_recall: 4,
            write_memory: true,
            write_pool: false,
            memory_threshold: 0.55,
            decay_rate: 0.08,
            guard_retries: 1,
            use_assoc: true,
            use_pool: true,
            // 一回合只发一张：连发三张贴图不叫表达情绪，叫刷屏
            sticker_limit: 1,
            sticker_options: 36,
            sticker_affect: 0.08,
            media_options: 24,
            image_limit: 1,
            protocol_hops: 1,
            directive_limit: 3,
        }
    }
}

/// 后端状态快照（供 `probe` / `status` 展示与降级判断）。
#[derive(Debug, Clone)]
pub struct KernelStatus {
    pub llm: String,
    pub llm_endpoints: usize,
    pub memory: String,
    pub assoc: String,
    pub pool: Option<String>,
    pub tools: Option<String>,
    /// 表情包目录概况（`None` = 这个角色不会发表情）。
    pub stickers: Option<String>,
    /// 图片素材库概况（`None` = 这个角色没有可发的图片）。
    pub media: Option<String>,
    pub turn: u64,
    pub transcript: usize,
    pub degraded: Vec<String>,
}

impl KernelStatus {
    /// 渲染成可读多行文本。
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "模型    : {}（{} 个端点）\n",
            self.llm, self.llm_endpoints
        ));
        s.push_str(&format!("长期记忆: {}\n", self.memory));
        s.push_str(&format!("联想    : {}\n", self.assoc));
        s.push_str(&format!(
            "共享记忆: {}\n",
            self.pool.as_deref().unwrap_or("（未启用）")
        ));
        s.push_str(&format!(
            "工具    : {}\n",
            self.tools.as_deref().unwrap_or("（未启用）")
        ));
        s.push_str(&format!(
            "表情包  : {}\n",
            self.stickers.as_deref().unwrap_or("（未加载）")
        ));
        s.push_str(&format!(
            "图片素材: {}\n",
            self.media.as_deref().unwrap_or("（未加载）")
        ));
        s.push_str(&format!(
            "会话    : 第 {} 回合，{} 条事件\n",
            self.turn, self.transcript
        ));
        if self.degraded.is_empty() {
            s.push_str("降级    : 无");
        } else {
            s.push_str(&format!("降级    : {}", self.degraded.join("；")));
        }
        s
    }
}

/// 一个回合的完整结果。
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// 回合序号。
    pub turn: u64,
    /// 解析后的回复。
    pub reply: Reply,
    /// 本回合召回的长期记忆。
    pub recalled: Vec<Recalled>,
    /// 本回合的联想。
    pub associations: Vec<Association>,
    /// 本回合读到的共享记忆。
    pub pool_notes: Vec<Recalled>,
    /// 一致性审计。
    pub audit: Audit,
    /// 实际发生的重试次数。
    pub retries: usize,
    /// 写入长期记忆的条数。
    pub memory_written: usize,
    /// 写入共享记忆池的条数。
    pub pool_written: usize,
    /// 本回合发生的场景变更。
    pub scene_changed: Vec<String>,
    /// 提示词装配报告。
    pub report: PromptReport,
    /// 生成结果元信息。
    pub completion: Completion,
    /// 降级 / 异常提示。
    pub notices: Vec<String>,
    /// 文本约定协议里被执行的取资料指令（按执行顺序）。
    ///
    /// 这是"角色这一轮到底想起了什么"的完整记录，前端要把它显示出来——
    /// 否则使用者只看到角色突然提起旧照片，却不知道它为什么想起来。
    pub directives: Vec<DirectiveOutcome>,
    /// 实际发生的协议跳数（0 = 一次生成就答完了）。
    pub protocol_hops: usize,
    /// 本回合真正发出去的图片。
    pub sent_images: Vec<Photo>,
}

impl TurnOutcome {
    /// 人类可读的回放（CLI 用）。
    pub fn render(&self) -> String {
        let mut s = String::new();
        for a in &self.reply.actions {
            s.push_str(&format!("（{a}）\n"));
        }
        for t in &self.reply.thoughts {
            s.push_str(&format!("  ＞ {t}\n"));
        }
        for line in &self.reply.speech {
            s.push_str(line);
            s.push('\n');
        }
        for st in &self.reply.stickers {
            s.push_str(&format!("［表情：{st}］\n"));
        }
        for img in &self.sent_images {
            s.push_str(&format!("［图片：{}］\n", img.file));
        }
        s.trim_end().to_string()
    }

    /// 一行调试摘要。
    pub fn debug_line(&self) -> String {
        format!(
            "turn {} · {} · 审核{} · 重试{} · 取资料{} · 写记忆{} · {}",
            self.turn,
            self.report.render(),
            if self.audit.needs_retry() { "有违规" } else { "通过" },
            self.retries,
            self.directives.len(),
            self.memory_written,
            self.completion.endpoint
        )
    }
}

/// 本回合的"用户输入"是怎么来的。
///
/// 用枚举而不是 `Option<&Sticker>`：图片与表情包在**事件 meta** 上
/// 要写的东西完全不同（一个要 `sticker_id`，一个要 `image_id` + `image_file`），
/// 硬塞进一个类型迟早要靠 `if let` 到处分支。
#[derive(Debug, Clone, Copy)]
enum Incoming<'a> {
    /// 普通文本。
    None,
    /// 一张表情包（带情绪传染）。
    Sticker(&'a Sticker),
    /// 一张图片 / 照片（不带情绪传染）。
    Image(&'a Photo),
}

/// 解析一次生成结果。
///
/// 抽成自由函数是因为它在协议循环里要被调用多次：`Reply::parse` 只对
/// 空输出报错，而"模型吐了一句不能解析的东西"必须退化成台词而不是中断
/// 整个回合——角色扮演里宁可它答得平淡，也不能整轮没有回应。
fn parse_completion(text: &str) -> Reply {
    Reply::parse(text).unwrap_or_else(|_| Reply::as_plain_speech(text))
}

/// 角色扮演内核。
pub struct Kernel {
    session: Session,
    llm: Arc<dyn LlmPort>,
    memory: Arc<dyn MemoryPort>,
    assoc: Arc<dyn AssocPort>,
    pool: Option<Arc<dyn PoolPort>>,
    tools: Option<Arc<dyn ToolPort>>,
    /// 表情包目录。`None` 时内核不会在提示词里提表情，
    /// 也会拒绝任何 `[表情]` 输出——不会凭空发一张不存在的图。
    stickers: Option<Arc<StickerCatalog>>,
    /// 图片素材库。`None` 时同样拒绝任何 `[图片]` 输出。
    media: Option<Arc<MediaLibrary>>,
    config: KernelConfig,
    /// 上一轮审计留下的提醒（会注入下一轮提示）。
    pending_reminders: Vec<String>,
    last_audit: Audit,
}

/// 内核构建器。
pub struct KernelBuilder {
    card: CharacterCard,
    scene: Scene,
    session: Option<Session>,
    llm: Option<Arc<dyn LlmPort>>,
    memory: Option<Arc<dyn MemoryPort>>,
    assoc: Option<Arc<dyn AssocPort>>,
    pool: Option<Arc<dyn PoolPort>>,
    tools: Option<Arc<dyn ToolPort>>,
    stickers: Option<Arc<StickerCatalog>>,
    media: Option<Arc<MediaLibrary>>,
    config: KernelConfig,
}

impl Kernel {
    /// 开始构建内核。
    pub fn builder(card: CharacterCard, scene: Scene) -> KernelBuilder {
        KernelBuilder {
            card,
            scene,
            session: None,
            llm: None,
            memory: None,
            assoc: None,
            pool: None,
            tools: None,
            stickers: None,
            media: None,
            config: KernelConfig::default(),
        }
    }

    /// 直接构造（所有端口必须齐备）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: Session,
        llm: Arc<dyn LlmPort>,
        memory: Arc<dyn MemoryPort>,
        assoc: Arc<dyn AssocPort>,
        pool: Option<Arc<dyn PoolPort>>,
        tools: Option<Arc<dyn ToolPort>>,
        stickers: Option<Arc<StickerCatalog>>,
        media: Option<Arc<MediaLibrary>>,
        config: KernelConfig,
    ) -> Self {
        Kernel {
            session,
            llm,
            memory,
            assoc,
            pool,
            tools,
            stickers,
            media,
            config,
            pending_reminders: Vec::new(),
            last_audit: Audit::default(),
        }
    }

    // ------------------------------------------------------------- 访问器

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    pub fn card(&self) -> &CharacterCard {
        &self.session.card
    }

    pub fn scene(&self) -> &Scene {
        &self.session.scene
    }

    /// 直接改场景（外部剧情驱动）。
    pub fn set_scene(&mut self, scene: Scene) {
        self.session.scene = scene;
    }

    pub fn state(&self) -> &crate::state::DynamicState {
        &self.session.state
    }

    pub fn config(&self) -> &KernelConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut KernelConfig {
        &mut self.config
    }

    pub fn last_audit(&self) -> &Audit {
        &self.last_audit
    }

    /// 工具端口（若启用）。
    pub fn tools(&self) -> Option<&Arc<dyn ToolPort>> {
        self.tools.as_ref()
    }

    /// 表情包目录（若加载）。
    pub fn stickers(&self) -> Option<&Arc<StickerCatalog>> {
        self.stickers.as_ref()
    }

    /// 图片素材库（若加载）。
    pub fn media(&self) -> Option<&Arc<MediaLibrary>> {
        self.media.as_ref()
    }

    /// 换一整套角色卡与场景（用于前端"切换人物"）。
    ///
    /// 刻意**保留**会话状态与事件历史：切换人物是"换一个人来演这一场"，
    /// 不是"重开一局"。要重开请用 [`Kernel::reset`]。
    pub fn set_card(&mut self, card: CharacterCard) {
        self.session.card = card;
    }

    /// 换一套素材库（切人物时通常要跟着换）。
    pub fn set_stickers(&mut self, stickers: Option<Arc<StickerCatalog>>) {
        self.stickers = stickers;
    }

    /// 换一个图片素材库。
    pub fn set_media(&mut self, media: Option<Arc<MediaLibrary>>) {
        self.media = media;
    }

    /// 清空事件历史、状态与回合计数（保留角色卡与场景）。
    pub fn reset(&mut self) {
        self.session.transcript.clear();
        self.session.state = crate::state::DynamicState::default();
        self.pending_reminders.clear();
        self.last_audit = Audit::default();
        self.session.push(EventKind::System, "system", "（这一场戏重新开始）");
    }

    /// 后端状态。
    pub fn status(&self) -> KernelStatus {
        let mut degraded = Vec::new();
        if !self.llm.health() {
            degraded.push("语言模型不可用".to_string());
        }
        if !self.memory.health() {
            degraded.push("长期记忆降级到内存实现".to_string());
        }
        if !self.assoc.health() {
            degraded.push("联想降级到共现图实现".to_string());
        }
        match &self.pool {
            Some(p) if !p.health() => degraded.push("共享记忆池不可用".to_string()),
            None => {}
            _ => {}
        }
        KernelStatus {
            llm: self.llm.name().to_string(),
            llm_endpoints: self.llm.endpoint_count(),
            memory: self.memory.status(),
            assoc: self.assoc.status(),
            pool: self.pool.as_ref().map(|p| p.status()),
            tools: self.tools.as_ref().map(|t| t.status()),
            stickers: self
                .stickers
                .as_ref()
                .map(|c| format!("{} 张可用", c.len())),
            media: self.media.as_ref().map(|m| format!("{} 张可用", m.len())),
            turn: self.session.state.turn,
            transcript: self.session.transcript.len(),
            degraded,
        }
    }

    // --------------------------------------------------------------- 主要

    /// 执行一个回合。
    ///
    /// 顺序（注意提示词是在**用户输入入账之前**装配的，避免重复注入）：
    /// 召回 → 联想 → 装配 → 生成（含协议循环）→ 解析 → 守护 → 结算 → 落库
    pub fn turn(&mut self, user_input: &str) -> Result<TurnOutcome> {
        self.turn_inner(user_input, Incoming::None)
    }

    /// 对方发来一张表情包，作为一个回合。
    ///
    /// 表情包不是"文本的装饰"，而是一次**带情绪的输入**：内核先把它翻译成
    /// 一句角色看得懂的话（见 [`StickerCatalog::describe_incoming`]），
    /// 再按 `sticker_affect` 给状态机一个很小的倾向，然后走完全一样的回合流程。
    ///
    /// 之所以不让上层自己拼一句字符串再调 [`Kernel::turn`]：那样没有人负责
    /// 把编号写进事件的 `meta`，前端就分不清"他说了一句关于开心的话"
    /// 和"他发了一张开心的图"。
    pub fn send_sticker(&mut self, id: &str) -> Result<TurnOutcome> {
        let Some(cat) = self.stickers.clone() else {
            return Err(StyxError::unavailable(
                "sticker",
                "这个角色没有加载表情包目录（用 --stickers 指定一个目录）",
            ));
        };
        let sticker = cat
            .find(id)
            .ok_or_else(|| StyxError::Other(format!("没有这张表情包：{id}")))?
            .clone();
        let text = cat
            .describe_incoming(&self.config.user_name, &sticker.id)
            .ok_or_else(|| StyxError::Other(format!("没有这张表情包：{id}")))?;
        self.turn_inner(&text, Incoming::Sticker(&sticker))
    }

    /// 对方发来一张图片 / 照片，作为一个回合。
    ///
    /// 与表情包的关键差别是**不做情绪传染**：一张老照片本身不带情绪，
    /// 带情绪的是"看到它"这件事，而那应该由模型决定并写进 `[情]`。
    /// 内核只负责把图转述成一句角色看得懂的话（尺寸、主色、画面感觉由
    /// `styx-vision` 算好后存在 [`Photo::facts`] 里）。
    ///
    /// `note` 是用户随图附的一句话，会接在转述后面。
    pub fn show_image(&mut self, id: &str, note: Option<&str>) -> Result<TurnOutcome> {
        let Some(lib) = self.media.clone() else {
            return Err(StyxError::unavailable(
                "media",
                "这个角色没有加载图片素材库（用 --photos 指定一个目录）",
            ));
        };
        let photo = lib
            .find(id)
            .ok_or_else(|| StyxError::Other(format!("没有这张图片：{id}")))?;
        let mut text = lib
            .describe_incoming(&self.config.user_name, &photo.id)
            .ok_or_else(|| StyxError::Other(format!("没有这张图片：{id}")))?;
        if let Some(note) = note {
            let note = note.trim();
            if !note.is_empty() {
                text.push_str(&format!("，并说：{note}"));
            }
        }
        self.turn_inner(&text, Incoming::Image(&photo))
    }

    /// 回合主体。`incoming` 说明这一轮的"用户输入"是怎么来的。
    fn turn_inner(&mut self, user_input: &str, incoming: Incoming<'_>) -> Result<TurnOutcome> {
        if user_input.trim().is_empty() {
            return Err(StyxError::Other("用户输入为空".into()));
        }
        let mut notices: Vec<String> = Vec::new();

        // ── 1) 召回长期记忆 ───────────────────────────────────────────
        let recalled = match self.memory.recall(user_input, self.config.memory_recall) {
            Ok(v) => v,
            Err(e) => {
                notices.push(format!("长期记忆召回失败（{e}），本回合无记忆可用"));
                Vec::new()
            }
        };

        // ── 2) 联想发散 ──────────────────────────────────────────────
        let associations = if self.config.use_assoc {
            match self.gather_associations(user_input) {
                Ok(v) => v,
                Err(e) => {
                    notices.push(format!("联想失败（{e}），本回合无联想可用"));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        // ── 3) 共享记忆池 ────────────────────────────────────────────
        let pool_notes = if self.config.use_pool {
            match &self.pool {
                Some(p) => match p.recall(user_input, self.config.pool_recall) {
                    Ok(v) => v,
                    Err(e) => {
                        notices.push(format!("共享记忆池读取失败（{e}）"));
                        Vec::new()
                    }
                },
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

        // ── 4) 装配提示词 ────────────────────────────────────────────
        // 注意：此时用户输入还未入账，因此 plan.messages 末尾追加的 user 消息
        // 恰好就是本轮输入，不会与历史重复。
        let mut reminders = self.pending_reminders.clone();
        self.pending_reminders.clear();
        if !self.last_audit.warnings.is_empty() {
            reminders.extend(self.last_audit.warnings.clone());
        }
        let mut all_notices = notices.clone();
        all_notices.extend(reminders.clone());

        let plan = {
            let history: &[Event] = &self.session.transcript;
            let builder = PromptBuilder {
                card: &self.session.card,
                scene: &self.session.scene,
                state: &self.session.state,
                budget: &self.config.budget,
                history,
                user_role: &self.config.user_name,
                user_input,
                stickers: self.stickers.as_deref(),
                sticker_cap: self.config.sticker_options,
                media: self.media.as_deref(),
                media_cap: self.config.media_options,
            };
            builder.build(&recalled, &associations, &pool_notes, &all_notices)
        };
        if plan.report.over_budget {
            notices.push("提示词超出预算".into());
        }

        // 用户输入入账（在提示词装配之后，保证历史不重复）
        let mut input_event =
            Event::new(EventKind::UserInput, self.config.user_name.clone(), user_input);
        match incoming {
            // 编号进 meta：前端靠它把气泡渲染成图片，而不是一句转述
            Incoming::Sticker(st) => {
                input_event = input_event
                    .with_meta("sticker_id", st.id.clone())
                    .with_meta("sticker_label", st.label.clone());
            }
            Incoming::Image(p) => {
                input_event = input_event
                    .with_meta("image_id", p.id.clone())
                    .with_meta("image_file", p.file.clone());
            }
            Incoming::None => {}
        }
        self.session.push_event(input_event);

        // ── 5) 生成（含文本约定协议的两阶段） + 一致性重试 ───────────
        let (mut reply, mut completion, mut directives, mut protocol_hops) =
            self.generate_with_protocol(&plan.messages, &self.config.llm, &mut notices)?;

        // ── 5.5) 素材裁决 ────────────────────────────────────────────
        // 必须在一致性审计之前：一个"只发了一张表情、一句话没说"的回复
        // 是合法的表达，不该被 Guard 判成"缺少台词"而重试。
        let mut sent_stickers = self.resolve_stickers(&mut reply, &mut notices);
        let mut sent_images = self.resolve_images(&mut reply, &mut notices);

        let mut audit = Guard::audit(&self.session.card, &reply);
        let mut retries = 0usize;
        while audit.needs_retry() && retries < self.config.guard_retries {
            retries += 1;
            let mut msgs = plan.messages.clone();
            msgs.push(ChatMessage::user(audit.retry_instruction()));
            // 重试时略微降温，减少再次踩同一个坑的概率
            let mut retry_opts = self.config.llm.clone();
            if let Some(t) = retry_opts.temperature {
                retry_opts.temperature = Some((t - 0.2).max(0.1));
            }
            match self.generate_with_protocol(&msgs, &retry_opts, &mut notices) {
                Ok((mut new_reply, new_completion, new_directives, new_hops)) => {
                    // 重试结果也要过一遍素材裁决，否则会绕过
                    // "不存在的编号"和"一回合最多一张"这两道闸
                    let new_sent = self.resolve_stickers(&mut new_reply, &mut notices);
                    let new_images = self.resolve_images(&mut new_reply, &mut notices);
                    let new_audit = Guard::audit(&self.session.card, &new_reply);
                    // 采纳重试结果：违规项变少就换
                    if new_audit.violations.len() < audit.violations.len() {
                        reply = new_reply;
                        completion = new_completion;
                        directives = new_directives;
                        protocol_hops = new_hops;
                        sent_stickers = new_sent;
                        sent_images = new_images;
                        audit = new_audit;
                    }
                }
                Err(e) => {
                    notices.push(format!("一致性重试失败（{e}），沿用首次输出"));
                    break;
                }
            }
        }
        self.last_audit = audit.clone();

        // ── 6) 记录表演事件 ──────────────────────────────────────────
        let actor = self.session.card.name.clone();
        // 先把"它想起了什么"写进事件流：使用者需要看到角色为什么突然提起旧事，
        // 否则那些台词看起来像是凭空冒出来的。
        for d in &directives {
            self.session.push_event(Event::recall(
                d.kind.as_str(),
                d.query.clone(),
                d.summary.clone(),
            ));
        }
        for a in &reply.actions {
            self.session
                .push(EventKind::Action, actor.clone(), a.clone());
        }
        for s in &reply.speech {
            self.session.push(EventKind::Speech, actor.clone(), s.clone());
        }
        for t in &reply.thoughts {
            self.session
                .push(EventKind::Thought, actor.clone(), t.clone());
        }
        for st in &sent_stickers {
            // text 存中文标签（进提示词），编号进 meta（前端取图）
            self.session.push_event(Event::sticker(
                actor.clone(),
                st.id.clone(),
                st.label.clone(),
            ));
        }
        for (photo, req) in sent_images.iter().zip(reply.images.iter()) {
            let caption = if req.caption.trim().is_empty() {
                photo.caption.clone()
            } else {
                req.caption.clone()
            };
            self.session.push_event(Event::image(
                actor.clone(),
                photo.id.clone(),
                photo.file.clone(),
                caption,
            ));
        }

        // ── 7) 结算状态与场景 ────────────────────────────────────────
        // 对方递过来的情绪只做很小的牵引；先合并再结算，
        // 避免两次 apply 各自夹取一次导致结果依赖调用顺序。
        //
        // 只有表情包传染情绪，图片不传染：一张老照片本身不带情绪，
        // 带情绪的是"看到它"这件事，那该由模型写进 [情] 来决定。
        let mut delta = reply.state_delta.clone();
        if let Incoming::Sticker(st) = incoming {
            delta = delta.merged(&st.affect(self.config.sticker_affect));
        }
        self.session.settle(&delta, self.config.decay_rate);
        let scene_changed = self.session.apply_scene(&reply.scene_set);

        // ── 8) 落库 ─────────────────────────────────────────────────
        let mut memory_written = 0usize;
        let mut pool_written = 0usize;
        if self.config.write_memory {
            match self.persist(&reply) {
                Ok((m, p)) => {
                    memory_written = m;
                    pool_written = p;
                }
                Err(e) => notices.push(format!("写记忆失败：{e}")),
            }
        }

        // 下一轮要带上的提醒
        if let Some(rem) = Guard::reminder(&self.session.card, &audit) {
            self.pending_reminders.push(rem);
        }

        Ok(TurnOutcome {
            turn: self.session.state.turn,
            reply,
            recalled,
            associations,
            pool_notes,
            audit,
            retries,
            memory_written,
            pool_written,
            scene_changed,
            report: plan.report,
            completion,
            notices,
            directives,
            protocol_hops,
            sent_images,
        })
    }

    /// 生成，并在模型申请取资料时执行"取 → 再生成"的协议循环。
    ///
    /// 返回：最终回复、最后一次生成的元信息、执行过的指令、实际跳数。
    ///
    /// **刻意不写事件**：调用方随后还要做一致性重试，只有最终被采纳的那一份
    /// 才该变成对使用者可见的事件。否则一次被否决的生成会留下"角色想起了 X"
    /// 的痕迹，而那句台词根本没出现过——这类幻觉痕迹比丢几条事件难查得多。
    fn generate_with_protocol(
        &self,
        messages: &[ChatMessage],
        opts: &LlmOptions,
        notices: &mut Vec<String>,
    ) -> Result<(Reply, Completion, Vec<DirectiveOutcome>, usize)> {
        let mut convo: Vec<ChatMessage> = messages.to_vec();
        let mut completion = self.llm.complete(&convo, opts)?;
        let mut reply = parse_completion(&completion.text);
        let mut outcomes: Vec<DirectiveOutcome> = Vec::new();
        let mut hops = 0usize;

        while reply.has_directives() && hops < self.config.protocol_hops {
            let batch = self.execute_directives(&reply, notices);
            if batch.is_empty() {
                break;
            }
            hops += 1;
            // 把"申请"和"内核返回"接进消息序列：模型是在知道事实之后才开口的，
            // 这正是两阶段生成与"先编再查"的区别所在。
            convo.push(ChatMessage::assistant(completion.text.clone()));
            convo.push(ChatMessage::user(continuation_prompt(&batch)));
            outcomes.extend(batch);

            completion = match self.llm.complete(&convo, opts) {
                Ok(c) => c,
                Err(e) => {
                    // 取资料之后的续写失败：不能把只剩指令的半成品当回复
                    notices.push(format!("取到资料后的续写失败（{e}），本回合按资料前的输出展示"));
                    break;
                }
            };
            reply = parse_completion(&completion.text);
        }

        if reply.has_directives() && !reply.has_performance() {
            notices.push(format!(
                "模型只输出了查询指令、没有正式表演（协议已用满 {} 跳）",
                self.config.protocol_hops
            ));
        }
        Ok((reply, completion, outcomes, hops))
    }

    /// 执行一批控制指令。
    ///
    /// 三条原则：
    /// - **同一条指令只执行一次**：模型常把 `[回忆] 照片` 写两遍；
    /// - **查不到不是错误**：返回一条 `empty`，让模型老实说"想不起来"；
    /// - **后端报错也不中断回合**：降级成 `failed`，表演照常进行。
    fn execute_directives(&self, reply: &Reply, notices: &mut Vec<String>) -> Vec<DirectiveOutcome> {
        let mut out: Vec<DirectiveOutcome> = Vec::new();
        let mut seen: Vec<(DirectiveKind, String)> = Vec::new();
        for d in &reply.directives {
            if seen.iter().any(|(k, q)| *k == d.kind && *q == d.query) {
                continue;
            }
            seen.push((d.kind, d.query.clone()));
            let limit = d
                .limit
                .unwrap_or(self.config.directive_limit)
                .clamp(1, 20);
            let outcome = match d.kind {
                DirectiveKind::Recall => match self.memory.recall(&d.query, limit) {
                    Ok(list) => {
                        let items: Vec<String> = list
                            .iter()
                            .map(|r| format!("{}（相关度 {:.2}）", r.text, r.score))
                            .collect();
                        if items.is_empty() {
                            DirectiveOutcome::empty(d.kind, &d.query)
                        } else {
                            DirectiveOutcome::hit(d.kind, &d.query, items)
                        }
                    }
                    Err(e) => {
                        notices.push(format!("取记忆失败（{e}）"));
                        DirectiveOutcome::failed(d.kind, &d.query, e.to_string())
                    }
                },
                DirectiveKind::Associate => match self.assoc.associate(&d.query, limit) {
                    Ok(list) => {
                        let items: Vec<String> = list
                            .iter()
                            .map(|a| {
                                if a.evidence.is_empty() {
                                    format!("{}（{:.2}）", a.word, a.score)
                                } else {
                                    format!("{}（{:.2}；{}）", a.word, a.score, a.evidence.join("、"))
                                }
                            })
                            .collect();
                        if items.is_empty() {
                            DirectiveOutcome::empty(d.kind, &d.query)
                        } else {
                            DirectiveOutcome::hit(d.kind, &d.query, items)
                        }
                    }
                    Err(e) => {
                        notices.push(format!("联想失败（{e}）"));
                        DirectiveOutcome::failed(d.kind, &d.query, e.to_string())
                    }
                },
                DirectiveKind::Lore => {
                    let items = self.look_up_card(&d.query, limit);
                    if items.is_empty() {
                        DirectiveOutcome::empty(d.kind, &d.query)
                    } else {
                        DirectiveOutcome::hit(d.kind, &d.query, items)
                    }
                }
            };
            out.push(outcome);
        }
        out
    }

    /// 在自己的设定里翻找一段资料。
    ///
    /// 两级：先按世界书的关键词触发，再退化为"在设定文本里找包含这个词的行"。
    /// 第二级是必要的——世界书是人手写的，而模型的问法千变万化，
    /// 只认关键词会让 `[查阅]` 绝大多数时候空手而归。
    fn look_up_card(&self, query: &str, limit: usize) -> Vec<String> {
        let card = &self.session.card;
        let mut items: Vec<String> = card
            .triggered_lore(query)
            .iter()
            .map(|l| l.text.clone())
            .collect();

        if items.is_empty() {
            let needle = query.trim().to_lowercase();
            if !needle.is_empty() {
                let fields: [(&str, &str); 5] = [
                    ("定位", &card.archetype),
                    ("人格", &card.persona),
                    ("口吻", &card.speech_style),
                    ("背景", &card.background),
                    ("禁则", &card.boundaries.join("；")),
                ];
                for (label, text) in fields {
                    for line in text.lines() {
                        let line = line.trim();
                        if !line.is_empty() && line.to_lowercase().contains(&needle) {
                            items.push(format!("（{label}）{line}"));
                        }
                    }
                }
                for rel in &card.relations {
                    if rel.target.to_lowercase().contains(&needle)
                        || rel.kind.to_lowercase().contains(&needle)
                        || rel.note.to_lowercase().contains(&needle)
                    {
                        items.push(format!(
                            "（关系）{} | {} | {}（好感 {:.2}）",
                            rel.target, rel.kind, rel.note, rel.affinity
                        ));
                    }
                }
            }
        }
        items.dedup();
        items.truncate(limit);
        items
    }

    /// 注入一条系统观察（不触发生成，只进历史与记忆判断）。
    pub fn observe(&mut self, text: impl Into<String>, importance: f32) -> Result<()> {
        let text = text.into();
        self.session.push(EventKind::System, "system", text.clone());
        if self.config.write_memory && importance >= self.config.memory_threshold {
            let note = MemoryNote::new(summarize(&text, 400))
                .with_tags(["观察", self.card().namespace()])
                .with_importance(importance)
                .with_source(format!("styx:{}", self.card().namespace()));
            self.memory.remember(&note)?;
        }
        Ok(())
    }

    /// 手动让角色记住一件事。
    pub fn remember(&self, text: &str, tags: &[String], importance: f32) -> Result<String> {
        let note = MemoryNote::new(summarize(text, 800))
            .with_tags(tags.iter().cloned())
            .with_importance(importance)
            .with_source(format!("styx:{}", self.card().namespace()));
        self.memory.remember(&note)
    }

    /// 只做召回，不生成（用于调试与"角色回忆"面板）。
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Recalled>> {
        self.memory.recall(query, limit)
    }

    /// 让联想后端对一个词发散。
    pub fn associate(&self, seed: &str, limit: usize) -> Result<Vec<Association>> {
        self.assoc.associate(seed, limit)
    }

    // --------------------------------------------------------------- 内部

    /// 把模型给的表情编号，变成"真的能发出去的表情"。
    ///
    /// 三条闸门，都是有意的宽容而非报错：
    ///
    /// - **不认识的编号直接丢掉**并留一条提示。发错一张图不该推翻整段表演，
    ///   所以不触发重试——这也是为什么它排在 Guard 之前、却不算违规；
    /// - **超出 `sticker_limit` 的截断**：连发三张贴图不是表达情绪，是刷屏；
    /// - **没有目录就全部丢掉**：没加载表情包时绝不能凭空发一张不存在的图。
    ///
    /// 返回被采纳的表情（供写事件与前端展示），同时把 `reply.stickers`
    /// 收敛成同一份清单，保证"看到的"和"记下的"是同一件事。
    fn resolve_stickers(&self, reply: &mut Reply, notices: &mut Vec<String>) -> Vec<Sticker> {
        if reply.stickers.is_empty() {
            return Vec::new();
        }
        let Some(cat) = &self.stickers else {
            notices.push(format!(
                "模型想发 {} 张表情包，但当前角色没有加载表情包目录，已全部忽略",
                reply.stickers.len()
            ));
            reply.stickers.clear();
            return Vec::new();
        };

        let limit = self.config.sticker_limit.max(1);
        let mut kept: Vec<Sticker> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        for id in &reply.stickers {
            match cat.find(id) {
                Some(st) => {
                    if kept.len() < limit && !kept.iter().any(|k| k.id == st.id) {
                        kept.push(st.clone());
                    }
                }
                None => unknown.push(id.clone()),
            }
        }
        let overflow = reply
            .stickers
            .len()
            .saturating_sub(kept.len() + unknown.len());
        if !unknown.is_empty() {
            notices.push(format!(
                "模型发了目录里没有的表情包（{}），已忽略",
                unknown.join("、")
            ));
        }
        if overflow > 0 {
            notices.push(format!("一回合最多发 {limit} 张表情包，多余的已忽略"));
        }

        reply.stickers = kept.iter().map(|s| s.id.clone()).collect();
        kept
    }

    /// 把模型给的图片编号，变成"真的能发出去的图片"。
    ///
    /// 与 [`Kernel::resolve_stickers`] 同构，三道闸门也一样宽：
    /// 不认识的编号丢掉并提示、超 `image_limit` 的截断、没素材库就全部丢掉。
    /// 差别只在配文——表情包的说明来自情绪词典，图片的配文来自模型自己写的
    /// `[图片] xxx ｜ 配文`，所以要按请求把它带回来。
    fn resolve_images(&self, reply: &mut Reply, notices: &mut Vec<String>) -> Vec<Photo> {
        if reply.images.is_empty() {
            return Vec::new();
        }
        let Some(lib) = &self.media else {
            notices.push(format!(
                "模型想发 {} 张图片，但当前角色没有加载图片素材库，已全部忽略",
                reply.images.len()
            ));
            reply.images.clear();
            return Vec::new();
        };

        let limit = self.config.image_limit.max(1);
        let mut kept: Vec<Photo> = Vec::new();
        let mut kept_requests: Vec<crate::protocol::ImageRequest> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        for req in &reply.images {
            match lib.find(&req.key) {
                Some(p) => {
                    if kept.len() < limit && !kept.iter().any(|k| k.id == p.id) {
                        kept.push(p);
                        kept_requests.push(req.clone());
                    }
                }
                None => unknown.push(req.key.clone()),
            }
        }
        let overflow = reply
            .images
            .len()
            .saturating_sub(kept.len() + unknown.len());
        if !unknown.is_empty() {
            notices.push(format!(
                "模型发了素材库里没有的图片（{}），已忽略",
                unknown.join("、")
            ));
        }
        if overflow > 0 {
            notices.push(format!("一回合最多发 {limit} 张图片，多余的已忽略"));
        }

        reply.images = kept_requests;
        kept
    }

    /// 从输入里取种子词，逐个发散，再按分数归并去重。
    fn gather_associations(&self, user_input: &str) -> Result<Vec<Association>> {
        let mut seeds = keywords(user_input, self.config.assoc_seeds);
        // 输入太短（例如"嗯"）时用角色当前意图补种子，避免联想总是空转
        if seeds.is_empty() {
            seeds = self
                .session
                .state
                .agenda
                .iter()
                .take(self.config.assoc_seeds)
                .cloned()
                .collect();
        }
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        let mut merged: std::collections::BTreeMap<String, Association> =
            std::collections::BTreeMap::new();
        for seed in &seeds {
            let list = self.assoc.associate(seed, self.config.assoc_limit)?;
            for a in list {
                merged
                    .entry(a.word.clone())
                    .and_modify(|cur| {
                        // 同一联想词被多个种子命中 → 强化
                        cur.score += a.score * 0.5;
                        cur.confidence = cur.confidence.max(a.confidence);
                        for e in &a.evidence {
                            if !cur.evidence.contains(e) && cur.evidence.len() < 3 {
                                cur.evidence.push(e.clone());
                            }
                        }
                    })
                    .or_insert(a);
            }
        }

        // 种子词自身不该出现在联想结果里
        for s in &seeds {
            merged.remove(s);
        }

        let mut out: Vec<Association> = merged.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(self.config.assoc_limit * 2);
        Ok(out)
    }

    /// 把回复与高重要度事件写进长期记忆 / 共享记忆池。
    fn persist(&self, reply: &Reply) -> Result<(usize, usize)> {
        let ns = self.card().namespace();
        let source = format!("styx:{ns}");
        let mut mem_count = 0usize;
        let mut pool_count = 0usize;

        // 1) 模型显式标记的 [忆] 条目
        for note in &reply.memories {
            let mut note = note.clone();
            note.source = source.clone();
            if note.tags.is_empty() {
                note.tags = vec!["角色记忆".into(), ns.to_string()];
            }
            self.memory.remember(&note)?;
            mem_count += 1;

            if self.config.write_pool && note.importance >= self.config.memory_threshold {
                if let Some(p) = &self.pool {
                    if p.remember(&note).is_ok() {
                        pool_count += 1;
                    }
                }
            }
        }

        // 2) 本回合高重要度事件（台词/动作按重要度筛）
        let recent: Vec<crate::event::Event> = self
            .session
            .transcript
            .iter()
            .rev()
            // 一轮之内的事件数量很有限，取最近 16 条足够
            .take(16)
            .filter(|e| {
                matches!(e.kind, EventKind::Speech | EventKind::Action)
                    && e.importance >= self.config.memory_threshold
            })
            .cloned()
            .collect();
        // 默认重要度 0.35~0.4，低于常见门槛，因此这里通常不会触发；
        // 保留这条路径是为了让上层调低门槛时能自动生效。
        for e in recent {
            let text = format!("{}：{}", self.card().name, e.text);
            let note = MemoryNote::new(summarize(&text, 400))
                .with_tags([e.kind.as_str(), ns])
                .with_importance(e.importance)
                .with_source(source.clone());
            self.memory.remember(&note)?;
            mem_count += 1;
        }

        Ok((mem_count, pool_count))
    }
}

impl KernelBuilder {
    /// 注入语言模型端口。
    pub fn llm(mut self, llm: Arc<dyn LlmPort>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// 注入长期记忆端口。
    pub fn memory(mut self, memory: Arc<dyn MemoryPort>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// 注入联想端口。
    pub fn assoc(mut self, assoc: Arc<dyn AssocPort>) -> Self {
        self.assoc = Some(assoc);
        self
    }

    /// 注入共享记忆池端口。
    pub fn pool(mut self, pool: Arc<dyn PoolPort>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// 注入工具端口。
    pub fn tools(mut self, tools: Arc<dyn ToolPort>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// 注入表情包目录。
    pub fn stickers(mut self, stickers: Arc<StickerCatalog>) -> Self {
        self.stickers = Some(stickers);
        self
    }

    /// 装载图片素材库（角色可以 `[图片] <编号>` 发图）。
    pub fn media(mut self, media: Arc<MediaLibrary>) -> Self {
        self.media = Some(media);
        self
    }

    /// 使用已有会话（续演）。
    pub fn session(mut self, session: Session) -> Self {
        self.session = Some(session);
        self
    }

    /// 设置配置。
    pub fn config(mut self, config: KernelConfig) -> Self {
        self.config = config;
        self
    }

    /// 构建。
    pub fn build(self) -> Result<Kernel> {
        self.card.validate()?;
        let llm = self
            .llm
            .ok_or_else(|| StyxError::unavailable("llm", "构建内核时必须提供语言模型端口"))?;
        let memory = self
            .memory
            .ok_or_else(|| StyxError::unavailable("memory", "构建内核时必须提供长期记忆端口"))?;
        let assoc = self
            .assoc
            .ok_or_else(|| StyxError::unavailable("assoc", "构建内核时必须提供联想端口"))?;
        let session = self
            .session
            .unwrap_or_else(|| Session::new(self.card.clone(), self.scene.clone()));
        Ok(Kernel::new(
            session,
            llm,
            memory,
            assoc,
            self.pool,
            self.tools,
            self.stickers,
            self.media,
            self.config,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{Completion, LlmOptions, PoolStats};
    use std::sync::Mutex;

    // ---------------- 测试替身 ----------------

    #[derive(Default)]
    struct FakeLlm {
        replies: Mutex<Vec<String>>,
        calls: Mutex<usize>,
    }

    impl FakeLlm {
        fn new(replies: Vec<&str>) -> Self {
            FakeLlm {
                replies: Mutex::new(replies.into_iter().map(String::from).collect()),
                calls: Mutex::new(0),
            }
        }
        fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl LlmPort for FakeLlm {
        fn name(&self) -> &str {
            "fake"
        }
        fn complete(&self, _m: &[ChatMessage], _o: &LlmOptions) -> Result<Completion> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let idx = (*calls - 1).min(self.replies.lock().unwrap().len().saturating_sub(1));
            let text = self
                .replies
                .lock()
                .unwrap()
                .get(idx)
                .cloned()
                .unwrap_or_default();
            Ok(Completion::new(text, "fake-model", "fake-endpoint"))
        }
    }

    #[derive(Default)]
    struct FakeMemory {
        notes: Mutex<Vec<MemoryNote>>,
        hits: Mutex<Vec<Recalled>>,
        fail: bool,
    }

    impl MemoryPort for FakeMemory {
        fn name(&self) -> &str {
            "fake-memory"
        }
        fn health(&self) -> bool {
            !self.fail
        }
        fn remember(&self, n: &MemoryNote) -> Result<String> {
            if self.fail {
                return Err(StyxError::Memory("down".into()));
            }
            let mut v = self.notes.lock().unwrap();
            v.push(n.clone());
            Ok(v.len().to_string())
        }
        fn recall(&self, _q: &str, limit: usize) -> Result<Vec<Recalled>> {
            if self.fail {
                return Err(StyxError::Memory("down".into()));
            }
            let h = self.hits.lock().unwrap();
            Ok(h.iter().take(limit).cloned().collect())
        }
        fn related(&self, _id: &str, _limit: usize) -> Result<Vec<Recalled>> {
            Ok(Vec::new())
        }
        fn forget(&self, _id: &str) -> Result<bool> {
            Ok(true)
        }
    }

    #[derive(Default)]
    struct FakeAssoc {
        map: std::collections::BTreeMap<String, Vec<Association>>,
    }

    impl AssocPort for FakeAssoc {
        fn name(&self) -> &str {
            "fake-assoc"
        }
        fn associate(&self, seed: &str, limit: usize) -> Result<Vec<Association>> {
            Ok(self
                .map
                .get(seed)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(limit)
                .collect())
        }
    }

    struct FakePool {
        items: Mutex<Vec<MemoryNote>>,
    }
    impl PoolPort for FakePool {
        fn name(&self) -> &str {
            "fake-pool"
        }
        fn remember(&self, n: &MemoryNote) -> Result<String> {
            self.items.lock().unwrap().push(n.clone());
            Ok("1".into())
        }
        fn recall(&self, _q: &str, _l: usize) -> Result<Vec<Recalled>> {
            Ok(vec![Recalled::new("p1", "陈默昨天在城南见过林夏", 0.5, "pool")])
        }
        fn stats(&self) -> Result<PoolStats> {
            Ok(PoolStats {
                count: self.items.lock().unwrap().len(),
                detail: "fake".into(),
            })
        }
    }

    fn card() -> CharacterCard {
        CharacterCard::parse_markdown(
            r#"
# 林夏

## 定位
外冷内热的旧书店主

## 口吻
短句。

## 示例台词
- 不卖。

## 禁则
- 绝不承认自己害怕孤独

## 禁用词
- 绝绝子
"#,
        )
        .unwrap()
    }

    fn kernel_with(replies: Vec<&str>) -> (Kernel, Arc<FakeLlm>, Arc<FakeMemory>, Arc<FakeAssoc>) {
        let llm = Arc::new(FakeLlm::new(replies));
        let memory = Arc::new(FakeMemory::default());
        let mut assoc = FakeAssoc::default();
        assoc.map.insert(
            "相册".to_string(),
            vec![Association::new("照片", 0.9).with_confidence(0.95)],
        );
        let assoc = Arc::new(assoc);
        let cfg = KernelConfig {
            write_pool: true,
            ..Default::default()
        };
        let kernel = Kernel::builder(card(), Scene::new("拾光旧书店"))
            .llm(llm.clone())
            .memory(memory.clone())
            .assoc(assoc.clone())
            .pool(Arc::new(FakePool {
                items: Mutex::new(Vec::new()),
            }))
            .config(cfg)
            .build()
            .unwrap();
        // FakeMemory 在 kernel 内部是 Arc<dyn>，这里把具体类型再传回去做断言
        (kernel, llm, memory, assoc)
    }

    /// 带表情包目录的内核。
    fn kernel_with_catalog(replies: Vec<&str>, cfg: KernelConfig) -> (Kernel, Arc<FakeLlm>) {
        let llm = Arc::new(FakeLlm::new(replies));
        let catalog = Arc::new(StickerCatalog::from_files(&[
            "happy_01.png",
            "cry_03.png",
            "wailing_22.png",
        ]));
        let kernel = Kernel::builder(card(), Scene::new("拾光旧书店"))
            .llm(llm.clone())
            .memory(Arc::new(FakeMemory::default()))
            .assoc(Arc::new(FakeAssoc::default()))
            .stickers(catalog)
            .config(cfg)
            .build()
            .unwrap();
        (kernel, llm)
    }

    #[test]
    fn turn_produces_reply_state_and_persistence() {
        let raw = r#"[做] 她把相册合上
[说] 不卖。
[情] 心情=戒备 效价=-0.1 紧张=+0.2
[关系] 陈默 好感=-0.1
[忆] 陈默想看母亲的相册 | 标签=相册,母亲 | 重要度=0.9
[场] 时间=深夜"#;
        let (mut k, llm, mem, _assoc) = kernel_with(vec![raw]);
        let out = k.turn("我想看看那本相册。").unwrap();

        assert_eq!(llm.call_count(), 1);
        assert_eq!(out.reply.speech, vec!["不卖。".to_string()]);
        assert_eq!(out.reply.actions.len(), 1);
        assert_eq!(out.turn, 1);
        // 状态结算
        assert_eq!(k.state().mood.label, "戒备");
        assert!(k.state().affinity_to("陈默") < 0.0);
        // 场景
        assert_eq!(k.scene().time, "深夜");
        assert_eq!(out.scene_changed.len(), 1);
        // 落库：1 条 [忆] + 共享池 1 条
        let notes = mem.notes.lock().unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].text.contains("相册"));
        assert_eq!(out.memory_written, 1);
        assert_eq!(out.pool_written, 1);
        // 联想被喂进提示词
        assert!(out.associations.iter().any(|a| a.word == "照片"));
        assert!(out.report.associations_used >= 1);
        assert!(!out.audit.needs_retry());
        assert_eq!(out.retries, 0);
    }

    #[test]
    fn user_input_is_not_duplicated_in_history() {
        let (mut k, _llm, _mem, _assoc) = kernel_with(vec![
            "[说] 不卖。",
            "[说] 第二句。",
        ]);
        k.turn("第一句输入").unwrap();
        let out = k.turn("第二句输入").unwrap();
        // 历史里用户输入只应出现一次
        let hist: String = k
            .session()
            .transcript
            .iter()
            .filter(|e| e.kind == EventKind::UserInput)
            .map(|e| e.text.clone())
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(hist, "第一句输入|第二句输入");
        assert_eq!(out.turn, 2);
    }

    #[test]
    fn guard_triggers_one_retry_and_adopts_better_output() {
        let bad = "[说] 绝绝子，这也太好吃了吧";
        let good = "[说] 不卖。";
        let (mut k, llm, _mem, _assoc) = kernel_with(vec![bad, good]);
        let out = k.turn("这家店怎么样？").unwrap();
        assert_eq!(llm.call_count(), 2, "应当重试一次");
        assert_eq!(out.retries, 1);
        assert_eq!(out.reply.speech, vec!["不卖。".to_string()]);
        assert!(!out.audit.needs_retry());
    }

    #[test]
    fn guard_keeps_first_output_when_retry_is_no_better() {
        let bad = "[说] 绝绝子。";
        let (mut k, llm, _mem, _assoc) = kernel_with(vec![bad, bad]);
        let out = k.turn("在吗").unwrap();
        assert_eq!(llm.call_count(), 2);
        assert_eq!(out.retries, 1);
        // 两次都违规：保留首次，违规项数量不变，不会无限重试
        assert!(out.audit.needs_retry());
        assert!(out.reply.speech[0].contains("绝绝子"));
    }

    #[test]
    fn degraded_memory_does_not_break_the_turn() {
        let llm = Arc::new(FakeLlm::new(vec!["[说] 嗯。"]));
        let memory = Arc::new(FakeMemory {
            fail: true,
            ..Default::default()
        });
        let mut k = Kernel::builder(card(), Scene::new("书店"))
            .llm(llm)
            .memory(memory)
            .assoc(Arc::new(FakeAssoc::default()))
            .config(KernelConfig::default())
            .build()
            .unwrap();
        let out = k.turn("在吗").unwrap();
        assert!(out.recalled.is_empty());
        assert!(out.notices.iter().any(|n| n.contains("长期记忆召回失败")));
        assert!(!k.status().degraded.is_empty());
        // 写记忆也会失败，但只是提示，不该让回合失败
        assert!(out.notices.iter().any(|n| n.contains("写记忆失败")) || out.memory_written == 0);
    }

    #[test]
    fn assoc_failure_is_non_fatal() {
        struct BrokenAssoc;
        impl AssocPort for BrokenAssoc {
            fn name(&self) -> &str {
                "broken"
            }
            fn associate(&self, _s: &str, _l: usize) -> Result<Vec<Association>> {
                Err(StyxError::Assoc("timeout".into()))
            }
        }
        let mut k = Kernel::builder(card(), Scene::new("书店"))
            .llm(Arc::new(FakeLlm::new(vec!["[说] 嗯。"])))
            .memory(Arc::new(FakeMemory::default()))
            .assoc(Arc::new(BrokenAssoc))
            .config(KernelConfig::default())
            .build()
            .unwrap();
        let out = k.turn("旧照片").unwrap();
        assert!(out.associations.is_empty());
        assert!(out.notices.iter().any(|n| n.contains("联想失败")));
    }

    #[test]
    fn pool_recall_feeds_prompt() {
        let (mut k, _llm, _mem, _assoc) = kernel_with(vec!["[说] 嗯。"]);
        let out = k.turn("昨天有人见过她吗").unwrap();
        assert_eq!(out.pool_notes.len(), 1);
        assert!(out.report.pool_used >= 1);
    }

    #[test]
    fn empty_input_is_rejected() {
        let (mut k, _llm, _mem, _assoc) = kernel_with(vec!["[说] 嗯。"]);
        assert!(k.turn("   ").is_err());
    }

    #[test]
    fn observe_writes_memory_above_threshold() {
        let (mut k, _llm, mem, _assoc) = kernel_with(vec!["[说] 嗯。"]);
        k.observe("陈默离开了这座城", 0.9).unwrap();
        assert_eq!(mem.notes.lock().unwrap().len(), 1);
        k.observe("街角有人经过", 0.1).unwrap();
        assert_eq!(mem.notes.lock().unwrap().len(), 1, "低重要度不该落库");
    }

    #[test]
    fn status_reports_all_ports() {
        let (k, _llm, _mem, _assoc) = kernel_with(vec!["[说] 嗯。"]);
        let s = k.status();
        assert_eq!(s.llm, "fake");
        assert_eq!(s.llm_endpoints, 1);
        assert!(s.pool.is_some());
        assert!(s.degraded.is_empty());
        assert!(s.render().contains("长期记忆"));
    }

    #[test]
    fn retry_instruction_is_passed_to_second_call() {
        // 用一个记录所有请求的假模型验证"重试指令确实发出去了"
        #[derive(Default)]
        struct RecordingLlm {
            seen: Mutex<Vec<String>>,
        }
        impl LlmPort for RecordingLlm {
            fn complete(&self, m: &[ChatMessage], _o: &LlmOptions) -> Result<Completion> {
                self.seen
                    .lock()
                    .unwrap()
                    .push(m.last().map(|x| x.content.clone()).unwrap_or_default());
                Ok(Completion::new("[说] 绝绝子。", "m", "e"))
            }
        }
        let rec = Arc::new(RecordingLlm::default());
        let mut k = Kernel::builder(card(), Scene::new("书店"))
            .llm(rec.clone())
            .memory(Arc::new(FakeMemory::default()))
            .assoc(Arc::new(FakeAssoc::default()))
            .build()
            .unwrap();
        let _ = k.turn("在吗").unwrap();
        let seen = rec.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].contains("请重写"));
        assert!(seen[1].contains("绝绝子"));
    }

    #[test]
    fn builder_requires_core_ports() {
        let err = Kernel::builder(card(), Scene::new("x")).build();
        assert!(err.is_err());
        let e = err.err().unwrap();
        assert!(e.to_string().contains("语言模型"));
    }

    #[test]
    fn builder_rejects_invalid_card() {
        let mut bad = CharacterCard::new("无名");
        bad.persona = String::new();
        let built = Kernel::builder(bad, Scene::new("x"))
            .llm(Arc::new(FakeLlm::new(vec!["x"])))
            .memory(Arc::new(FakeMemory::default()))
            .assoc(Arc::new(FakeAssoc::default()))
            .build();
        let Err(err) = built else {
            panic!("非法角色卡不该构建成功");
        };
        assert!(err.to_string().contains("角色卡非法"));
    }

    #[test]
    fn kernel_can_resume_from_a_session_snapshot() {
        let (mut k, _llm, _mem, _assoc) = kernel_with(vec!["[说] 嗯。"]);
        k.turn("第一句").unwrap();
        let json = k.session().snapshot_json();

        let restored = Session::from_snapshot_json(&json).unwrap();
        let mut k2 = Kernel::builder(card(), Scene::new("x"))
            .llm(Arc::new(FakeLlm::new(vec!["[说] 继续。"])))
            .memory(Arc::new(FakeMemory::default()))
            .assoc(Arc::new(FakeAssoc::default()))
            .session(restored)
            .build()
            .unwrap();
        let out = k2.turn("第二句").unwrap();
        assert_eq!(out.turn, 2);
        assert_eq!(k2.session().transcript.len(), 4);
    }

    // ------------------------------------------------------------ 表情包

    #[test]
    fn sending_a_sticker_makes_a_turn_with_meta_and_a_small_mood_nudge() {
        let (mut k, _llm) = kernel_with_catalog(vec!["[说] 别闹。"], KernelConfig::default());
        let before = k.state().mood.valence;

        let out = k.send_sticker("happy_01").unwrap();
        assert_eq!(out.turn, 1);

        // 用户输入被翻译成一句人话，并且带上编号供前端取图
        let input = k
            .session()
            .transcript
            .iter()
            .find(|e| e.kind == EventKind::UserInput)
            .expect("应当有一条用户输入事件");
        assert_eq!(input.sticker_id(), Some("happy_01"));
        assert!(input.text.contains("发来一张表情包"), "{}", input.text);
        assert!(input.text.contains("开心"), "{}", input.text);

        // 情绪传染：幅度很小，但方向对
        let after = k.state().mood.valence;
        assert!(after > before, "{before} → {after}（对方发来开心的表情）");
        assert!(after - before < 0.2, "传染必须是小幅的：{}", after - before);
    }

    #[test]
    fn zero_affect_means_the_character_decides_on_their_own() {
        let cfg = KernelConfig {
            sticker_affect: 0.0,
            ..Default::default()
        };
        let (mut k, _llm) = kernel_with_catalog(vec!["[说] 哦。"], cfg);
        let before = k.state().mood.valence;
        k.send_sticker("wailing_22").unwrap();
        assert_eq!(
            k.state().mood.valence,
            before,
            "关掉传染后，对方大哭也不该改角色自己的心情"
        );
    }

    #[test]
    fn an_unknown_sticker_id_is_a_hard_error_on_the_way_in() {
        let (mut k, _llm) = kernel_with_catalog(vec!["[说] 哦。"], KernelConfig::default());
        let err = k.send_sticker("nope_99").unwrap_err();
        assert!(err.to_string().contains("没有这张表情包"), "{err}");
        assert_eq!(k.state().turn, 0, "失败的调用不该推进回合");
    }

    #[test]
    fn without_a_catalog_sending_a_sticker_explains_why() {
        let (mut k, _llm, _mem, _assoc) = kernel_with(vec!["[说] 哦。"]);
        let err = k.send_sticker("happy_01").unwrap_err();
        assert!(err.to_string().contains("表情包目录"), "{err}");
    }

    #[test]
    fn sticker_from_the_model_becomes_an_event_with_its_label() {
        let (mut k, _llm) = kernel_with_catalog(
            vec!["[表情] cry_03\n[说] 别哭。"],
            KernelConfig::default(),
        );
        let out = k.turn("我可能要走了。").unwrap();
        assert_eq!(out.reply.stickers, vec!["cry_03".to_string()]);
        assert_eq!(out.retries, 0);

        let st = k
            .session()
            .transcript
            .iter()
            .find(|e| e.kind == EventKind::Sticker)
            .expect("应当有一条表情包事件");
        assert_eq!(st.sticker_id(), Some("cry_03"));
        assert_eq!(st.text, "哭泣", "进提示词的是人话");
        assert_eq!(st.actor, "林夏");
    }

    #[test]
    fn a_sticker_only_reply_is_not_retried() {
        // 有些时刻一张图就够了，Guard 不该逼出一句台词
        let (mut k, llm) = kernel_with_catalog(vec!["[表情] wailing_22"], KernelConfig::default());
        let out = k.turn("我真的要走了。").unwrap();
        assert_eq!(llm.call_count(), 1, "不该因为没说话而重试");
        assert_eq!(out.retries, 0);
        assert!(out.reply.speech.is_empty());
        assert_eq!(out.reply.stickers.len(), 1);
    }

    #[test]
    fn invented_sticker_ids_are_dropped_with_a_notice() {
        let (mut k, _llm) = kernel_with_catalog(
            vec!["[表情] 大概是一张开心的图\n[说] 嗯。"],
            KernelConfig::default(),
        );
        let out = k.turn("在吗").unwrap();
        assert!(out.reply.stickers.is_empty());
        assert!(
            out.notices.iter().any(|n| n.contains("没有的表情包")),
            "{:?}",
            out.notices
        );
        assert!(!k
            .session()
            .transcript
            .iter()
            .any(|e| e.kind == EventKind::Sticker));
    }

    #[test]
    fn sticker_limit_keeps_a_turn_from_becoming_a_wall_of_images() {
        let cfg = KernelConfig {
            sticker_limit: 1,
            ..Default::default()
        };
        let (mut k, _llm) =
            kernel_with_catalog(vec!["[表情] happy_01\n[表情] cry_03\n[说] 嗯。"], cfg);
        let out = k.turn("在吗").unwrap();
        assert_eq!(out.reply.stickers, vec!["happy_01".to_string()]);
        assert!(
            out.notices.iter().any(|n| n.contains("最多发 1 张")),
            "{:?}",
            out.notices
        );
        let sticker_events = k
            .session()
            .transcript
            .iter()
            .filter(|e| e.kind == EventKind::Sticker)
            .count();
        assert_eq!(sticker_events, 1);
    }

    #[test]
    fn a_model_reply_with_stickers_is_ignored_when_no_catalog_is_loaded() {
        // 没加载目录却让"模型发图"，等于让前端去加载一张不存在的文件
        let (mut k, _llm, _mem, _assoc) = kernel_with(vec!["[表情] happy_01\n[说] 嗯。"]);
        let out = k.turn("在吗").unwrap();
        assert!(out.reply.stickers.is_empty());
        assert!(
            out.notices.iter().any(|n| n.contains("没有加载表情包目录")),
            "{:?}",
            out.notices
        );
    }

    #[test]
    fn sticker_catalog_reaches_the_prompt() {
        let (mut k, _llm) = kernel_with_catalog(vec!["[说] 嗯。"], KernelConfig::default());
        let out = k.turn("在吗").unwrap();
        assert_eq!(out.report.sticker_options, 3);
        assert!(out.report.render().contains("表情 3张"));
        assert!(k.status().stickers.as_deref().unwrap().contains("3 张"));
    }

    #[test]
    fn sticker_history_keeps_the_id_in_meta() {
        // 表情事件进了历史，下一轮的提示词里就能看到"我上轮发过一张大哭的图"
        let (mut k, _llm) = kernel_with_catalog(
            vec!["[表情] wailing_22\n[说] 呜。", "[说] 我没事。"],
            KernelConfig::default(),
        );
        k.turn("别哭啊。").unwrap();
        let out = k.turn("真的没事吗？").unwrap();
        assert!(out.report.history_events >= 2);
        let rendered: String = k
            .session()
            .transcript
            .iter()
            .map(|e| e.render_line())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("发来一张表情包：大哭"), "{rendered}");
    }

    // -------------------------------------------------- 文本约定协议：控制通道

    /// 带记忆命中、表情包目录与图片素材库的内核。
    fn kernel_rich(
        replies: Vec<&str>,
        cfg: KernelConfig,
        hits: Vec<Recalled>,
        sticker_files: &[&str],
        photo_files: &[&str],
    ) -> (Kernel, Arc<FakeLlm>, Arc<FakeMemory>) {
        let llm = Arc::new(FakeLlm::new(replies));
        let memory = Arc::new(FakeMemory {
            hits: Mutex::new(hits),
            ..Default::default()
        });
        let mut builder = Kernel::builder(card(), Scene::new("拾光旧书店"))
            .llm(llm.clone())
            .memory(memory.clone())
            .assoc(Arc::new(FakeAssoc::default()))
            .config(cfg);
        if !sticker_files.is_empty() {
            builder = builder.stickers(Arc::new(StickerCatalog::from_files(sticker_files)));
        }
        if !photo_files.is_empty() {
            builder = builder.media(Arc::new(MediaLibrary::from_files(photo_files)));
        }
        (builder.build().unwrap(), llm, memory)
    }

    #[test]
    fn a_recall_directive_causes_a_second_generation() {
        let (mut k, llm, _mem) = kernel_rich(
            vec!["[回忆] 母亲的照片", "[说] 那本相册最下面那层。"],
            KernelConfig::default(),
            vec![Recalled::new(
                "m1",
                "母亲留下的旧照片放在相册最下层",
                0.8,
                "memory",
            )],
            &[],
            &[],
        );
        let out = k.turn("你还记得那张照片吗？").unwrap();

        // 一次"申请" + 一次"正式表演"：这正是两阶段生成的证据
        assert_eq!(llm.call_count(), 2, "应当重新生成一次");
        assert_eq!(out.protocol_hops, 1);
        assert_eq!(out.retries, 0, "取资料不是违规，不该触发一致性重试");

        assert_eq!(out.directives.len(), 1);
        assert_eq!(out.directives[0].kind, DirectiveKind::Recall);
        assert_eq!(out.directives[0].query, "母亲的照片");
        assert!(out.directives[0].has_items());
        assert!(out.directives[0].summary.contains("1 条"));

        // 阶段 1 的表演被丢掉，最终只有阶段 2 的台词
        assert_eq!(out.reply.speech, vec!["那本相册最下面那层。".to_string()]);
    }

    #[test]
    fn the_recall_becomes_a_visible_event() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[回忆] 照片", "[说] 嗯。"],
            KernelConfig::default(),
            vec![Recalled::new("m1", "相册里有母亲的旧照片", 0.7, "memory")],
            &[],
            &[],
        );
        k.turn("那照片呢？").unwrap();

        let ev = k
            .session()
            .transcript
            .iter()
            .find(|e| e.kind == EventKind::Recall)
            .expect("应当有一条取资料事件");
        assert_eq!(ev.actor, "kernel");
        assert_eq!(ev.meta.get("directive").map(String::as_str), Some("recall"));
        assert_eq!(ev.meta.get("query").map(String::as_str), Some("照片"));
        assert!(ev.render_line().starts_with("【回忆】"));
    }

    #[test]
    fn the_recalled_material_actually_reaches_the_second_prompt() {
        // 光"多调了一次"不够，资料必须真的进了第二次请求，否则等于白查
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[回忆] 母亲的照片", "[说] 嗯。"],
            KernelConfig::default(),
            vec![Recalled::new("m1", "母亲留下的旧照片", 0.8, "memory")],
            &[],
            &[],
        );
        let out = k.turn("照片呢？").unwrap();
        // 断言方式：续写提示词里写着"内核返回"，而资料原文出现在提示里
        assert!(out.notices.is_empty(), "{:?}", out.notices);
        assert!(out.directives[0].items[0].contains("母亲留下的旧照片"));
    }

    #[test]
    fn an_empty_lookup_is_not_an_error() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[回忆] 火星殖民地", "[说] 我真想不起来。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &[],
        );
        let out = k.turn("火星殖民地怎么样了？").unwrap();
        assert_eq!(out.directives.len(), 1);
        assert!(!out.directives[0].has_items());
        assert!(
            out.directives[0].error.is_none(),
            "查不到是正常结果，不该记成失败"
        );
        assert_eq!(out.reply.speech, vec!["我真想不起来。".to_string()]);
    }

    #[test]
    fn asking_twice_in_one_reply_runs_once() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[回忆] 照片\n[回忆] 照片", "[说] 嗯。"],
            KernelConfig::default(),
            vec![Recalled::new("m1", "旧照片", 0.5, "memory")],
            &[],
            &[],
        );
        let out = k.turn("照片？").unwrap();
        assert_eq!(out.directives.len(), 1, "同一条指令重复发只该执行一次");
    }

    #[test]
    fn association_directive_uses_the_assoc_port() {
        let llm = Arc::new(FakeLlm::new(vec!["[联想] 相册", "[说] 嗯。"]));
        let mut assoc = FakeAssoc::default();
        assoc.map.insert(
            "相册".to_string(),
            vec![Association::new("照片", 0.9)
                .with_confidence(0.95)
                .with_evidence(["她抽屉里那本"])],
        );
        let mut k = Kernel::builder(card(), Scene::new("书店"))
            .llm(llm)
            .memory(Arc::new(FakeMemory::default()))
            .assoc(Arc::new(assoc))
            .build()
            .unwrap();
        let out = k.turn("相册。").unwrap();
        assert_eq!(out.directives.len(), 1);
        assert_eq!(out.directives[0].kind, DirectiveKind::Associate);
        assert!(out.directives[0].has_items());
        // 证据要带上，否则模型没法判断这条联想可不可信
        assert!(out.directives[0].items[0].contains("她抽屉里那本"));
    }

    #[test]
    fn lore_directive_reads_the_character_card() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[查阅] 旧书店", "[说] 嗯。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &[],
        );
        let out = k.turn("这店是什么地方？").unwrap();
        assert_eq!(out.directives[0].kind, DirectiveKind::Lore);
        assert!(
            out.directives[0].has_items(),
            "角色卡里写着「旧书店主」，应当能查出来"
        );
        assert!(out.directives[0].items[0].contains("旧书店"));
    }

    #[test]
    fn zero_hops_disables_the_control_channel_without_breaking_the_turn() {
        let cfg = KernelConfig {
            protocol_hops: 0,
            ..Default::default()
        };
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[回忆] 照片", "[说] 嗯。"],
            cfg,
            vec![Recalled::new("m1", "旧照片", 0.5, "memory")],
            &[],
            &[],
        );
        let out = k.turn("照片？").unwrap();
        assert_eq!(out.protocol_hops, 0);
        assert!(out.directives.is_empty(), "关掉协议就不该执行任何指令");
        // 但回合必须照常完成：Guard 会因为它没说话而重试一次
        assert_eq!(out.reply.speech, vec!["嗯。".to_string()]);
        assert!(
            k.session()
                .transcript
                .iter()
                .all(|e| e.kind != EventKind::Recall),
            "关掉协议后不该留下取资料事件"
        );
    }

    #[test]
    fn a_lookup_never_leaves_the_turn_speechless() {
        // 最坏情况：模型只会发指令、续写也只会发指令 —— 回合仍要有输出
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[回忆] 照片", "[回忆] 照片", "[说] 好吧。"],
            KernelConfig::default(),
            vec![Recalled::new("m1", "旧照片", 0.5, "memory")],
            &[],
            &[],
        );
        let out = k.turn("照片？").unwrap();
        assert_eq!(out.reply.speech, vec!["好吧。".to_string()]);
    }

    // -------------------------------------------------------- 图片素材与发图

    #[test]
    fn a_model_image_becomes_an_event_with_file_and_caption() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[图片] old_photo ｜ 你还记得吗\n[说] 我一直收着。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &["old_photo.png"],
        );
        let out = k.turn("我想看看以前的东西。").unwrap();
        assert_eq!(out.sent_images.len(), 1);
        assert_eq!(out.sent_images[0].id, "old_photo");
        assert_eq!(out.sent_images[0].file, "old_photo.png");

        let ev = k
            .session()
            .transcript
            .iter()
            .find(|e| e.kind == EventKind::Image)
            .expect("应当有一条图片事件");
        assert_eq!(ev.image_id(), Some("old_photo"));
        assert_eq!(ev.image_file(), Some("old_photo.png"));
        assert_eq!(ev.text, "你还记得吗");
        assert_eq!(ev.actor, "林夏");
    }

    #[test]
    fn an_image_without_a_caption_falls_back_to_the_material_description() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[图片] old_photo\n[说] 给你。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &["old_photo.png"],
        );
        k.turn("看看。").unwrap();
        let ev = k
            .session()
            .transcript
            .iter()
            .find(|e| e.kind == EventKind::Image)
            .unwrap();
        assert_eq!(ev.text, "old photo");
    }

    #[test]
    fn an_unknown_image_is_dropped_with_a_notice() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[图片] nope.png\n[说] 嗯。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &["old_photo.png"],
        );
        let out = k.turn("看看。").unwrap();
        assert!(out.sent_images.is_empty());
        assert!(out.reply.images.is_empty());
        assert!(
            out.notices.iter().any(|n| n.contains("素材库里没有")),
            "{:?}",
            out.notices
        );
        assert_eq!(out.retries, 0, "发错一张图不该推翻整段表演");
    }

    #[test]
    fn without_a_media_library_images_are_dropped() {
        let (mut k, _llm) = kernel_with_catalog(vec!["[图片] a.png\n[说] 嗯。"], KernelConfig::default());
        let out = k.turn("看看。").unwrap();
        assert!(out.sent_images.is_empty());
        assert!(
            out.notices.iter().any(|n| n.contains("图片素材库")),
            "{:?}",
            out.notices
        );
    }

    #[test]
    fn the_model_cannot_flood_with_images() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[图片] a\n[图片] b\n[说] 嗯。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &["a.png", "b.png"],
        );
        let out = k.turn("看看。").unwrap();
        assert_eq!(out.sent_images.len(), 1, "一回合最多一张");
        assert!(
            out.notices.iter().any(|n| n.contains("最多发 1 张图片")),
            "{:?}",
            out.notices
        );
    }

    #[test]
    fn an_image_only_reply_is_a_valid_way_to_answer() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[图片] old_photo"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &["old_photo.png"],
        );
        let out = k.turn("给我看看。").unwrap();
        assert_eq!(out.retries, 0, "只发一张图是合法表达，不该被逼出台词");
        assert!(out.reply.speech.is_empty());
        assert_eq!(out.sent_images.len(), 1);
    }

    #[test]
    fn the_user_can_send_an_image_and_the_kernel_transcribes_it() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[说] 这是……"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &["old_photo.png"],
        );
        let out = k.show_image("old_photo", Some("你看这个")).unwrap();
        assert_eq!(out.turn, 1);

        let input = k
            .session()
            .transcript
            .iter()
            .find(|e| e.kind == EventKind::UserInput)
            .unwrap();
        assert_eq!(input.image_id(), Some("old_photo"));
        assert_eq!(input.image_file(), Some("old_photo.png"));
        assert!(input.text.contains("给你看了一张图片"), "{}", input.text);
        assert!(input.text.contains("你看这个"), "{}", input.text);
    }

    #[test]
    fn an_incoming_image_does_not_contaminate_the_mood() {
        // 老照片本身不带情绪；带情绪的是"看到它"，那该由模型写 [情] 决定
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[说] 嗯。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &["old_photo.png"],
        );
        let before = k.state().mood.valence;
        k.show_image("old_photo", None).unwrap();
        assert_eq!(k.state().mood.valence, before);
    }

    #[test]
    fn show_image_without_a_library_explains_why() {
        let (mut k, _llm, _mem) = kernel_rich(vec!["[说] 哦。"], KernelConfig::default(), Vec::new(), &[], &[]);
        let err = k.show_image("a", None).unwrap_err();
        assert!(err.to_string().contains("图片素材库"), "{err}");
        assert_eq!(k.state().turn, 0, "失败的调用不该推进回合");
    }

    #[test]
    fn show_image_rejects_an_unknown_id() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[说] 哦。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &["a.png"],
        );
        let err = k.show_image("nope", None).unwrap_err();
        assert!(err.to_string().contains("没有这张图片"), "{err}");
    }

    // ------------------------------------------------------- 人物设定与重置

    #[test]
    fn set_card_swaps_the_performer_but_keeps_the_scene_and_history() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[说] 不卖。", "[说] 谁啊。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &[],
        );
        k.turn("这本书多少钱？").unwrap();
        let events_before = k.session().transcript.len();

        let mut other = CharacterCard::new("陈默");
        other.persona = "话多，爱打听。".into();
        k.set_card(other);

        assert_eq!(k.card().name, "陈默");
        assert_eq!(k.scene().location, "拾光旧书店");
        assert_eq!(
            k.session().transcript.len(),
            events_before,
            "换人是换谁来演，不是重开一局"
        );

        // 换人之后，事件的 actor 跟着换
        let out = k.turn("是我。").unwrap();
        assert_eq!(out.reply.speech, vec!["谁啊。".to_string()]);
        let last = k.session().transcript.last().unwrap();
        assert_eq!(last.actor, "陈默");
    }

    #[test]
    fn reset_clears_the_stage_but_keeps_the_card() {
        let (mut k, _llm, _mem) = kernel_rich(
            vec!["[说] 不卖。"],
            KernelConfig::default(),
            Vec::new(),
            &[],
            &[],
        );
        k.turn("在吗").unwrap();
        assert!(k.state().turn >= 1);

        k.reset();
        assert_eq!(k.state().turn, 0);
        assert_eq!(k.session().transcript.len(), 1, "只留一条「重新开始」的记号");
        assert_eq!(k.card().name, "林夏");
    }

    #[test]
    fn generation_defaults_are_deliberately_loose() {
        // 角色扮演要的是"这个人会怎么反应"，不是"最可能的下一句"。
        // 温度低了会明显趋向书面语与固定句式，人物越演越像同一个人。
        let cfg = KernelConfig::default();
        assert!(
            cfg.llm.temperature.unwrap() >= 0.9,
            "默认温度应当偏自由：{:?}",
            cfg.llm.temperature
        );
        assert!(cfg.protocol_hops >= 1, "控制通道默认应当是开着的");
        assert_eq!(cfg.image_limit, 1);
    }
}
