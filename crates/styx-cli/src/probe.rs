//! 后端探针：在开始演之前，先问清楚"哪些后端真的在工作"。
//!
//! 这不是可有可无的调试功能。Styx 的默认策略是"能连上就用真的，连不上就降级"，
//! 而降级是**静默**的——角色照样会说话，只是记性变短、联想变浅。
//! `styx probe` 的存在就是为了让这件事从"静默"变成"可见"。
//!
//! 两级探测：
//!
//! | 级别 | 做什么 | 代价 |
//! |---|---|---|
//! | 默认 | 只建 TCP 连接（HTTP 端点解析 host:port 后连接） | 毫秒级、不花钱 |
//! | `--deep` | 真的发一次最小请求（模型端点发一句 1 token 的对话） | 会用到额度 |

use std::sync::Arc;
use std::time::{Duration, Instant};

use styx_core::ports::{ChatMessage, LlmPort, LlmOptions};
use styx_llm::{EndpointPool, OpenAiBackend};
use styx_memory::{NebulaConfig, NebulaClient};

use crate::config::Config;

/// 一条探测结果。
#[derive(Debug, Clone)]
pub struct ProbeLine {
    /// 后端类别：`llm` / `memory` / `assoc` / `pool`。
    pub kind: String,
    /// 后端名字。
    pub name: String,
    /// 探测目标（地址 / 网络名）。
    pub target: String,
    /// 是否可用。
    pub ok: bool,
    /// 说明。
    pub detail: String,
    /// 耗时毫秒。
    pub ms: u128,
}

impl ProbeLine {
    fn ok(kind: &str, name: &str, target: &str, detail: String, ms: u128) -> Self {
        ProbeLine {
            kind: kind.into(),
            name: name.into(),
            target: target.into(),
            ok: true,
            detail,
            ms,
        }
    }

    fn fail(kind: &str, name: &str, target: &str, detail: String, ms: u128) -> Self {
        ProbeLine {
            kind: kind.into(),
            name: name.into(),
            target: target.into(),
            ok: false,
            detail,
            ms,
        }
    }

    /// 一行文本。
    pub fn render(&self) -> String {
        format!(
            "{} {:>14}  {:<28} {}",
            if self.ok { "✔" } else { "✘" },
            self.name,
            self.target,
            self.detail
        )
    }
}

/// 跑一遍全部探测。
pub fn run(cfg: &Config, deep: bool) -> Vec<ProbeLine> {
    let mut out = Vec::new();
    out.extend(probe_llm(cfg, deep));
    out.push(probe_memory(cfg));
    out.push(probe_assoc(cfg));
    if cfg.pool.enabled {
        out.push(probe_pool(cfg));
    }
    out
}

/// 渲染成可读报告。
pub fn render(lines: &[ProbeLine]) -> String {
    if lines.is_empty() {
        return "（没有任何可探测的后端）".into();
    }
    let mut s = String::new();
    let mut last_kind = "";
    for line in lines {
        if line.kind != last_kind {
            if !last_kind.is_empty() {
                s.push('\n');
            }
            s.push_str(&format!("[{}]\n", line.kind));
            last_kind = &line.kind;
        }
        s.push_str(&line.render());
        s.push_str(&format!("  ({}ms)\n", line.ms));
    }
    let ok = lines.iter().filter(|l| l.ok).count();
    s.push_str(&format!("\n{ok}/{} 可用", lines.len()));
    s
}

/// 总结成一句给日志用的话。
pub fn summary(lines: &[ProbeLine]) -> String {
    let ok = lines.iter().filter(|l| l.ok).count();
    let total = lines.len();
    if ok == total {
        format!("{total} 个后端全部可用")
    } else {
        format!("{ok}/{total} 可用，其余将被降级替换")
    }
}

// ---------------------------------------------------------------- 语言模型

fn probe_llm(cfg: &Config, deep: bool) -> Vec<ProbeLine> {
    let eps = cfg.llm.resolved_endpoints();
    if eps.is_empty() {
        if cfg.llm.backend.eq_ignore_ascii_case("mock") {
            return vec![ProbeLine::ok(
                "llm",
                "mock",
                "（离线）",
                "显式配置为离线 Mock，无需探测".into(),
                0,
            )];
        }
        return vec![ProbeLine::fail(
            "llm",
            "—",
            "（无端点）",
            "配置里没有端点，也没有 STYX_LLM_* 环境变量".into(),
            0,
        )];
    }

    let mut out = Vec::new();
    for ep in eps {
        let started = Instant::now();
        let target = ep.chat_url();

        // 第一层：TCP 可达性
        match styx_http::Url::parse(&target) {
            Err(e) => out.push(ProbeLine::fail(
                "llm",
                &ep.name,
                &target,
                format!("地址无法解析：{e}"),
                started.elapsed().as_millis(),
            )),
            Ok(url) => {
                let host = if url.host.contains(':') {
                    format!("[{}]:{}", url.host, url.port)
                } else {
                    format!("{}:{}", url.host, url.port)
                };
                let reachable = tcp_reachable(&host);

                if !reachable {
                    out.push(ProbeLine::fail(
                        "llm",
                        &ep.name,
                        &target,
                        format!("TCP 连不上 {}（服务没起？地址写错了？）", host),
                        started.elapsed().as_millis(),
                    ));
                    continue;
                }

                if !deep {
                    out.push(ProbeLine::ok(
                        "llm",
                        &ep.name,
                        &target,
                        format!("可达（模型 {}）—— 加 --deep 可发一次真实请求", ep.model),
                        started.elapsed().as_millis(),
                    ));
                    continue;
                }

                // 第二层：真的发一次最小请求
                match deep_chat(&ep) {
                    Ok(text) => out.push(ProbeLine::ok(
                        "llm",
                        &ep.name,
                        &target,
                        format!("模型响应正常：{}", one_line(&text, 40)),
                        started.elapsed().as_millis(),
                    )),
                    Err(e) => out.push(ProbeLine::fail(
                        "llm",
                        &ep.name,
                        &target,
                        format!("可达但调用失败：{e}"),
                        started.elapsed().as_millis(),
                    )),
                }
            }
        }
    }
    out
}

fn deep_chat(ep: &styx_llm::Endpoint) -> Result<String, String> {
    let pool = EndpointPool::new(vec![ep.clone()], Arc::new(OpenAiBackend::new()))
        .map_err(|e| e.to_string())?;
    let msgs = vec![ChatMessage::user("回复一个字：好")];
    let opts = LlmOptions {
        temperature: Some(0.0),
        max_tokens: Some(8),
        ..Default::default()
    };
    pool.complete(&msgs, &opts)
        .map(|c| c.text)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 长期记忆

fn probe_memory(cfg: &Config) -> ProbeLine {
    if cfg.memory.wants_memory_only() {
        return ProbeLine::ok(
            "memory",
            "memory",
            "（进程内）",
            "显式配置为内存实现，真 BM25，但退出即失".into(),
            0,
        );
    }

    let started = Instant::now();
    let nc = NebulaConfig {
        addr: cfg.memory.addr.clone(),
        user: cfg.memory.user.clone(),
        password: cfg.memory.password.clone(),
        ..Default::default()
    }
    .with_env();

    if nc.password.is_empty() {
        return ProbeLine::fail(
            "memory",
            "nebula",
            &nc.addr,
            "未提供密码（写入配置或设置 NEBULA_PASSWORD），将降级到内存实现".into(),
            started.elapsed().as_millis(),
        );
    }

    match NebulaClient::connect_as(&nc.addr, &nc.user, &nc.password, Duration::from_secs(5)) {
        Ok(mut client) => {
            let ping = client.ping().unwrap_or(false);
            let _ = client.close();
            let ms = started.elapsed().as_millis();
            if ping {
                ProbeLine::ok("memory", "nebula", &nc.addr, "握手成功，ping 正常".into(), ms)
            } else {
                ProbeLine::fail(
                    "memory",
                    "nebula",
                    &nc.addr,
                    "握手成功但 ping 失败".into(),
                    ms,
                )
            }
        }
        Err(e) => ProbeLine::fail(
            "memory",
            "nebula",
            &nc.addr,
            format!("连接失败：{e}"),
            started.elapsed().as_millis(),
        ),
    }
}

// -------------------------------------------------------------------- 联想

fn probe_assoc(cfg: &Config) -> ProbeLine {
    if cfg.assoc.wants_graph_only() {
        return ProbeLine::ok(
            "assoc",
            "graph",
            "（进程内）",
            "显式配置为内存共现图（PMI + 多跳）".into(),
            0,
        );
    }

    let started = Instant::now();
    match styx_assoc::MightBeClient::connect(&cfg.assoc.addr, Duration::from_secs(5)) {
        Ok(mut client) => {
            let ping = client.ping();
            let ms = started.elapsed().as_millis();
            if ping {
                ProbeLine::ok(
                    "assoc",
                    "mightbe",
                    &cfg.assoc.addr,
                    format!("连通（网络 {}）", cfg.assoc.network),
                    ms,
                )
            } else {
                ProbeLine::fail(
                    "assoc",
                    "mightbe",
                    &cfg.assoc.addr,
                    "连接成功但 SHOW STATUS 无响应".into(),
                    ms,
                )
            }
        }
        Err(e) => ProbeLine::fail(
            "assoc",
            "mightbe",
            &cfg.assoc.addr,
            format!("连接失败：{e}"),
            started.elapsed().as_millis(),
        ),
    }
}

// -------------------------------------------------------------- 共享记忆池

fn probe_pool(cfg: &Config) -> ProbeLine {
    let started = Instant::now();
    let pc = styx_pool::PoolConfig {
        base_url: cfg.pool.base_url.clone(),
        token: cfg.pool.token.clone(),
        use_remote: cfg.pool.use_remote,
        ..Default::default()
    };
    let pool = styx_pool::MemoryPool::new(pc);
    let ms = started.elapsed().as_millis();
    if pool.ping() {
        ProbeLine::ok("pool", "memorypool", &cfg.pool.base_url, "GET /health 正常".into(), ms)
    } else {
        ProbeLine::fail(
            "pool",
            "memorypool",
            &cfg.pool.base_url,
            "GET /health 失败（服务没起或 token 不对）".into(),
            ms,
        )
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{cut}…")
}

/// `host:port` 的 TCP 可达性。名字解析失败也算不可达。
fn tcp_reachable(host: &str) -> bool {
    use std::net::ToSocketAddrs;

    let deadline = Duration::from_millis(800);
    match host.parse::<std::net::SocketAddr>() {
        Ok(sock) => std::net::TcpStream::connect_timeout(&sock, deadline).is_ok(),
        Err(_) => host
            .to_socket_addrs()
            .map(|addrs| {
                addrs.into_iter().any(|s| {
                    std::net::TcpStream::connect_timeout(&s, deadline).is_ok()
                })
            })
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_backend_probes_offline() {
        let cfg: Config = toml::from_str("[llm]\nbackend = \"mock\"\n").unwrap();
        let lines = probe_llm(&cfg, false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ok);
        assert!(lines[0].target.contains("离线"));
    }

    #[test]
    fn no_endpoint_is_reported_as_failure() {
        std::env::remove_var("STYX_LLM_PRIMARY_BASE_URL");
        let cfg: Config = toml::from_str("[llm]\nbackend = \"auto\"\n").unwrap();
        let lines = probe_llm(&cfg, false);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].ok);
        assert!(lines[0].detail.contains("没有端点"));
    }

    #[test]
    fn unhealthy_endpoint_does_not_panic() {
        // 留给一个几乎不可能有人监听的端口：探测应当返回"不可用"，而不是 panic
        let cfg: Config = toml::from_str(
            r#"
            [[llm.endpoints]]
            name = "nothing"
            base_url = "http://127.0.0.1:1/v1"
            model = "m"
            "#,
        )
        .unwrap();
        let lines = probe_llm(&cfg, false);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].ok);
    }

    #[test]
    fn memory_only_short_circuits() {
        let cfg: Config = toml::from_str("[memory]\nbackend = \"memory\"\n").unwrap();
        let l = probe_memory(&cfg);
        assert!(l.ok);
    }

    #[test]
    fn report_renders_groups_and_totals() {
        let lines = vec![
            ProbeLine::ok("llm", "a", "t", "fine".into(), 1),
            ProbeLine::fail("memory", "b", "t", "down".into(), 2),
        ];
        let text = render(&lines);
        assert!(text.contains("[llm]"));
        assert!(text.contains("[memory]"));
        assert!(text.contains("1/2 可用"));
        assert_eq!(summary(&lines), "1/2 可用，其余将被降级替换");
    }

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("a\nb", 10), "a b");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(one_line("abc", 3), "abc");
    }
}
