//! 对着**真的跑起来的** `services/vision` sidecar 做一次端到端验证。
//!
//! 这个测试默认跳过（`#[ignore]`），因为它需要一个已经启动的服务和一张
//! 真实的图片，而这两样都不该是 `cargo test` 的前提条件。
//!
//! ## 怎么跑
//!
//! ```bash
//! # 一个终端：
//! cd services/vision && python app.py           # 默认 127.0.0.1:8420
//!
//! # 另一个终端：
//! STYX_VISION_TEST_IMAGE=/path/to/photo.jpg \
//!   cargo test -p styx-vision --features http --test live_sidecar -- --ignored --nocapture
//! ```
//!
//! ## 它验证的是什么
//!
//! 单元测试能证明"我解析得对"，但证明不了"对端真的这么说话"。这个测试
//! 把两者接起来跑一遍，并且**把合并后的描述原样打印出来**——因为最终的
//! 质检标准不是"字段解析正确"，而是"这段文字交给语言模型能不能用"。
//!
//! 顺带验证一条容易被忽略的性质：**两个后端都挂掉时，描述依然可用**。
//! 这一点是整套设计的地基，值得对着真实进程验一遍，而不是只对着 stub 验。

#![cfg(feature = "http")]

use std::time::Duration;

use styx_vision::{Composer, RemoteTagger, RemoteVision};

/// 从环境变量取服务地址，默认和 `app.py` 的默认端口一致。
fn base() -> String {
    std::env::var("STYX_VISION_BASE").unwrap_or_else(|_| "http://127.0.0.1:8420".to_string())
}

/// 取一张真实图片。没设置就跳过——不要用合成图冒充，
/// 那只会让这个测试变成"我以为我验证了"。
fn sample_image() -> Option<Vec<u8>> {
    let path = std::env::var("STYX_VISION_TEST_IMAGE").ok()?;
    match std::fs::read(&path) {
        Ok(bytes) if !bytes.is_empty() => Some(bytes),
        Ok(_) => {
            eprintln!("STYX_VISION_TEST_IMAGE 指向的文件是空的：{path}");
            None
        }
        Err(e) => {
            eprintln!("读不到 STYX_VISION_TEST_IMAGE（{path}）：{e}");
            None
        }
    }
}

#[test]
#[ignore = "需要一个跑起来的 services/vision sidecar"]
fn the_whole_chain_produces_a_usable_description() {
    let Some(image) = sample_image() else {
        eprintln!("跳过：没有设置 STYX_VISION_TEST_IMAGE");
        return;
    };

    // 超时放松到 60 秒：第一次加载 onnxruntime 会慢一些，
    // 而这是人手动跑的测试，不是每回合都要忍受的延迟。
    let composer = Composer::new()
        .with(Box::new(
            RemoteVision::new(base()).with_timeout(Duration::from_secs(60)),
        ))
        .with(Box::new(
            RemoteTagger::new(base()).with_timeout(Duration::from_secs(60)),
        ));

    let (facts, text, used) = composer.describe(&image);

    println!("\n=== 用到的后端：{:?} ===", used);
    println!(
        "=== 本地事实：{}×{} 格式={} 像素解出来了={} ===",
        facts.width, facts.height, facts.format, facts.decoded
    );
    println!("=== 目标检测：{} 个 ===", facts.objects.len());
    for o in &facts.objects {
        println!(
            "  {}（{:.0}% 把握，{}，占画面 {:.0}%）",
            o.display_name(),
            o.confidence * 100.0,
            o.position_label(),
            o.area * 100.0
        );
    }
    println!("=== 反推标签：{} 个 ===", facts.tags.len());
    for t in &facts.tags {
        println!(
            "  {} [{:?}] {:.2}（来源 {}）",
            t.display(),
            t.group(),
            t.score,
            t.source
        );
    }
    println!("=== 交给语言模型的描述 ===\n{text}\n");

    // ---- 与格式无关的底线 ----
    assert!(!text.is_empty(), "任何时候都要有描述");
    assert!(text.contains('×'), "描述里至少要有尺寸这一条硬事实：{text}");

    // ---- 只在像素真的解出来时才成立的 ----
    // 刻意按格式分支而不是一律断言 `decoded`：这个测试要能在
    // "只给了 JPEG、而本地解码器还不支持它"的情况下依然跑通，
    // 否则它会变成一个假的红灯，掩盖真正的问题。
    if facts.decoded {
        assert!(!facts.dominant.is_empty(), "解出像素就必须给出主色：{text}");
        assert!(facts.mean_luma > 0.0, "要有平均亮度：{text}");
    } else {
        println!(
            "注意：本地没解出 {} 的像素，描述里只剩尺寸——如果是 JPEG，\
             这是解码器的缺口，不是链路问题",
            facts.format
        );
    }

    // ---- 后端真跑通了才有的话 ----
    if !facts.objects.is_empty() {
        assert!(
            text.contains("能认出的东西"),
            "有检测结果就必须出现在描述里：{text}"
        );
    }
    if !facts.tags.is_empty() {
        assert!(
            text.contains("反推出来的画面标签"),
            "有标签就必须如实说是反推的，不能混进事实陈述：{text}"
        );
    }

    // 后端一个都没连上时，至少要说清楚为什么，而不是假装"画面里什么都没有"
    if used.is_empty() {
        assert!(
            !facts.notes.is_empty(),
            "后端不可用时必须留下说明，而不是安静地什么都不说：{text}"
        );
    }
}

#[test]
#[ignore = "需要一个跑起来的 services/vision sidecar"]
fn a_dead_sidecar_still_yields_a_description() {
    let Some(image) = sample_image() else {
        return;
    };
    // 指向一个确定没人监听的端口
    let composer = Composer::new()
        .with(Box::new(
            RemoteVision::new("http://127.0.0.1:9").with_timeout(Duration::from_millis(800)),
        ))
        .with(Box::new(
            RemoteTagger::new("http://127.0.0.1:9").with_timeout(Duration::from_millis(800)),
        ));

    let (facts, text, used) = composer.describe(&image);
    assert!(used.is_empty(), "连不上就不该有后端被记成'用过'");
    assert!(facts.objects.is_empty() && facts.tags.is_empty());
    assert!(
        facts.notes.len() >= 2,
        "两个后端各该留一条说明：{:?}",
        facts.notes
    );
    assert!(
        !text.is_empty(),
        "**这是整套设计的地基**：后端全挂也要有描述"
    );
    assert!(
        facts.width > 0 && facts.height > 0,
        "尺寸这一条硬事实永远拿得到，哪怕只认识文件头"
    );
    println!("\n=== 后端全挂时降级出来的描述 ===\n{text}\n");
}
