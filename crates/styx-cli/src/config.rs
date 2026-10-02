//! `styx.toml`：把"用哪些后端、演哪个角色"写进一个文件。
//!
//! 设计原则：**所有字段都有默认值**。一份空配置也能跑起来，
//! 用户只需要写他真正关心的那几行。每个 `backend` 字段支持 `auto`，
//! 语义是"能用就用真的，用不了就降级"——这让同一份配置在
//! 「装了 Nebula 的机器」和「一台空机器」上都能开演。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use styx_core::{KernelConfig, PromptBudget, Scene};
use styx_llm::Endpoint;

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
                other => return Err(format!("未知的 kernel.budget：{other}（可选 default/compact/large）")),
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
        let from_cfg: Vec<Endpoint> = self.endpoints.iter().filter(|e| e.enabled).cloned().collect();
        if !from_cfg.is_empty() {
            return from_cfg;
        }
        styx_llm::from_env(&[
            "primary", "secondary", "tertiary", "openai", "deepseek", "ollama", "local",
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
}
