//! # styx — 角色扮演内核的命令行入口
//!
//! ```text
//!   styx repl     交互式角色扮演（默认命令）
//!   styx say      单次对话，适合脚本 / 管道
//!   styx web      在浏览器里演（含表情包面板）
//!   styx serve    把内核暴露成多会话 TCP 服务
//!   styx probe    逐个探测后端（模型 / Nebula / MightBe / MemoryPool）
//!   styx demo     完全离线跑通整条链路（CI 也用它做冒烟测试）
//!   styx card     校验 / 规范化角色卡
//!   styx init     生成 styx.toml 与示例角色卡
//! ```
//!
//! 所有命令共享 `--config`，默认在当前目录找 `styx.toml`；
//! 找不到时用内置默认配置（自动探测 → 探测不到就离线 Mock）。

mod assets;
mod config;
mod probe;
mod repl;
mod wiring;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use styx_core::{CharacterCard, Kernel, Scene, StickerCatalog, StyxError};

use crate::config::Config;
use crate::wiring::Wiring;

#[derive(Debug, Parser)]
#[command(
    name = "styx",
    version,
    about = "Styx —— 可联动的角色扮演内核",
    long_about = "Styx 把一个「角色」在「场景」中随时间演化的状态，与语言模型、长期记忆、\
                  联想、共享记忆池、工具编排成一个回合闭环。\n\
                  内核只依赖端口（trait），因此同一份内核既可以跑在 Nebula / MightBe / \
                  MemoryPool 组成的集群上，也可以完全离线跑内存兜底实现。",
    arg_required_else_help = true
)]
struct Cli {
    /// 配置文件路径（默认在当前目录查找 styx.toml）
    #[arg(long, global = true, value_name = "FILE", env = "STYX_CONFIG")]
    config: Option<PathBuf>,

    /// 打印装配细节（降级原因、连上的服务等）
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 交互式角色扮演（不指定子命令时的默认行为）
    Repl(ReplArgs),
    /// 单次对话：读一句话，打印角色的回应
    Say(SayArgs),
    /// 启动 Web 前端：在浏览器里演，支持表情包
    Web(WebArgs),
    /// 启动多会话 TCP 服务
    Serve(ServeArgs),
    /// 探测各后端的可用性
    Probe(ProbeArgs),
    /// 完全离线地跑通整条链路
    Demo(DemoArgs),
    /// 校验 / 规范化角色卡
    Card(CardArgs),
    /// 生成 styx.toml 与示例角色卡
    Init(InitArgs),
}

#[derive(Debug, Args)]
struct ReplArgs {
    /// 角色卡路径（覆盖配置）
    #[arg(short, long, value_name = "FILE")]
    card: Option<PathBuf>,
    /// 开局就显示调试信息
    #[arg(long)]
    debug: bool,
    /// 先跑一遍后端探测，再进入对话
    #[arg(long)]
    probe: bool,
}

#[derive(Debug, Args)]
struct SayArgs {
    /// 角色卡路径（覆盖配置）
    #[arg(short, long, value_name = "FILE")]
    card: Option<PathBuf>,
    /// 以 JSON 输出（便于脚本消费）
    #[arg(long)]
    json: bool,
    /// 额外打印回合报告
    #[arg(long)]
    report: bool,
    /// 你说的话（可以多个词；不写则从标准输入读一行）
    #[arg(value_name = "TEXT")]
    text: Vec<String>,
}

#[derive(Debug, Args)]
struct WebArgs {
    /// 监听地址（覆盖配置里的 web.addr）
    #[arg(long)]
    addr: Option<String>,
    /// 角色卡路径（覆盖配置）
    #[arg(short, long, value_name = "FILE")]
    card: Option<PathBuf>,
    /// 表情包目录（覆盖配置里的 web.stickers）
    #[arg(long, value_name = "DIR")]
    stickers: Option<PathBuf>,
    /// 默认会话名（同一个名字刷新浏览器后能接着演）
    #[arg(long)]
    session: Option<String>,
    /// 启动后自动打开浏览器
    #[arg(long)]
    open: bool,
    /// 打印每个请求（排查前端问题、观察回合耗时）
    #[arg(long)]
    request_log: bool,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// 监听地址
    #[arg(long, default_value = "127.0.0.1:7879")]
    addr: String,
    /// 角色卡路径（覆盖配置）
    #[arg(short, long, value_name = "FILE")]
    card: Option<PathBuf>,
    /// 默认会话名
    #[arg(long, default_value = "default")]
    session: String,
}

#[derive(Debug, Args)]
struct ProbeArgs {
    /// 真的发一次最小请求（会用到模型额度），而不是只测连通性
    #[arg(long)]
    deep: bool,
}

#[derive(Debug, Args)]
struct DemoArgs {
    /// 演出多少个回合
    #[arg(long, default_value_t = 3)]
    turns: usize,
    /// 角色卡路径（覆盖内置示例）
    #[arg(short, long, value_name = "FILE")]
    card: Option<PathBuf>,
    /// 打印每回合给模型的完整提示词
    #[arg(long)]
    show_prompt: bool,
}

#[derive(Debug, Args)]
struct CardArgs {
    /// 角色卡路径（.md 或 .json）
    path: PathBuf,
    /// 打印规范化后的 Markdown
    #[arg(long)]
    normalize: bool,
    /// 以 JSON 打印解析结果
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// 输出目录
    #[arg(long, default_value = ".")]
    dir: PathBuf,
    /// 覆盖已存在的文件
    #[arg(long)]
    force: bool,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("styx: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let (cfg, path) = Config::load(cli.config.as_deref())?;
    if cli.verbose {
        match &path {
            Some(p) => eprintln!("· 配置：{}", p.display()),
            None => eprintln!("· 配置：未找到 {}，使用内置默认值", config::FILE_NAME),
        }
    }

    match cli.command {
        Command::Init(a) => cmd_init(&a, cli.verbose),
        Command::Card(a) => cmd_card(&a),
        Command::Probe(a) => cmd_probe(&cfg, &a),
        Command::Demo(a) => cmd_demo(&a, cli.verbose),
        Command::Say(a) => cmd_say(&cfg, &a, cli.verbose),
        Command::Web(a) => cmd_web(&cfg, &a, cli.verbose),
        Command::Repl(a) => cmd_repl(&cfg, &a, cli.verbose),
        Command::Serve(a) => cmd_serve(&cfg, &a, cli.verbose),
    }
}

// -------------------------------------------------------------------- init

fn cmd_init(args: &InitArgs, verbose: bool) -> Result<(), String> {
    std::fs::create_dir_all(&args.dir).map_err(|e| format!("创建 {} 失败：{e}", args.dir.display()))?;

    let cfg_path = args.dir.join(config::FILE_NAME);
    let card_dir = args.dir.join("examples").join("cards");
    let card_path = card_dir.join("linxia.md");

    write_new(&cfg_path, assets::CONFIG_TEMPLATE, args.force)?;
    std::fs::create_dir_all(&card_dir).map_err(|e| format!("创建 {} 失败：{e}", card_dir.display()))?;
    write_new(&card_path, assets::DEFAULT_CARD_MD, args.force)?;

    if verbose {
        println!("· 已写入 {}", cfg_path.display());
        println!("· 已写入 {}", card_path.display());
    }
    println!("已生成配置与示例角色卡。接下来：");
    println!("  styx probe          看看哪些后端能用");
    println!("  styx repl           开始对话");
    println!("  styx web            在浏览器里演（把表情包放进 web/stickers）");
    println!("  styx demo           不联网先跑一遍试试");
    Ok(())
}

fn write_new(path: &std::path::Path, body: &str, force: bool) -> Result<(), String> {
    if path.exists() && !force {
        return Err(format!(
            "{} 已存在（加 --force 覆盖）",
            path.display()
        ));
    }
    std::fs::write(path, body).map_err(|e| format!("写入 {} 失败：{e}", path.display()))
}

// -------------------------------------------------------------------- card

fn cmd_card(args: &CardArgs) -> Result<(), String> {
    let card = CharacterCard::load(&args.path)
        .map_err(|e| format!("加载 {} 失败：{e}", args.path.display()))?;

    if args.json {
        println!("{}", card.to_json());
        return Ok(());
    }
    if args.normalize {
        print!("{}", card.to_markdown());
        return Ok(());
    }

    println!("角色卡：{}", args.path.display());
    println!("名字    : {}", card.name);
    println!("标识    : {}", card.namespace());
    println!("定位    : {}", one_line(&card.archetype, 50));
    println!("人格    : {}", one_line(&card.persona, 50));
    println!("口吻    : {}", one_line(&card.speech_style, 50));
    println!("示例台词: {} 条", card.speech_samples.len());
    println!("禁则    : {} 条", card.boundaries.len());
    println!("禁用词  : {} 条", card.banned_phrases.len());
    println!("关系    : {} 条", card.relations.len());
    println!("世界书  : {} 条", card.lore.len());
    println!("校验    : 通过");
    Ok(())
}

// -------------------------------------------------------------------- probe

fn cmd_probe(cfg: &Config, args: &ProbeArgs) -> Result<(), String> {
    println!("后端探测{}\n", if args.deep { "（深度）" } else { "" });
    let lines = probe::run(cfg, args.deep);
    print!("{}", probe::render(&lines));
    println!("\n{}", probe::summary(&lines));
    if lines.iter().any(|l| !l.ok) {
        println!(
            "不可用的后端不会让 Styx 起不来——内核会自动降级到内存兜底实现，\
             只是角色的记性和联想会变浅。"
        );
    }
    Ok(())
}

// --------------------------------------------------------------------- demo

/// 演示用的语料：喂给内存共现图，让兜底联想不是空转。
const DEMO_LORE: &[&str] = &[
    "母亲留下的一张旧照片夹在相册最上面那一格",
    "那张旧照片的背面有一行褪色的字",
    "相册一直放在书架最上面那一格，谁也不许碰",
    "拾光旧书店开在老城区一条没有名字的巷子里",
    "雨天的时候书店的屋顶会漏，她总是先救书再救自己",
    "陈默每周三来一次，从不买书，只看最后一排",
];

const DEMO_LINES: &[&str] = &[
    "我想看看那本相册。",
    "你可以不解释，但我想知道那张照片上是谁。",
    "外面雨大了，我帮你把窗关上吧。",
    "如果有一天你不开店了，这些书怎么办？",
    "我明天还来。",
];

fn cmd_demo(args: &DemoArgs, verbose: bool) -> Result<(), String> {
    let card = match &args.card {
        Some(p) => CharacterCard::load(p).map_err(|e| format!("加载 {} 失败：{e}", p.display()))?,
        None => CharacterCard::parse_markdown(assets::DEFAULT_CARD_MD)
            .map_err(|e| format!("内置角色卡解析失败：{e}"))?,
    };
    let seed: Vec<String> = DEMO_LORE.iter().map(|s| s.to_string()).collect();

    let wiring = Wiring::offline(&seed);
    let mut cfg = Config::default();
    cfg.character.user_name = "陈默".into();
    cfg.kernel.write_pool = Some(true);

    wiring.explain(verbose);

    let scene = Scene::new("拾光旧书店");
    let mut kernel = wiring.kernel(&cfg, card, scene)?;

    println!("\n═══ Styx 离线演示 ═══");
    println!("角色：{}", kernel.card().name);
    println!("场景：{}", kernel.scene().render());
    println!("说明：模型是 Mock，记忆与联想是内存实现，全程不联网。\n");

    for i in 0..args.turns.max(1) {
        let input = DEMO_LINES[i % DEMO_LINES.len()];
        println!("──────────────────────────────────────────────");
        println!("陈默：{input}");

        let outcome = kernel.turn(input).map_err(|e| e.to_string())?;
        println!();
        println!("{}", outcome.render());
        println!();

        if args.show_prompt {
            println!("[提示词预算] {}", outcome.report.render());
        }

        println!("[回合] {}", outcome.debug_line());
        if !outcome.recalled.is_empty() {
            println!("[召回] {} 条", outcome.recalled.len());
        }
        if !outcome.associations.is_empty() {
            let words: Vec<String> = outcome
                .associations
                .iter()
                .map(|a| a.word.clone())
                .collect();
            println!("[联想] {}", words.join(" / "));
        }
        if !outcome.scene_changed.is_empty() {
            println!("[场景] {}", outcome.scene_changed.join("；"));
        }
        for n in &outcome.notices {
            println!("[提示] {n}");
        }
        println!();
    }

    println!("──────────────────────────────────────────────");
    println!("{}", kernel.status().render());
    println!(
        "\n以上全部由内存后端完成。接入真实服务只需在 styx.toml 里填地址：\n\
         · 长期记忆 → Nebula（NEBULA_ADDR / NEBULA_PASSWORD）\n\
         · 联想     → MightBe（MIGHTBE_ADDR / MIGHTBE_NETWORK）\n\
         · 共享记忆 → MemoryPool（MEMORYPOOL_URL）\n\
         用 `styx probe` 逐个确认。"
    );
    Ok(())
}

// ---------------------------------------------------------------------- say

fn cmd_say(cfg: &Config, args: &SayArgs, verbose: bool) -> Result<(), String> {
    let text = if args.text.is_empty() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| format!("读取标准输入失败：{e}"))?;
        line.trim().to_string()
    } else {
        args.text.join(" ")
    };
    if text.is_empty() {
        return Err("没有输入内容".into());
    }

    let card = resolve_card(cfg, args.card.as_deref())?;
    let namespace = format!("{}-cli", card.namespace());
    let wiring = Wiring::build(cfg, &namespace, &seed_from_card(&card))?;
    wiring.explain(verbose);
    let mut kernel = wiring.kernel(cfg, card, cfg.scene())?;

    let outcome = kernel.turn(&text).map_err(|e| e.to_string())?;

    if args.json {
        let value = serde_json::json!({
            "ok": true,
            "turn": outcome.turn,
            "input": text,
            "speech": outcome.reply.speech,
            "actions": outcome.reply.actions,
            "thoughts": outcome.reply.thoughts,
            "state": kernel.state().render(),
            "scene": kernel.scene().render(),
            "scene_changed": outcome.scene_changed,
            "recalled": outcome.recalled.iter().map(|r| serde_json::json!({
                "id": r.id, "text": r.text, "score": r.score, "tags": r.tags, "origin": r.origin,
            })).collect::<Vec<_>>(),
            "associations": outcome.associations.iter().map(|a| serde_json::json!({
                "word": a.word, "score": a.score, "confidence": a.confidence,
            })).collect::<Vec<_>>(),
            "audit": {
                "violations": outcome.audit.violations,
                "warnings": outcome.audit.warnings,
            },
            "retries": outcome.retries,
            "memory_written": outcome.memory_written,
            "report": outcome.report.render(),
            "notices": outcome.notices,
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return Ok(());
    }

    let body = outcome.render();
    if !body.is_empty() {
        println!("{body}");
    }
    if !outcome.scene_changed.is_empty() {
        println!("【场景】{}", outcome.scene_changed.join("；"));
    }
    for n in &outcome.notices {
        println!("· {n}");
    }
    if args.report {
        eprintln!("· {}", outcome.debug_line());
        eprintln!("· {}", outcome.report.render());
    }
    Ok(())
}

// --------------------------------------------------------------------- repl

fn cmd_repl(cfg: &Config, args: &ReplArgs, verbose: bool) -> Result<(), String> {
    if args.probe {
        println!("{}", probe::render(&probe::run(cfg, false)));
        println!();
    }
    let card = resolve_card(cfg, args.card.as_deref())?;
    let namespace = format!("{}-repl", card.namespace());
    let wiring = Wiring::build(cfg, &namespace, &seed_from_card(&card))?;
    wiring.explain(verbose);
    let mut kernel = wiring.kernel(cfg, card, cfg.scene())?;
    repl::run(&mut kernel, cfg, args.debug)?;
    Ok(())
}

// -------------------------------------------------------------------- serve

/// 服务端的内核工厂：每个会话一个独立内核（独立记忆命名空间）。
struct Factory {
    cfg: Config,
    card_path: Option<PathBuf>,
    /// 表情包目录。`styx web` 会挂上它，`styx serve` 默认没有——
    /// 终端客户端发不出一张图，给它加载目录只是徒增困惑。
    stickers: Option<Arc<StickerCatalog>>,
}

impl styx_server::KernelFactory for Factory {
    fn create(&self, session_id: &str) -> styx_core::Result<Kernel> {
        let card = resolve_card(&self.cfg, self.card_path.as_deref())
            .map_err(StyxError::Other)?;
        let namespace = format!("{}-{}", card.namespace(), sanitize(session_id));
        let wiring = Wiring::build(&self.cfg, &namespace, &seed_from_card(&card))
            .map_err(StyxError::Other)?;
        let wiring = match &self.stickers {
            Some(c) => wiring.with_stickers(Arc::clone(c)),
            None => wiring,
        };
        for note in &wiring.notes {
            eprintln!("[{}] · {note}", session_id);
        }
        wiring
            .kernel(&self.cfg, card, self.cfg.scene())
            .map_err(StyxError::Other)
    }

    fn describe(&self) -> String {
        let stickers = match &self.stickers {
            Some(c) => format!("，表情包 {} 张", c.len()),
            None => String::new(),
        };
        format!(
            "styx-cli（模型后端 {}，角色卡 {}{}）",
            self.cfg.llm.backend,
            self.card_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "内置示例".into()),
            stickers
        )
    }
}

fn cmd_serve(cfg: &Config, args: &ServeArgs, verbose: bool) -> Result<(), String> {
    if verbose {
        for line in probe::run(cfg, false) {
            eprintln!("· {}", line.render());
        }
    }
    let factory = Factory {
        cfg: cfg.clone(),
        card_path: args.card.clone(),
        stickers: None,
    };
    let server = Arc::new(styx_server::Server::new(Arc::new(factory)).with_default_session(&args.session));
    eprintln!("提示：客户端用一行一个 JSON 对话，例如 echo '{{\"op\":\"ping\"}}' | nc 127.0.0.1 7879");
    server.bind_and_run(&args.addr).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------- web

fn cmd_web(cfg: &Config, args: &WebArgs, verbose: bool) -> Result<(), String> {
    let addr = args
        .addr
        .clone()
        .unwrap_or_else(|| cfg.web.addr.clone());
    let session = args
        .session
        .clone()
        .unwrap_or_else(|| cfg.web.session.clone());

    if verbose {
        for line in probe::run(cfg, false) {
            eprintln!("· {}", line.render());
        }
    }

    // 表情包目录：命令行 > 配置 > 当前目录的 web/stickers > 仓库内的默认目录
    let candidates = sticker_candidates(cfg, args.stickers.as_deref());
    let dir = match styx_web::locate_sticker_dir(&candidates) {
        Some(d) => d,
        None => {
            let first = candidates.first().cloned().unwrap_or_default();
            eprintln!(
                "· 表情包目录 {} 不存在，界面会是空的（放进 PNG 再启动即可）",
                first.display()
            );
            first
        }
    };
    let catalog = styx_web::load_catalog(&dir);
    if catalog.is_empty() {
        eprintln!("· 表情包：0 张（{}）", dir.display());
    } else {
        eprintln!("· 表情包：{} 张（{}）", catalog.len(), dir.display());
    }

    let factory = Factory {
        cfg: cfg.clone(),
        card_path: args.card.clone(),
        stickers: Some(Arc::clone(&catalog)),
    };
    let server = Arc::new(
        styx_server::Server::new(Arc::new(factory)).with_default_session(&session),
    );
    let web = Arc::new(
        styx_web::WebServer::new(server, catalog, dir)
            .with_default_session(&session)
            .with_verbose(args.request_log || verbose),
    );

    if args.open || cfg.web.open {
        // 等监听真正起来再开浏览器，否则会开出"无法访问"
        let url = format!("http://{}", browser_host(&addr));
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(700));
            styx_web::open_browser(&url);
        });
    }

    web.bind_and_run(&addr)
        .map_err(|e| format!("无法监听 {addr}：{e}"))
}

/// 监听地址 → 浏览器能访问的地址（`0.0.0.0` 换成回环）。
fn browser_host(addr: &str) -> String {
    if let Some(port) = addr.strip_prefix("0.0.0.0") {
        format!("127.0.0.1{port}")
    } else if let Some(port) = addr.strip_prefix("[::]") {
        format!("127.0.0.1{port}")
    } else {
        addr.to_string()
    }
}

/// 表情包目录的候选位置（按优先级）。
fn sticker_candidates(cfg: &Config, explicit: Option<&std::path::Path>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        v.push(p.to_path_buf());
    }
    if !cfg.web.stickers.as_os_str().is_empty() {
        v.push(cfg.web.stickers.clone());
    }
    v.push(PathBuf::from("web/stickers"));
    // 在仓库里直接 `cargo run -p styx -- web` 时能用上自带的示例表情包
    v.push(PathBuf::from("crates/styx-web/web/stickers"));
    v.dedup();
    v
}

// -------------------------------------------------------------------- 公共

/// 角色卡优先级：命令行 > 配置文件 > 内置示例。
fn resolve_card(cfg: &Config, explicit: Option<&std::path::Path>) -> Result<CharacterCard, String> {
    if let Some(p) = explicit {
        return CharacterCard::load(p).map_err(|e| format!("加载角色卡 {} 失败：{e}", p.display()));
    }
    if let Some(p) = &cfg.character.card {
        return CharacterCard::load(p).map_err(|e| format!("加载角色卡 {} 失败：{e}", p.display()));
    }
    CharacterCard::parse_markdown(assets::DEFAULT_CARD_MD)
        .map_err(|e| format!("内置角色卡解析失败：{e}"))
}

/// 从角色卡里取几段文本，作为内存共现图的初始语料。
fn seed_from_card(card: &CharacterCard) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in [&card.background, &card.persona, &card.archetype, &card.speech_style] {
        if !s.trim().is_empty() {
            out.push(s.clone());
        }
    }
    for l in &card.lore {
        out.push(l.text.clone());
    }
    for r in &card.relations {
        out.push(format!("{} 与 {} 是{}关系", card.name, r.target, r.kind));
    }
    out
}

/// 只保留文件名安全的字符，避免会话名跑到存储键里变成意外路径。
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_matches('_').to_string();
    if cleaned.is_empty() {
        "default".into()
    } else {
        cleaned
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

#[cfg(test)]
mod tests {
    use super::*;
    use styx_core::KernelConfig;

    #[test]
    fn builtin_card_is_valid() {
        let card = CharacterCard::parse_markdown(assets::DEFAULT_CARD_MD).unwrap();
        card.validate().unwrap();
        assert_eq!(card.name, "林夏");
        assert!(!card.boundaries.is_empty());
        assert!(!card.speech_samples.is_empty());
        assert!(!card.lore.is_empty());
    }

    #[test]
    fn builtin_card_round_trips_through_markdown() {
        let a = CharacterCard::parse_markdown(assets::DEFAULT_CARD_MD).unwrap();
        let b = CharacterCard::parse_markdown(&a.to_markdown()).unwrap();
        assert_eq!(a.name, b.name);
        assert_eq!(a.boundaries, b.boundaries);
        assert_eq!(a.banned_phrases, b.banned_phrases);
        assert_eq!(a.lore.len(), b.lore.len());
        assert_eq!(a.relations.len(), b.relations.len());
    }

    #[test]
    fn config_template_is_parseable() {
        let cfg: Config = toml::from_str(assets::CONFIG_TEMPLATE).unwrap();
        assert_eq!(cfg.llm.backend, "auto");
        assert_eq!(cfg.character.location, "拾光旧书店");
        assert!(cfg.tools.builtin);
        // 模板里的 [web] 必须和服务默认值一致，否则生成配置后行为会悄悄变掉
        assert_eq!(cfg.web.addr, Config::default().web.addr);
        assert_eq!(cfg.web.session, "web");
        assert!(!cfg.web.open);
        // 模板里的内核调参也不该和代码默认值打架
        assert_eq!(
            cfg.kernel.assoc_seeds,
            Some(KernelConfig::default().assoc_seeds)
        );
    }

    #[test]
    fn sanitize_keeps_session_ids_path_safe() {
        assert_eq!(sanitize("林夏线"), "林夏线");
        assert_eq!(sanitize("a/b\\c"), "a_b_c");
        assert_eq!(sanitize("///"), "default");
        assert_eq!(sanitize(""), "default");
    }

    #[test]
    fn seed_from_card_pulls_background_lore_and_relations() {
        let card = CharacterCard::parse_markdown(assets::DEFAULT_CARD_MD).unwrap();
        let seed = seed_from_card(&card);
        assert!(seed.iter().any(|s| s.contains("母亲留下的")));
        assert!(seed.iter().any(|s| s.contains("陈默")));
    }

    #[test]
    fn one_line_truncates() {
        assert_eq!(one_line("abc\ndef", 20), "abc def");
        assert_eq!(one_line("abcdef", 3), "abc…");
    }

    #[test]
    fn browser_host_rewrites_wildcard_binds() {
        assert_eq!(browser_host("0.0.0.0:8770"), "127.0.0.1:8770");
        assert_eq!(browser_host("[::]:8770"), "127.0.0.1:8770");
        assert_eq!(browser_host("127.0.0.1:8080"), "127.0.0.1:8080");
    }

    #[test]
    fn sticker_candidates_put_the_explicit_dir_first_and_dedupe() {
        let cfg = Config::default();
        let v = sticker_candidates(&cfg, Some(std::path::Path::new("/tmp/emoji")));
        assert_eq!(v[0], PathBuf::from("/tmp/emoji"));

        // 配置默认值与候选列表里那一档重名，不该出现两遍
        let dupes = v
            .iter()
            .filter(|p| *p == std::path::Path::new("web/stickers"))
            .count();
        assert_eq!(dupes, 1, "{v:?}");
        assert!(v.iter().any(|p| p.ends_with("styx-web/web/stickers")));
    }
}
