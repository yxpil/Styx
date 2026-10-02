//! 交互式角色扮演循环。
//!
//! 一条输入就是一次 [`styx_core::Kernel::turn`]；以 `/` 开头的行走斜杠命令，
//! 不触发生成。这样"说话"和"操作"不会互相干扰——角色扮演里最常见的事故
//! 就是用户想查一下状态，结果角色把 `[/state]` 当台词回应了。

use std::io::{BufRead, IsTerminal, Write};

use styx_core::{Kernel, Session, TurnOutcome};

use crate::config::Config;

/// REPL 的运行状态。
struct Repl<'a> {
    kernel: &'a mut Kernel,
    cfg: &'a Config,
    debug: bool,
    last: Option<TurnOutcome>,
    quit: bool,
}

/// 进入循环。返回 `Ok(true)` 表示用户主动退出。
pub fn run(kernel: &mut Kernel, cfg: &Config, debug: bool) -> Result<bool, String> {
    let interactive = std::io::stdin().is_terminal();

    banner(kernel, cfg, debug, interactive);

    let stdin = std::io::stdin();
    let mut line = String::new();
    let mut state = Repl {
        kernel,
        cfg,
        debug,
        last: None,
        quit: false,
    };

    loop {
        if interactive {
            print!("> ");
            let _ = std::io::stdout().flush();
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => return Err(format!("读取输入失败：{e}")),
        }
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }

        if text.starts_with('/') {
            if let Err(e) = state.command(&text) {
                println!("！ {e}");
            }
        } else if let Err(e) = state.say(&text) {
            println!("！ 本回合失败：{e}");
        }

        if state.quit {
            break;
        }
    }

    if interactive {
        println!();
    }
    Ok(state.quit)
}

fn banner(kernel: &Kernel, cfg: &Config, _debug: bool, interactive: bool) {
    if !interactive {
        return;
    }
    println!("┌─ Styx · 角色扮演内核 ─────────────────────────────");
    println!("│ 角色：{}（{}）", kernel.card().name, short(&kernel.card().persona, 28));
    println!("│ 场景：{}", short(&kernel.scene().render(), 36));
    println!("│ 你  ：{}", cfg.character.user_name);
    println!("│ 输入 /help 看命令，/quit 退出");
    println!("└──────────────────────────────────────────────────");
}

impl Repl<'_> {
    // ------------------------------------------------------------ 说话

    fn say(&mut self, text: &str) -> Result<(), String> {
        let outcome = self.kernel.turn(text).map_err(|e| e.to_string())?;
        println!();
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
        if self.debug {
            println!("· {}", outcome.debug_line());
            if !outcome.recalled.is_empty() {
                let names: Vec<&str> = outcome.recalled.iter().map(|r| r.text.as_str()).collect();
                println!("· 召回：{}", short(&names.join(" / "), 90));
            }
            if !outcome.associations.is_empty() {
                let words: Vec<String> = outcome
                    .associations
                    .iter()
                    .map(|a| format!("{}({:.2})", a.word, a.score))
                    .collect();
                println!("· 联想：{}", short(&words.join(" "), 90));
            }
        }
        println!();
        self.last = Some(outcome);
        Ok(())
    }

    // ------------------------------------------------------------ 命令

    fn command(&mut self, line: &str) -> Result<(), String> {
        let mut parts = line.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
        let rest = parts.next().unwrap_or("").trim().to_string();

        match cmd.as_str() {
            "/help" | "/?" | "/h" => self.help(),
            "/quit" | "/exit" | "/q" => {
                println!("（散场。）");
                self.quit = true;
            }
            "/state" => println!("{}", self.kernel.state().render()),
            "/scene" => println!("{}", self.kernel.scene().render()),
            "/card" => println!("{}", self.kernel.card().to_markdown()),
            "/status" => println!("{}", self.kernel.status().render()),
            "/events" => self.events(&rest),
            "/tools" => self.tools(),
            "/call" => self.call(&rest)?,
            "/recall" => self.recall(&rest)?,
            "/assoc" => self.assoc(&rest)?,
            "/remember" => self.remember(&rest)?,
            "/observe" => self.observe(&rest)?,
            "/raw" => match &self.last {
                Some(o) => println!("{}", o.reply.raw),
                None => println!("（还没有回合）"),
            },
            "/report" => match &self.last {
                Some(o) => {
                    println!("{}", o.debug_line());
                    println!("{}", o.report.render());
                    if !o.audit.violations.is_empty() {
                        println!("违规：{}", o.audit.violations.join(" / "));
                    }
                    if !o.audit.warnings.is_empty() {
                        println!("提醒：{}", o.audit.warnings.join(" / "));
                    }
                }
                None => println!("（还没有回合）"),
            },
            "/debug" => {
                self.debug = match rest.to_ascii_lowercase().as_str() {
                    "" => !self.debug,
                    "on" | "1" | "true" | "yes" => true,
                    "off" | "0" | "false" | "no" => false,
                    other => return Err(format!("/debug 只接受 on / off，收到 {other}")),
                };
                println!("调试输出：{}", if self.debug { "开" } else { "关" });
            }
            "/reset" => {
                let card = self.kernel.card().clone();
                let scene = self.kernel.scene().clone();
                *self.kernel.session_mut() = Session::new(card, scene);
                self.last = None;
                println!("（会话已重置，角色仍在原地。）");
            }
            "/save" => self.save(&rest)?,
            "/load" => self.load(&rest)?,
            "/probe" => println!("{}", crate::probe::render(&crate::probe::run(self.cfg, false))),
            other => return Err(format!("未知命令 {other}（输入 /help 看全部）")),
        }
        Ok(())
    }

    fn help(&self) {
        println!(
            r#"直接输入文字即为台词，会走完整回合（召回→联想→装配→生成→守护→结算）。
以 / 开头的是命令，不触发生成：

  /help              这份帮助
  /state             当前动态状态（心情/精力/好感/信任/意图）
  /scene             当前场景
  /card              当前角色卡（Markdown）
  /status            各后端状态与降级情况
  /events [n]        最近 n 条事件（默认 10）
  /tools             可用工具列表
  /call T {{json}}   调用工具，例如 /call dice {{"sides":20}}
  /recall Q [n]      只做长期记忆召回，不生成
  /assoc W [n]       只做联想发散
  /remember 文本     手动写入一条长期记忆
  /observe 文本      注入一条系统观察
  /raw               上一次模型返回的原文
  /report            上一次的回合报告
  /debug [on|off]    切换调试输出
  /reset             重置会话（保留端口连接）
  /save FILE         把会话快照写成 JSON
  /load FILE         从 JSON 快照续演
  /probe             探测各后端可用性
  /quit              退出"#
        );
    }

    fn events(&self, rest: &str) {
        let n: usize = rest.parse().unwrap_or(10);
        let events = &self.kernel.session().transcript;
        let start = events.len().saturating_sub(n);
        if events.is_empty() {
            println!("（还没有事件）");
            return;
        }
        for e in &events[start..] {
            println!("{:>3} {}", e.seq, e.render_line());
        }
    }

    fn tools(&self) {
        let Some(port) = self.kernel.tools() else {
            println!("（未启用工具端口）");
            return;
        };
        let list = port.list();
        if list.is_empty() {
            println!("（没有可用工具）");
            return;
        }
        for t in list {
            println!("  {:<16} [{}] {}", t.name, t.origin, t.description);
        }
    }

    fn call(&self, rest: &str) -> Result<(), String> {
        let Some(port) = self.kernel.tools() else {
            return Err("未启用工具端口".into());
        };
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        if name.is_empty() {
            return Err("用法：/call <工具名> [JSON 入参]".into());
        }
        let args: serde_json::Value = if args.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(args).map_err(|e| format!("入参不是合法 JSON：{e}"))?
        };
        match port.invoke(name, args) {
            Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())),
            Err(e) => println!("！ 调用失败：{e}"),
        }
        Ok(())
    }

    fn recall(&self, rest: &str) -> Result<(), String> {
        let (query, limit) = split_limit(rest, 5);
        if query.is_empty() {
            return Err("用法：/recall <查询> [条数]".into());
        }
        match self.kernel.recall(&query, limit) {
            Ok(hits) if hits.is_empty() => println!("（想不起来。）"),
            Ok(hits) => {
                for h in hits {
                    println!(
                        "  {:.3} [{}] {}",
                        h.score,
                        short(&h.tags.join(","), 18),
                        short(&h.text, 100)
                    );
                }
            }
            Err(e) => println!("！ 召回失败：{e}"),
        }
        Ok(())
    }

    fn assoc(&self, rest: &str) -> Result<(), String> {
        let (seed, limit) = split_limit(rest, 8);
        if seed.is_empty() {
            return Err("用法：/assoc <词> [条数]".into());
        }
        match self.kernel.associate(&seed, limit) {
            Ok(hits) if hits.is_empty() => println!("（想不起来。这就是 MightBe 的弃判语义。）"),
            Ok(hits) => {
                for h in hits {
                    let ev = if h.evidence.is_empty() {
                        String::new()
                    } else {
                        format!("  ← {}", short(&h.evidence.join(" / "), 60))
                    };
                    println!("  {:.3} {:.2} {}{}", h.score, h.confidence, h.word, ev);
                }
            }
            Err(e) => println!("！ 联想失败：{e}"),
        }
        Ok(())
    }

    fn remember(&self, rest: &str) -> Result<(), String> {
        if rest.is_empty() {
            return Err("用法：/remember <文本>".into());
        }
        let tags = vec!["手动".to_string(), self.kernel.card().namespace().to_string()];
        match self.kernel.remember(rest, &tags, 0.8) {
            Ok(id) => println!("（记住了，id={id}）"),
            Err(e) => println!("！ 写入失败：{e}"),
        }
        Ok(())
    }

    fn observe(&mut self, rest: &str) -> Result<(), String> {
        if rest.is_empty() {
            return Err("用法：/observe <文本>".into());
        }
        match self.kernel.observe(rest, 0.7) {
            Ok(()) => println!("（已注入观察）"),
            Err(e) => println!("！ 注入失败：{e}"),
        }
        Ok(())
    }

    fn save(&self, rest: &str) -> Result<(), String> {
        let path = if rest.is_empty() {
            format!("styx-session-{}.json", self.kernel.session().id)
        } else {
            rest.to_string()
        };
        std::fs::write(&path, self.kernel.session().snapshot_json())
            .map_err(|e| format!("写入 {path} 失败：{e}"))?;
        println!("（已保存到 {path}）");
        Ok(())
    }

    fn load(&mut self, rest: &str) -> Result<(), String> {
        if rest.is_empty() {
            return Err("用法：/load <快照文件>".into());
        }
        let text = std::fs::read_to_string(rest).map_err(|e| format!("读取 {rest} 失败：{e}"))?;
        let session =
            Session::from_snapshot_json(&text).map_err(|e| format!("快照无法解析：{e}"))?;
        let turn = session.state.turn;
        *self.kernel.session_mut() = session;
        self.last = None;
        println!("（已续演：第 {turn} 回合之后）");
        Ok(())
    }
}

/// 把 `"词 5"` 拆成 `("词", 5)`；数字缺失时用默认值。
fn split_limit(rest: &str, default: usize) -> (String, usize) {
    let rest = rest.trim();
    match rest.rsplit_once(char::is_whitespace) {
        Some((head, tail)) => match tail.parse::<usize>() {
            Ok(n) => (head.trim().to_string(), n.max(1)),
            Err(_) => (rest.to_string(), default),
        },
        None => (rest.to_string(), default),
    }
}

fn short(s: &str, max: usize) -> String {
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

    #[test]
    fn split_limit_handles_both_forms() {
        assert_eq!(split_limit("旧照片 3", 5), ("旧照片".to_string(), 3));
        assert_eq!(split_limit("旧 照 片", 5), ("旧 照 片".to_string(), 5));
        assert_eq!(split_limit("相册", 5), ("相册".to_string(), 5));
        assert_eq!(split_limit("相册 0", 5), ("相册".to_string(), 1));
    }

    #[test]
    fn short_flattens_and_truncates() {
        assert_eq!(short("a\nb", 5), "a b");
        assert_eq!(short("abcdefg", 4), "abcd…");
    }
}
