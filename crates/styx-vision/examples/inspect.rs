//! 把一张图喂给 styx-vision，看看它到底「看见」了什么。
//!
//! ```bash
//! # 只用本地事实（零第三方依赖）
//! cargo run --example inspect -- photo.jpg
//!
//! # 接上 sidecar（见 services/vision），拿到检测 + 属性反推
//! cargo run --example inspect --features http -- photo.jpg --base http://127.0.0.1:8420
//! ```
//!
//! 刻意不用 clap：这个 crate 的卖点之一就是零第三方依赖，示例不该反过来
//! 给它加一个依赖。参数只有两个，手写解析比引一个框架更短。

use std::process::ExitCode;

#[cfg(feature = "http")]
use styx_vision::Composer;
use styx_vision::{analyze, ImageFacts};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut path: Option<String> = None;
    let mut base: Option<String> = None;
    let mut raw = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--base" => {
                i += 1;
                match args.get(i) {
                    Some(v) => base = Some(v.clone()),
                    None => {
                        eprintln!("--base 后面要跟一个地址，例如 http://127.0.0.1:8420");
                        return ExitCode::from(2);
                    }
                }
            }
            "--raw" => raw = true,
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("不认识的参数：{other}");
                print_help();
                return ExitCode::from(2);
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }

    let Some(path) = path else {
        print_help();
        return ExitCode::from(2);
    };

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读不了 {path}：{e}");
            return ExitCode::from(1);
        }
    };

    println!("文件      {path}");
    println!("大小      {} 字节（{}）", bytes.len(), human(bytes.len()));

    if base.is_some() && !cfg!(feature = "http") {
        eprintln!();
        eprintln!("提示：你带了 --base，但这个构建没开 `http` feature，远程后端不会生效。");
        eprintln!("      用 `cargo run --example inspect --features http -- ...` 重跑。");
    }

    let (facts, description) = run(&bytes, base.as_deref());

    println!();
    print_facts(&facts);

    println!();
    println!("================ 交给语言模型的描述 ================");
    println!("{description}");
    println!("====================================================");

    if !facts.objects.is_empty() {
        println!();
        println!("检测到的物体（{}）", facts.objects.len());
        for o in &facts.objects {
            let name = if o.label_zh.is_empty() {
                o.label.clone()
            } else {
                format!("{}（{}）", o.label_zh, o.label)
            };
            let where_ = describe_box(&o.r#box);
            println!(
                "  {:<16} {:.3}  占画面 {:.0}%{}",
                name,
                o.confidence,
                o.area * 100.0,
                where_
            );
        }
    }

    if !facts.tags.is_empty() {
        println!();
        println!("反推出来的标签（{}）", facts.tags.len());
        for group in styx_vision::TagGroup::ALL {
            let hit: Vec<&styx_vision::TagHit> =
                facts.tags.iter().filter(|t| t.group() == group).collect();
            if hit.is_empty() {
                continue;
            }
            let list: Vec<String> = hit
                .iter()
                .map(|t| format!("{} {:.3}", t.display(), t.score))
                .collect();
            println!("  {:<8} {}", group.label(), list.join("、"));
        }
        for c in styx_vision::tags::caveats(&facts.tags) {
            println!("  提醒     {c}");
        }
    }

    if raw {
        println!();
        println!("原始结构（{:?}）", facts);
    }

    ExitCode::SUCCESS
}

/// 跑完整链路，返回事实与描述。
///
/// 没有 `--base`（或没开 `http` feature）时只用本地事实——这条路径永远可用。
fn run(bytes: &[u8], base: Option<&str>) -> (ImageFacts, String) {
    #[cfg(feature = "http")]
    if let Some(base) = base {
        use styx_vision::{RemoteTagger, RemoteVision};
        let composer = Composer::new()
            .with(Box::new(RemoteVision::new(base)))
            .with(Box::new(RemoteTagger::new(base)));
        let (facts, text, used) = composer.describe(bytes);
        if !used.is_empty() {
            eprintln!("（用到的后端：{}）", used.join("、"));
        }
        return (facts, text);
    }

    #[cfg(not(feature = "http"))]
    let _ = base;

    let facts = analyze(bytes);
    let text = facts.describe();
    (facts, text)
}

fn print_facts(f: &ImageFacts) {
    let fmt = if f.format.is_empty() {
        "认不出".to_string()
    } else {
        f.format.to_uppercase()
    };
    println!("尺寸      {}×{}  {}", f.width, f.height, fmt);

    if !f.decoded {
        println!("像素      没能解出来（后面的数值都没有意义）");
        return;
    }

    println!("亮度      {:.2}   对比度 {:.2}", f.mean_luma, f.contrast);
    println!(
        "饱和      {:.2}   色彩丰富度 {:.2}   边缘密度 {:.2}",
        f.saturation, f.colorfulness, f.edge_density
    );
    println!(
        "色调      {}{}",
        f.hue_family,
        if f.grayscale {
            "（基本是灰度）"
        } else {
            ""
        }
    );
    if !f.dominant.is_empty() {
        println!("主色      {}", f.dominant.join("、"));
    }
}

/// 把归一化坐标翻成一句人话。检测框只有编好坐标才有用——
/// 「左上角」「画面中央」这种说法比四个浮点数好读得多。
fn describe_box(b: &[f32]) -> String {
    if b.len() < 4 {
        return String::new();
    }
    let (x1, y1, x2, y2) = (b[0], b[1], b[2], b[3]);
    let cx = (x1 + x2) / 2.0;
    let cy = (y1 + y2) / 2.0;

    let h = if cx < 0.34 {
        "左侧"
    } else if cx > 0.66 {
        "右侧"
    } else {
        "水平居中"
    };
    let v = if cy < 0.34 {
        "偏上"
    } else if cy > 0.66 {
        "偏下"
    } else {
        "垂直居中"
    };
    format!("（{h}、{v}）")
}

fn human(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1024.0 / 1024.0)
    }
}

fn print_help() {
    eprintln!("用法：inspect <图片路径> [--base http://127.0.0.1:8420] [--raw]");
    eprintln!();
    eprintln!("  --base <地址>   接上远程后端（需要 --features http）");
    eprintln!("  --raw           额外打印原始 ImageFacts 结构");
}
