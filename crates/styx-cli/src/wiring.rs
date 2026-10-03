//! 装配：把配置变成一组可用的端口，再变成内核。
//!
//! # 一条原则
//!
//! **任何后端不可用都不应该让程序起不来。**
//!
//! 这不是"容错洁癖"，而是角色扮演这个场景的真实需求：记忆服务没开、
//! 模型还没配好，用户依然希望敲下一句话就能看到角色开口。因此这里的
//! `build_*` 全部返回"可用的东西 + 一句说明"，而不是错误；只有
//! 「配置自相矛盾」（比如 `backend = "openai"` 却一个端点都没写）才报错。
//!
//! 降级路径：
//!
//! ```text
//!   语言模型  配置端点 → STYX_LLM_* 环境变量 → 探测本机 Ollama/LM Studio → 离线 Mock
//!   长期记忆  Nebula   → 内存实现（真 BM25，退出即失）
//!   联想发散  MightBe  → 内存共现图（PMI + 多跳 + 弃判）
//!   共享记忆  MemoryPool（健康检查失败就不注入，回合自动跳过）
//! ```

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use styx_core::ports::{AssocPort, LlmPort, MemoryPort, PoolPort, ToolPort};
use styx_core::{CharacterCard, Kernel, Scene, StickerCatalog};

use styx_assoc::{InMemoryAssoc, MightBeAssoc, MightBeConfig};
use styx_llm::{EndpointPool, OpenAiBackend};
use styx_memory::{InMemoryMemory, NebulaConfig, NebulaMemory};
use styx_pool::{LocalPool, MemoryPool, PoolConfig};
use styx_tools::{register_builtins, ChainedTools, McpClient, McpToolPort, ToolRegistry};

use crate::config::Config;

/// 一组装好的端口。
pub struct Wiring {
    pub llm: Arc<dyn LlmPort>,
    pub memory: Arc<dyn MemoryPort>,
    pub assoc: Arc<dyn AssocPort>,
    pub pool: Option<Arc<dyn PoolPort>>,
    pub tools: Option<Arc<dyn ToolPort>>,
    /// 表情包目录。装配阶段不做决定（它和"后端"无关，是角色资源），
    /// 由需要它的入口（目前的 `styx web`）用 [`Wiring::with_stickers`] 挂上。
    pub stickers: Option<Arc<StickerCatalog>>,
    /// 装配过程中的说明（降级原因、实际连上的服务……），由上层打印。
    pub notes: Vec<String>,
}

impl Wiring {
    /// 按配置装配。
    ///
    /// `namespace` 会传给远端记忆后端，用于把不同会话/角色的数据隔开；
    /// `seed` 是给内存共现图的初始语料（通常是角色卡的背景与世界书），
    /// 否则兜底联想后端一上来是空图，联想永远为空。
    pub fn build(cfg: &Config, namespace: &str, seed: &[String]) -> Result<Wiring, String> {
        let mut notes = Vec::new();

        let llm = build_llm(&cfg.llm, &mut notes)?;
        let memory = build_memory(&cfg.memory, namespace, &mut notes);
        let assoc = build_assoc(&cfg.assoc, seed, &mut notes);
        let pool = build_pool(&cfg.pool, &mut notes);
        let tools = build_tools(
            &cfg.tools,
            memory.clone(),
            assoc.clone(),
            pool.clone(),
            &mut notes,
        );

        Ok(Wiring {
            llm,
            memory,
            assoc,
            pool,
            tools,
            stickers: None,
            notes,
        })
    }

    /// 完全离线的装配：Mock 模型 + 内存记忆 + 内存联想 + 本地共享池 + 内置工具。
    ///
    /// `styx demo` 用它来证明"整条链路本身是通的"，把外部变量全部排除掉。
    pub fn offline(seed: &[String]) -> Wiring {
        let memory: Arc<dyn MemoryPort> = Arc::new(InMemoryMemory::new());
        let assoc = Arc::new(InMemoryAssoc::new());
        for line in seed {
            assoc.observe(line);
        }
        let pool: Arc<dyn PoolPort> = Arc::new(LocalPool::new());

        let mut registry = ToolRegistry::new();
        register_builtins(
            &mut registry,
            Some(memory.clone()),
            Some(assoc.clone()),
            Some(pool.clone()),
        );

        Wiring {
            llm: styx_llm::offline_llm(),
            memory,
            assoc,
            pool: Some(pool),
            tools: Some(registry.into_port()),
            stickers: None,
            notes: vec!["离线模式：Mock 模型 + 内存记忆 + 内存联想".into()],
        }
    }

    /// 挂上表情包目录（链式，不影响其它端口）。
    pub fn with_stickers(mut self, stickers: Arc<StickerCatalog>) -> Self {
        self.stickers = Some(stickers);
        self
    }

    /// 造一个可以直接开演的内核。
    pub fn kernel(
        &self,
        cfg: &Config,
        card: CharacterCard,
        scene: Scene,
    ) -> Result<Kernel, String> {
        let kcfg = cfg.kernel_config()?;
        let mut builder = Kernel::builder(card, scene)
            .llm(self.llm.clone())
            .memory(self.memory.clone())
            .assoc(self.assoc.clone())
            .config(kcfg);
        if let Some(p) = &self.pool {
            builder = builder.pool(p.clone());
        }
        if let Some(t) = &self.tools {
            builder = builder.tools(t.clone());
        }
        if let Some(c) = &self.stickers {
            builder = builder.stickers(c.clone());
        }
        builder.build().map_err(|e| e.to_string())
    }

    /// 把装配说明打到 stderr（stdout 留给正文与 JSON）。
    pub fn explain(&self, verbose: bool) {
        for note in &self.notes {
            if verbose {
                eprintln!("· {note}");
            }
        }
    }
}

// ---------------------------------------------------------------- 语言模型

fn build_llm(section: &crate::config::LlmSection, notes: &mut Vec<String>) -> Result<Arc<dyn LlmPort>, String> {
    let mode = section.backend.trim().to_ascii_lowercase();
    match mode.as_str() {
        "mock" | "offline" => {
            notes.push("模型：离线 Mock（不联网、不消耗额度）".into());
            Ok(styx_llm::offline_llm())
        }
        "openai" | "compatible" => {
            let eps = section.resolved_endpoints();
            if eps.is_empty() {
                return Err(
                    "llm.backend = \"openai\"，但没有任何可用端点：\
                     请在 styx.toml 里写 [[llm.endpoints]]，或设置 STYX_LLM_PRIMARY_BASE_URL"
                        .into(),
                );
            }
            let pool = make_pool(eps, notes)?;
            Ok(pool)
        }
        "auto" | "" => {
            let mut eps = section.resolved_endpoints();
            if eps.is_empty() {
                if let Some(ep) = detect_local_endpoint() {
                    notes.push(format!(
                        "模型：自动发现本机服务 {}（{}）",
                        ep.name, ep.base_url
                    ));
                    eps = vec![ep];
                }
            }
            if eps.is_empty() {
                notes.push("模型：没有可用端点，回退到离线 Mock".into());
                return Ok(styx_llm::offline_llm());
            }
            make_pool(eps, notes)
        }
        other => Err(format!(
            "未知的 llm.backend：{other}（可选 auto / openai / mock）"
        )),
    }
}

fn make_pool(
    eps: Vec<styx_llm::Endpoint>,
    notes: &mut Vec<String>,
) -> Result<Arc<dyn LlmPort>, String> {
    let names: Vec<String> = eps.iter().map(|e| e.name.clone()).collect();
    let pool = EndpointPool::new(eps, Arc::new(OpenAiBackend::new())).map_err(|e| e.to_string())?;
    notes.push(format!(
        "模型：{} 个端点（{}），按权重平滑轮询",
        names.len(),
        names.join(" / ")
    ));
    Ok(Arc::new(pool))
}

/// 探测本机常见的模型服务。只做 TCP 连接，不发请求、不需要密钥。
fn detect_local_endpoint() -> Option<styx_llm::Endpoint> {
    const PROBES: [(&str, &str, &str); 2] = [
        ("ollama", "127.0.0.1:11434", "qwen2.5:7b"),
        ("lmstudio", "127.0.0.1:1234", "local-model"),
    ];
    let model_override = std::env::var("STYX_LOCAL_MODEL").ok();

    for (name, addr, default_model) in PROBES {
        let Ok(sock) = addr.parse::<SocketAddr>() else {
            continue;
        };
        if TcpStream::connect_timeout(&sock, Duration::from_millis(300)).is_err() {
            continue;
        }
        let model = model_override.clone().unwrap_or_else(|| default_model.to_string());
        return Some(match name {
            "ollama" => styx_llm::Endpoint::ollama(model),
            _ => styx_llm::Endpoint::lmstudio(model),
        });
    }
    None
}

// ---------------------------------------------------------------- 长期记忆

fn build_memory(
    section: &crate::config::MemorySection,
    namespace: &str,
    notes: &mut Vec<String>,
) -> Arc<dyn MemoryPort> {
    if section.wants_memory_only() {
        notes.push("长期记忆：内存实现（真 BM25，但退出即失）".into());
        return Arc::new(InMemoryMemory::new());
    }

    let nc = NebulaConfig {
        addr: section.addr.clone(),
        user: section.user.clone(),
        password: section.password.clone(),
        namespace: namespace.to_string(),
        ..Default::default()
    }
    .with_env();

    if nc.password.is_empty() {
        notes.push(
            "长期记忆：未提供 Nebula 密码（可用 NEBULA_PASSWORD），使用内存实现".into(),
        );
        return Arc::new(InMemoryMemory::new());
    }

    match NebulaMemory::connect(nc) {
        Ok(m) => {
            notes.push(format!("长期记忆：Nebula @ {}（命名空间 {namespace}）", section.addr));
            Arc::new(m)
        }
        Err(e) => {
            notes.push(format!("长期记忆：Nebula 连接失败（{e}），使用内存实现"));
            Arc::new(InMemoryMemory::new())
        }
    }
}

// -------------------------------------------------------------------- 联想

fn build_assoc(
    section: &crate::config::AssocSection,
    seed: &[String],
    notes: &mut Vec<String>,
) -> Arc<dyn AssocPort> {
    if section.wants_graph_only() {
        notes.push("联想：内存共现图（PMI + 多跳扩展）".into());
        return Arc::new(seed_graph(seed));
    }

    let mc = MightBeConfig {
        addr: section.addr.clone(),
        network: section.network.clone(),
        query_template: section.query_template.clone(),
        ..Default::default()
    }
    .with_env();

    match MightBeAssoc::connect(mc) {
        Ok(a) => {
            notes.push(format!(
                "联想：MightBe @ {}（网络 {}）",
                section.addr, section.network
            ));
            Arc::new(a)
        }
        Err(e) => {
            notes.push(format!("联想：MightBe 连接失败（{e}），使用内存共现图"));
            Arc::new(seed_graph(seed))
        }
    }
}

fn seed_graph(seed: &[String]) -> InMemoryAssoc {
    let graph = InMemoryAssoc::new();
    for line in seed {
        graph.observe(line);
    }
    graph
}

// -------------------------------------------------------------- 共享记忆池

fn build_pool(
    section: &crate::config::PoolSection,
    notes: &mut Vec<String>,
) -> Option<Arc<dyn PoolPort>> {
    if !section.enabled {
        return None;
    }
    let pc = PoolConfig {
        base_url: section.base_url.clone(),
        token: section.token.clone(),
        use_remote: section.use_remote,
        ..Default::default()
    };
    let pool = MemoryPool::new(pc);
    if pool.ping() {
        notes.push(format!("共享记忆池：MemoryPool @ {}", section.base_url));
    } else {
        notes.push(format!(
            "共享记忆池：{} 不可达，回合将跳过共享记忆（不影响其它功能）",
            section.base_url
        ));
    }
    Some(Arc::new(pool))
}

// -------------------------------------------------------------------- 工具

fn build_tools(
    section: &crate::config::ToolsSection,
    memory: Arc<dyn MemoryPort>,
    assoc: Arc<dyn AssocPort>,
    pool: Option<Arc<dyn PoolPort>>,
    notes: &mut Vec<String>,
) -> Option<Arc<dyn ToolPort>> {
    let mut ports: Vec<Arc<dyn ToolPort>> = Vec::new();

    if section.builtin {
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry, Some(memory), Some(assoc), pool);
        if registry.is_empty() {
            notes.push("工具：没有可注册的内置工具".into());
        } else {
            notes.push(format!("工具：内置 {} 个", registry.len()));
            ports.push(registry.into_port());
        }
    }

    for spec in &section.mcp {
        let client = McpClient::new(spec.url.clone()).with_token(spec.token.clone());
        let port = McpToolPort::connect(client);
        match port.refresh() {
            Ok(list) => {
                notes.push(format!(
                    "工具：MCP「{}」提供 {} 个工具",
                    spec.name,
                    list.len()
                ));
                ports.push(Arc::new(port));
            }
            Err(e) => notes.push(format!("工具：MCP「{}」不可用（{e}）", spec.name)),
        }
    }

    match ports.len() {
        0 => None,
        1 => ports.pop(),
        _ => Some(Arc::new(ChainedTools::new(ports))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn seed() -> Vec<String> {
        vec![
            "母亲留下的一张旧照片夹在相册里".to_string(),
            "那张旧照片被压在相册最上面一格".to_string(),
        ]
    }

    #[test]
    fn mock_backend_wires_offline() {
        let cfg: Config = toml::from_str("[llm]\nbackend = \"mock\"\n").unwrap();
        let w = Wiring::build(&cfg, "test", &seed()).unwrap();
        assert!(w.llm.health());
        assert!(w.memory.health());
        assert!(w.assoc.health());
        assert!(w.tools.is_some());
        assert!(w.pool.is_none(), "默认不启用共享记忆池");
        assert!(w.notes.iter().any(|n| n.contains("离线 Mock")));
    }

    #[test]
    fn openai_backend_without_endpoints_is_a_hard_error() {
        std::env::remove_var("STYX_LLM_PRIMARY_BASE_URL");
        let cfg: Config = toml::from_str("[llm]\nbackend = \"openai\"\n").unwrap();
        let Err(err) = Wiring::build(&cfg, "test", &[]) else {
            panic!("缺端点时必须硬报错");
        };
        assert!(err.contains("没有任何可用端点"), "{err}");
    }

    #[test]
    fn unknown_backend_is_a_hard_error() {
        let cfg: Config = toml::from_str("[llm]\nbackend = \"gemini\"\n").unwrap();
        assert!(Wiring::build(&cfg, "test", &[]).is_err());
    }

    #[test]
    fn explicit_memory_only_never_touches_the_network() {
        let cfg: Config = toml::from_str(
            r#"
            [llm]
            backend = "mock"
            [memory]
            backend = "memory"
            "#,
        )
        .unwrap();
        let w = Wiring::build(&cfg, "test", &[]).unwrap();
        assert!(w.notes.iter().any(|n| n.contains("内存实现")));
    }

    #[test]
    fn missing_nebula_password_falls_back_with_a_note() {
        std::env::remove_var("NEBULA_PASSWORD");
        let cfg: Config = toml::from_str(
            r#"
            [llm]
            backend = "mock"
            [memory]
            backend = "auto"
            password = ""
            "#,
        )
        .unwrap();
        let w = Wiring::build(&cfg, "test", &[]).unwrap();
        assert!(w.notes.iter().any(|n| n.contains("Nebula 密码")));
    }

    #[test]
    fn offline_wiring_seeds_the_association_graph() {
        // 离线兜底图必须已经吃到角色卡语料：拿种子词能想起东西，
        // 且每条联想都带证据（而不是一个空图或硬编结果）。
        let w = Wiring::offline(&seed());
        let hits = w.assoc.associate("相册", 5).unwrap();
        assert!(!hits.is_empty(), "离线兜底图应当是热的");
        assert!(hits.iter().all(|h| !h.evidence.is_empty()), "{hits:?}");
    }

    #[test]
    fn offline_wiring_supports_a_full_turn() {
        use styx_core::{CharacterCard, Scene};
        let w = Wiring::offline(&seed());
        let card = CharacterCard::parse_markdown(crate::assets::DEFAULT_CARD_MD).unwrap();
        let mut kernel = w.kernel(&Config::default(), card, Scene::new("书店")).unwrap();
        let out = kernel.turn("我想看看那本相册。").unwrap();
        assert!(!out.reply.speech.is_empty());
        assert_eq!(out.turn, 1);
    }
}
