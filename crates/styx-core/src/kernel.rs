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
use crate::event::EventKind;
use crate::ports::{
    AssocPort, Association, ChatMessage, Completion, LlmOptions, LlmPort, MemoryNote, MemoryPort,
    PoolPort, Recalled, ToolPort,
};
use crate::prompt::{PromptBudget, PromptBuilder, PromptReport};
use crate::reply::Reply;
use crate::scene::Scene;
use crate::session::{Audit, Guard, Session};
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
}

impl Default for KernelConfig {
    fn default() -> Self {
        KernelConfig {
            user_name: "对方".into(),
            budget: PromptBudget::default(),
            llm: LlmOptions {
                temperature: Some(0.85),
                top_p: Some(0.9),
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
        s.trim_end().to_string()
    }

    /// 一行调试摘要。
    pub fn debug_line(&self) -> String {
        format!(
            "turn {} · {} · 审核{} · 重试{} · 写记忆{} · {}",
            self.turn,
            self.report.render(),
            if self.audit.needs_retry() { "有违规" } else { "通过" },
            self.retries,
            self.memory_written,
            self.completion.endpoint
        )
    }
}

/// 角色扮演内核。
pub struct Kernel {
    session: Session,
    llm: Arc<dyn LlmPort>,
    memory: Arc<dyn MemoryPort>,
    assoc: Arc<dyn AssocPort>,
    pool: Option<Arc<dyn PoolPort>>,
    tools: Option<Arc<dyn ToolPort>>,
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
            config: KernelConfig::default(),
        }
    }

    /// 直接构造（所有端口必须齐备）。
    pub fn new(
        session: Session,
        llm: Arc<dyn LlmPort>,
        memory: Arc<dyn MemoryPort>,
        assoc: Arc<dyn AssocPort>,
        pool: Option<Arc<dyn PoolPort>>,
        tools: Option<Arc<dyn ToolPort>>,
        config: KernelConfig,
    ) -> Self {
        Kernel {
            session,
            llm,
            memory,
            assoc,
            pool,
            tools,
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
            turn: self.session.state.turn,
            transcript: self.session.transcript.len(),
            degraded,
        }
    }

    // --------------------------------------------------------------- 主要

    /// 执行一个回合。
    ///
    /// 顺序（注意提示词是在**用户输入入账之前**装配的，避免重复注入）：
    /// 召回 → 联想 → 装配 → 生成 → 解析 → 守护 → 结算 → 落库
    pub fn turn(&mut self, user_input: &str) -> Result<TurnOutcome> {
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
            let history: &[crate::event::Event] = &self.session.transcript;
            let builder = PromptBuilder {
                card: &self.session.card,
                scene: &self.session.scene,
                state: &self.session.state,
                budget: &self.config.budget,
                history,
                user_role: &self.config.user_name,
                user_input,
            };
            builder.build(&recalled, &associations, &pool_notes, &all_notices)
        };
        if plan.report.over_budget {
            notices.push("提示词超出预算".into());
        }

        // 用户输入入账（在提示词装配之后，保证历史不重复）
        self.session
            .push(EventKind::UserInput, self.config.user_name.clone(), user_input);

        // ── 5) 生成 + 一致性重试 ─────────────────────────────────────
        let completion = self.llm.complete(&plan.messages, &self.config.llm)?;
        let mut reply = Reply::parse(&completion.text).unwrap_or_else(|_| {
            Reply::as_plain_speech(&completion.text)
        });

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
            match self.llm.complete(&msgs, &retry_opts) {
                Ok(retry_completion) => {
                    if let Ok(new_reply) = Reply::parse(&retry_completion.text) {
                        let new_audit = Guard::audit(&self.session.card, &new_reply);
                        // 采纳重试结果：违规项变少就换
                        if new_audit.violations.len() < audit.violations.len() {
                            reply = new_reply;
                            audit = new_audit;
                        }
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

        // ── 7) 结算状态与场景 ────────────────────────────────────────
        self.session
            .settle(&reply.state_delta, self.config.decay_rate);
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
        })
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
}
