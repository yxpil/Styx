//! `styx.toml`：把"用哪些后端、演哪个角色"写进一个文件。
//!
//! 设计原则：**所有字段都有默认值**。一份空配置也能跑起来，
//! 用户只需要写他真正关心的那几行。每个 `backend` 字段支持 `auto`，
//! 语义是"能用就用真的，用不了就降级"——这让同一份配置在
//! 「装了 Nebula 的机器」和「一台空机器」上都能开演。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use styx_core::{KernelConfig, PromptBudget, Scene};
use styx_guard::Limits;
use styx_llm::Endpoint;
use styx_observ::{AlertRule, Comparison, Severity};

/// 默认配置文件名（在 CWD 里自动发现）。
pub const FILE_NAME: &str = "styx.toml";

/// 顶层配置。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub character: CharacterSection,
    pub kernel: KernelSection,
    pub llm: LlmSection,
    pub memory: MemorySection,
    pub assoc: AssocSection,
    pub pool: PoolSection,
    pub tools: ToolsSection,
    pub web: WebSection,
    /// 服务层配额（连接数 / 会话数 / 单行长度）。默认全不限。
    pub limits: LimitsSection,
    /// 可观测性（日志与告警）。
    pub observability: ObservabilitySection,
    /// 访问控制。
    pub auth: AuthSection,
}

impl Config {
    /// 从显式路径或默认位置加载。
    ///
    /// 第二个返回值表示"实际读到的文件"，`None` 表示没有配置文件——
    /// 调用方据此给一句友好提示，而不是报错。
    pub fn load(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>), String> {
        let path = match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => {
                let cwd = PathBuf::from(FILE_NAME);
                if cwd.is_file() {
                    Some(cwd)
                } else {
                    None
                }
            }
        };

        let Some(path) = path else {
            return Ok((Config::default(), None));
        };

        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("读不到配置文件 {}：{e}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| format!("配置文件 {} 解析失败：{e}", path.display()))?;
        Ok((cfg, Some(path)))
    }

    /// 转成内核运行配置。
    pub fn kernel_config(&self) -> Result<KernelConfig, String> {
        let mut k = KernelConfig::default();
        self.kernel.apply(&mut k)?;
        k.user_name = self.character.user_name.clone();
        Ok(k)
    }

    /// 场景初始状态。
    pub fn scene(&self) -> Scene {
        Scene {
            world: self.character.world.clone(),
            location: self.character.location.clone(),
            time: self.character.time.clone(),
            user_role: self.character.user_role.clone(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------- 角色与场景

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CharacterSection {
    /// 角色卡路径（`.md` 或 `.json`）。留空则用内置示例角色。
    pub card: Option<PathBuf>,
    /// 用户在故事里的称呼。
    pub user_name: String,
    pub world: String,
    pub location: String,
    pub time: String,
    /// 用户扮演的角色身份。
    pub user_role: String,
}

impl Default for CharacterSection {
    fn default() -> Self {
        CharacterSection {
            card: None,
            user_name: "陈默".into(),
            world: "当代都市".into(),
            location: "拾光旧书店".into(),
            time: "傍晚".into(),
            user_role: "常客".into(),
        }
    }
}

// -------------------------------------------------------------------- 内核

/// 内核调参。所有字段可选，写哪条改哪条。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct KernelSection {
    pub memory_recall: Option<usize>,
    pub assoc_seeds: Option<usize>,
    pub assoc_limit: Option<usize>,
    pub pool_recall: Option<usize>,
    pub write_memory: Option<bool>,
    pub write_pool: Option<bool>,
    pub memory_threshold: Option<f32>,
    pub decay_rate: Option<f32>,
    pub guard_retries: Option<usize>,
    pub use_assoc: Option<bool>,
    pub use_pool: Option<bool>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// 提示词预算：`default` / `compact` / `large`。
    pub budget: Option<String>,
}

impl KernelSection {
    /// 把写了的字段覆盖到内核配置上。
    pub fn apply(&self, cfg: &mut KernelConfig) -> Result<(), String> {
        if let Some(v) = self.memory_recall {
            cfg.memory_recall = v;
        }
        if let Some(v) = self.assoc_seeds {
            cfg.assoc_seeds = v;
        }
        if let Some(v) = self.assoc_limit {
            cfg.assoc_limit = v;
        }
        if let Some(v) = self.pool_recall {
            cfg.pool_recall = v;
        }
        if let Some(v) = self.write_memory {
            cfg.write_memory = v;
        }
        if let Some(v) = self.write_pool {
            cfg.write_pool = v;
        }
        if let Some(v) = self.memory_threshold {
            cfg.memory_threshold = v.clamp(0.0, 1.0);
        }
        if let Some(v) = self.decay_rate {
            cfg.decay_rate = v.clamp(0.0, 1.0);
        }
        if let Some(v) = self.guard_retries {
            cfg.guard_retries = v;
        }
        if let Some(v) = self.use_assoc {
            cfg.use_assoc = v;
        }
        if let Some(v) = self.use_pool {
            cfg.use_pool = v;
        }
        if let Some(v) = self.temperature {
            cfg.llm.temperature = Some(v);
        }
        if let Some(v) = self.max_tokens {
            cfg.llm.max_tokens = Some(v);
        }
        if let Some(name) = &self.budget {
            cfg.budget = match name.trim().to_ascii_lowercase().as_str() {
                "" | "default" => PromptBudget::default(),
                "compact" | "small" | "tiny" => PromptBudget::compact(),
                "large" | "big" => PromptBudget::large(),
                other => {
                    return Err(format!(
                        "未知的 kernel.budget：{other}（可选 default/compact/large）"
                    ))
                }
            };
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- 语言模型

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmSection {
    /// `auto`（默认）/ `openai` / `mock`。
    pub backend: String,
    /// 端点列表。可写多个，按权重轮询并自动故障转移。
    pub endpoints: Vec<Endpoint>,
}

impl Default for LlmSection {
    fn default() -> Self {
        LlmSection {
            backend: "auto".into(),
            endpoints: Vec::new(),
        }
    }
}

impl LlmSection {
    /// 实际要使用的端点：配置里有就用配置的，否则从 `STYX_LLM_*` 环境变量里找。
    ///
    /// 环境变量命名规则：`STYX_LLM_<NAME>_BASE_URL` / `_MODEL` / `_API_KEY`，
    /// 其中 `<NAME>` 取自下面这份候选名单（大写、连字符换下划线）。
    pub fn resolved_endpoints(&self) -> Vec<Endpoint> {
        let from_cfg: Vec<Endpoint> = self
            .endpoints
            .iter()
            .filter(|e| e.enabled)
            .cloned()
            .collect();
        if !from_cfg.is_empty() {
            return from_cfg;
        }
        styx_llm::from_env(&[
            "primary",
            "secondary",
            "tertiary",
            "openai",
            "deepseek",
            "ollama",
            "local",
        ])
        .into_iter()
        .filter(|e| e.enabled)
        .collect()
    }
}

// ---------------------------------------------------------------- 长期记忆

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MemorySection {
    /// `auto`（默认）/ `nebula` / `memory`。
    pub backend: String,
    /// Nebula 服务地址。
    pub addr: String,
    pub user: String,
    /// 密码。建议留空并用环境变量 `NEBULA_PASSWORD`。
    pub password: String,
}

impl Default for MemorySection {
    fn default() -> Self {
        MemorySection {
            backend: "auto".into(),
            addr: "127.0.0.1:7878".into(),
            user: "admin".into(),
            password: String::new(),
        }
    }
}

impl MemorySection {
    /// 是否被显式要求只用内存兜底。
    pub fn wants_memory_only(&self) -> bool {
        matches!(
            self.backend.trim().to_ascii_lowercase().as_str(),
            "memory" | "inmemory" | "in-memory" | "local"
        )
    }
}

// -------------------------------------------------------------------- 联想

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AssocSection {
    /// `auto`（默认）/ `mightbe` / `graph`。
    pub backend: String,
    pub addr: String,
    /// MightBe 的网络名。
    pub network: String,
    /// 联想语句模板，占位符 `{net}` `{seed}` `{limit}`。
    pub query_template: String,
}

impl Default for AssocSection {
    fn default() -> Self {
        AssocSection {
            backend: "auto".into(),
            addr: "127.0.0.1:9527".into(),
            network: "doc_rnn".into(),
            query_template: "SELECT word, score FROM ASSOCIATE({net}, {seed}) LIMIT {limit}".into(),
        }
    }
}

impl AssocSection {
    /// 是否被显式要求只用内存共现图。
    pub fn wants_graph_only(&self) -> bool {
        matches!(
            self.backend.trim().to_ascii_lowercase().as_str(),
            "graph" | "memory" | "inmemory" | "in-memory" | "local"
        )
    }
}

// -------------------------------------------------------------- 共享记忆池

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PoolSection {
    /// 是否启用共享记忆池。
    pub enabled: bool,
    pub base_url: String,
    pub token: String,
    /// 走 BIT Remote 的 `/invoke` 入口而不是 REST。
    pub use_remote: bool,
}

impl Default for PoolSection {
    fn default() -> Self {
        PoolSection {
            enabled: false,
            base_url: "http://127.0.0.1:8751".into(),
            token: String::new(),
            use_remote: false,
        }
    }
}

// -------------------------------------------------------------------- 工具

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolsSection {
    /// 注册内置工具（时钟 / 骰子 / 挑选 / 记忆检索 / 联想）。
    pub builtin: bool,
    /// 额外的 MCP 服务器（Streamable HTTP）。
    pub mcp: Vec<McpSpec>,
}

impl Default for ToolsSection {
    fn default() -> Self {
        ToolsSection {
            builtin: true,
            mcp: Vec::new(),
        }
    }
}

/// 一个 MCP 服务器。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpSpec {
    /// 可读名字，只用于日志。
    pub name: String,
    /// Streamable HTTP 地址。
    pub url: String,
    #[serde(default)]
    pub token: String,
}

// -------------------------------------------------------------------- 前端

/// `styx web`：浏览器界面。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct WebSection {
    /// 监听地址。默认只绑本机——这是个单人界面，不该默认暴露到局域网。
    pub addr: String,
    /// 表情包目录（`.png` / `.jpg` / `.gif` / `.webp`）。
    pub stickers: PathBuf,
    /// 默认会话名：同名刷新后能接着演。
    pub session: String,
    /// 启动后自动打开浏览器。
    pub open: bool,
    /// 打印每个请求（排查前端问题、观察回合耗时时打开）。
    pub request_log: bool,
}

impl Default for WebSection {
    fn default() -> Self {
        WebSection {
            addr: "127.0.0.1:8770".into(),
            stickers: PathBuf::from("web/stickers"),
            session: "web".into(),
            open: false,
            request_log: false,
        }
    }
}

// ---------------------------------------------------------------- 服务层配额

/// `[limits]`：服务层配额。
///
/// 写 `preset = "production"` 就能一次性拿到一份保守档，再按需要覆盖单项。
/// 不写就是个人模式——**不限制任何东西**，行为和从前一模一样。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LimitsSection {
    /// 预设：`personal`（默认）/ `production`。
    pub preset: Option<String>,
    pub max_connections: Option<usize>,
    pub max_sessions: Option<usize>,
    pub max_line_bytes: Option<usize>,
    pub acquire_timeout_ms: Option<u64>,
    pub drain_timeout_ms: Option<u64>,
}

impl LimitsSection {
    /// 预设打底，逐项覆盖。
    pub fn resolve(&self) -> Result<Limits, String> {
        let mut l = match self.preset.as_deref().map(str::trim) {
            None | Some("") | Some("personal") | Some("local") | Some("single") => {
                Limits::default()
            }
            Some("production") | Some("prod") | Some("server") => Limits::production(),
            Some(other) => {
                return Err(format!(
                    "未知的 limits.preset：{other}（可选 personal / production）"
                ))
            }
        };
        if let Some(v) = self.max_connections {
            l.max_connections = v;
        }
        if let Some(v) = self.max_sessions {
            l.max_sessions = v;
        }
        if let Some(v) = self.max_line_bytes {
            l.max_line_bytes = v;
        }
        if let Some(v) = self.acquire_timeout_ms {
            l.acquire_timeout_ms = v;
        }
        if let Some(v) = self.drain_timeout_ms {
            l.drain_timeout_ms = v;
        }
        Ok(l)
    }
}

// -------------------------------------------------------------------- 访问控制

/// `[auth]`：访问控制。
///
/// 只有一项，而且默认是空的。这是有意的：**个人使用不该被登录挡住**。
/// 一旦监听地址不再是本机专属，`styx web` 会在启动时把这个事实喊出来。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AuthSection {
    /// 访问令牌。留空 = 不校验。建议用 `styx web --generate-token` 生成。
    pub token: String,
}

impl AuthSection {
    /// 解析成令牌；空串返回 `None`。
    pub fn resolved(&self) -> Option<styx_guard::Token> {
        styx_guard::Token::new(self.token.clone())
    }
}

// ---------------------------------------------------------------- 可观测性

/// `[observability]`：日志与告警。
///
/// 告警默认**关着**。理由和 `Limits` 一样：一个人自己用的时候，
/// 不该有东西在背后自己响；对外提供服务时再打开。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ObservabilitySection {
    /// 日志级别：`error` / `warn` / `info` / `debug` / `trace`。
    pub log_level: String,
    /// 日志格式：`pretty`（人看）/ `json`（JSON Lines，给采集器）。
    pub log_format: String,
    /// 是否启用内置告警规则（在有人拉 `/metrics` 时评估）。
    pub alerts: bool,
}

impl Default for ObservabilitySection {
    fn default() -> Self {
        ObservabilitySection {
            log_level: "info".into(),
            log_format: "pretty".into(),
            alerts: false,
        }
    }
}

impl ObservabilitySection {
    /// 解析日志级别。
    pub fn level(&self) -> Result<styx_observ::Level, String> {
        styx_observ::Level::parse(&self.log_level).ok_or_else(|| {
            format!(
                "未知的 observability.log_level：{}（可选 error/warn/info/debug/trace）",
                self.log_level
            )
        })
    }

    /// 解析日志格式。
    pub fn format(&self) -> Result<styx_observ::Format, String> {
        styx_observ::Format::parse(&self.log_format).ok_or_else(|| {
            format!(
                "未知的 observability.log_format：{}（可选 pretty/json）",
                self.log_format
            )
        })
    }

    /// 内置告警规则。
    ///
    /// 只挑"确实说明出了问题"的量。刻意没有给错误数设一个很低的阈值：
    /// 角色扮演里偶尔一次模型超时是正常波动，一条会被正常波动触发的
    /// 告警，训练出来的只是"看到就关掉"。
    pub fn alert_rules(&self, max_connections: usize) -> Vec<AlertRule> {
        if !self.alerts {
            return Vec::new();
        }
        let mut rules = vec![
            AlertRule::new(
                "styx_errors_high",
                "styx_errors_total",
                Comparison::Above,
                50.0,
            )
            .cooldown_secs(300)
            .severity(Severity::Warning)
            .summary("累计错误数偏高"),
            AlertRule::new(
                "styx_rejections_high",
                "styx_rejected_total",
                Comparison::Above,
                100.0,
            )
            .cooldown_secs(300)
            .severity(Severity::Warning)
            .summary("大量连接被拒：配额可能太紧，或者正在被压"),
            AlertRule::new(
                "styx_http_5xx",
                "styx_http_5xx_total",
                Comparison::Above,
                20.0,
            )
            .cooldown_secs(300)
            .severity(Severity::Critical)
            .summary("服务端错误响应增多"),
        ];
        if max_connections > 0 {
            // 80% 就报，而不是 100%：等到打满的那一刻，拒绝已经在发生了。
            let threshold = (max_connections as f64 * 0.8).max(1.0);
            rules.push(
                AlertRule::new(
                    "styx_connections_saturated",
                    "styx_in_flight",
                    Comparison::Above,
                    threshold,
                )
                .for_secs(30)
                .cooldown_secs(300)
                .severity(Severity::Critical)
                .summary("连接数接近上限并持续了 30 秒"),
            );
        }
        rules
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_usable() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.llm.backend, "auto");
        assert!(cfg.tools.builtin);
        assert_eq!(cfg.scene().location, "拾光旧书店");
        let k = cfg.kernel_config().unwrap();
        assert_eq!(k.user_name, "陈默");
        assert_eq!(k.memory_recall, 8);
        assert_eq!(k.budget.total, PromptBudget::default().total);
    }

    #[test]
    fn partial_config_only_overrides_what_it_says() {
        let cfg: Config = toml::from_str(
            r#"
            [kernel]
            memory_recall = 3
            budget = "compact"
            "#,
        )
        .unwrap();
        let k = cfg.kernel_config().unwrap();
        assert_eq!(k.memory_recall, 3);
        assert_eq!(k.budget.total, PromptBudget::compact().total);
        // 没写的字段保持默认
        assert_eq!(k.assoc_seeds, KernelConfig::default().assoc_seeds);
        assert_eq!(k.decay_rate, KernelConfig::default().decay_rate);
    }

    #[test]
    fn unknown_budget_is_rejected() {
        let cfg: Config = toml::from_str("[kernel]\nbudget = \"huge\"\n").unwrap();
        assert!(cfg.kernel_config().is_err());
    }

    #[test]
    fn endpoints_parse_from_toml() {
        let cfg: Config = toml::from_str(
            r#"
            [llm]
            backend = "openai"
            [[llm.endpoints]]
            name = "a"
            base_url = "https://api.openai.com/v1"
            model = "gpt-4o-mini"
            weight = 3
            "#,
        )
        .unwrap();
        let eps = cfg.llm.resolved_endpoints();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].weight, 3);
        assert_eq!(
            eps[0].chat_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn disabled_endpoints_are_skipped() {
        let cfg: Config = toml::from_str(
            r#"
            [[llm.endpoints]]
            name = "a"
            base_url = "http://h/v1"
            model = "m"
            enabled = false
            "#,
        )
        .unwrap();
        assert!(cfg.llm.resolved_endpoints().is_empty());
    }

    #[test]
    fn backend_predicates() {
        let cfg: Config = toml::from_str("[memory]\nbackend = \"memory\"\n").unwrap();
        assert!(cfg.memory.wants_memory_only());
        let cfg: Config = toml::from_str("[assoc]\nbackend = \"graph\"\n").unwrap();
        assert!(cfg.assoc.wants_graph_only());
        let cfg: Config = toml::from_str("[assoc]\nbackend = \"auto\"\n").unwrap();
        assert!(!cfg.assoc.wants_graph_only());
    }

    #[test]
    fn web_section_defaults_to_localhost_and_no_auto_open() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.web.addr, "127.0.0.1:8770");
        assert!(!cfg.web.open, "不该默认抢走浏览器焦点");
        assert!(!cfg.web.request_log);
        assert_eq!(cfg.web.stickers, PathBuf::from("web/stickers"));
    }

    #[test]
    fn web_section_only_overrides_what_it_says() {
        let cfg: Config = toml::from_str(
            r#"
            [web]
            addr = "127.0.0.1:9000"
            stickers = "/tmp/emoji"
            open = true
            "#,
        )
        .unwrap();
        assert_eq!(cfg.web.addr, "127.0.0.1:9000");
        assert_eq!(cfg.web.stickers, PathBuf::from("/tmp/emoji"));
        assert!(cfg.web.open);
        assert_eq!(cfg.web.session, "web", "没写的字段保持默认");
    }
}
