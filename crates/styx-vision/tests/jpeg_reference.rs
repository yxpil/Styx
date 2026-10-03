//! 用 Pillow（libjpeg）生成的参考数据逐像素校验基线 JPEG 解码器。
//!
//! 参考数据由 `services/vision/make_fixtures.py` 生成，说明见那个脚本的头部。
//!
//! ## 判定标准是怎么定的
//!
//! JPEG 解码**不应该**逐比特相等：不同的 IDCT 实现（浮点 / 整数近似）、
//! 不同的色度升采样算法，都会让结果差 1~3 个色阶。这是规范允许的。
//!
//! 但"允许的差异"和"实现错了"在数值上分得很开：
//!
//! | 错误 | 典型表现 |
//! |---|---|
//! | DC 差分忘了累加 | 整块整块地偏色，均值差几十 |
//! | 升采样比例算错 | 彩边糊开，边缘处差上百 |
//! | 忘了 0xFF00 填充 | 后半张图全是噪点，最大差 255 |
//! | IDCT 电平偏移漏了 | 整体偏暗，均值差 20 以上 |
//!
//! 所以判据是"**平均差很小** + **极端差只出现在少数像素**"：
//! 平均差卡在 2 以内（真正的实现错误一定会超过），而允许个别像素
//! 差得多（硬边上的舍入分歧是正常的）。

use std::path::{Path, PathBuf};

use styx_vision::{decode, dimensions, sniff, Bitmap, DecodeError, Format, Rgb};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("jpeg")
}

/// 只在测试里用的极简 JSON 取值。
///
/// 不为一个测试引入 serde_json 之外的东西——但 `serde_json` 已经是
/// 正式依赖了（`parse_objects` 要用），所以直接用。
fn read_index(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("读不到 index.json");
    serde_json::from_str(&text).expect("index.json 不是合法 JSON")
}

struct Diff {
    max: i32,
    mean: f64,
    /// 差超过 8 的像素占比。用来区分"整体错"和"边缘舍入"。
    outliers: f64,
    worst: Option<(usize, usize, Rgb, Rgb)>,
    /// 带符号的均值差（按通道）。均匀的正/负偏移指向 DC 或电平偏移问题，
    /// 而随机分布的偏差才可能是 IDCT 舍入。
    signed: [f64; 3],
    hist: [usize; 9],
}

fn compare(got: &Bitmap, want: &[u8], width: usize) -> Diff {
    let mut max = 0i32;
    let mut sum = 0i64;
    let mut outliers = 0usize;
    let mut worst = None;
    let mut signed = [0f64; 3];
    let mut hist = [0usize; 9];
    let n = got.pixels.len().min(want.len() / 3).max(1);
    for (i, px) in got.pixels.iter().enumerate().take(n) {
        let base = i * 3;
        if base + 2 >= want.len() {
            break;
        }
        let w = Rgb::new(want[base], want[base + 1], want[base + 2]);
        let dr = px.r as i32 - w.r as i32;
        let dg = px.g as i32 - w.g as i32;
        let db = px.b as i32 - w.b as i32;
        signed[0] += dr as f64;
        signed[1] += dg as f64;
        signed[2] += db as f64;
        hist[(dr.abs().max(dg.abs()).max(db.abs()) as usize).min(8)] += 1;
        let d = dr.abs().max(dg.abs()).max(db.abs());
        max = max.max(d);
        sum += d as i64;
        if d > 8 {
            outliers += 1;
            if worst.is_none() {
                worst = Some((i % width, i / width, *px, w));
            }
        }
    }
    Diff {
        max,
        mean: sum as f64 / n as f64,
        outliers: outliers as f64 / n as f64,
        worst,
        signed: [
            signed[0] / n as f64,
            signed[1] / n as f64,
            signed[2] / n as f64,
        ],
        hist,
    }
}

#[test]
fn baseline_jpegs_match_pillow_pixel_for_pixel_within_decoder_tolerance() {
    let dir = fixtures_dir();
    let index = read_index(&dir.join("index.json"));
    let cases = index["cases"].as_array().expect("index.json 缺少 cases");

    let mut checked = 0usize;
    for case in cases {
        let file = case["file"].as_str().unwrap();
        let raw_file = case["raw"].as_str().unwrap();
        let width = case["width"].as_u64().unwrap() as usize;
        let height = case["height"].as_u64().unwrap() as usize;
        let mean_luma_want = case["mean_luma"].as_f64().unwrap();

        let jpg = std::fs::read(dir.join(file)).unwrap_or_else(|e| panic!("读不到 {file}：{e}"));
        let raw =
            std::fs::read(dir.join(raw_file)).unwrap_or_else(|e| panic!("读不到 {raw_file}：{e}"));

        assert_eq!(sniff(&jpg), Some(Format::Jpeg), "{file} 没被认成 JPEG");
        assert_eq!(
            dimensions(&jpg),
            Some((width, height)),
            "{file} 的尺寸读错了"
        );

        let got = match decode(&jpg) {
            Ok(b) => b,
            Err(e) => panic!("{file} 解码失败：{e}"),
        };
        assert_eq!(
            (got.width, got.height),
            (width, height),
            "{file} 解出来的尺寸不对"
        );
        assert_eq!(got.pixels.len(), width * height, "{file} 的像素数不对");

        let diff = compare(&got, &raw, width);
        let mean_luma_got: f64 = got
            .pixels
            .iter()
            .map(|p| (0.299 * p.r as f64 + 0.587 * p.g as f64 + 0.114 * p.b as f64) / 255.0)
            .sum::<f64>()
            / got.pixels.len() as f64;

        println!(
            "{file:20} max={:3} mean={:.3} 差>8 的占比={:.3}%  亮度 {:.4} vs {:.4}",
            diff.max,
            diff.mean,
            diff.outliers * 100.0,
            mean_luma_got,
            mean_luma_want
        );
        println!(
            "    带符号通道均值差 R/G/B = {:.3} / {:.3} / {:.3}   误差直方图(0..8+) = {:?}",
            diff.signed[0], diff.signed[1], diff.signed[2], diff.hist
        );
        if let Some((x, y, got_px, want_px)) = diff.worst {
            println!(
                "    最差处在 ({x},{y})：得到 {}/{}/{}，期望 {}/{}/{}",
                got_px.r, got_px.g, got_px.b, want_px.r, want_px.g, want_px.b
            );
        }
        if std::env::var("STYX_JPEG_DUMP").is_ok() {
            print!("    前 6 个像素 得到→期望：");
            for i in 0..6.min(got.pixels.len()) {
                let p = got.pixels[i];
                let b = i * 3;
                print!(
                    " {}/{}/{}→{}/{}/{} |",
                    p.r,
                    p.g,
                    p.b,
                    raw[b],
                    raw[b + 1],
                    raw[b + 2]
                );
            }
            println!();
        }

        // 平均差：真错了就一定会远超这个数，而不一样但都正确的 IDCT
        // 通常落在 0.5 以内。
        assert!(
            diff.mean <= 2.0,
            "{file} 平均差 {:.3} 太大——这不是 IDCT 舍入能解释的",
            diff.mean
        );
        // 大偏差像素必须少：硬边上有几个像素分歧正常，成片分歧就是错了。
        assert!(
            diff.outliers <= 0.02,
            "{file} 有 {:.2}% 的像素差了 8 以上，太多了",
            diff.outliers * 100.0
        );
        // 整体亮度是"DC 路径"是否正确的汇总指标，最灵敏也最稳。
        assert!(
            (mean_luma_got - mean_luma_want).abs() < 0.02,
            "{file} 整体亮度对不上：得到 {:.4}，期望 {:.4}",
            mean_luma_got,
            mean_luma_want
        );
        checked += 1;
    }
    assert!(checked >= 8, "只跑了 {checked} 个样例，夹具不全");
}

#[test]
fn progressive_jpeg_is_refused_instead_of_producing_garbage() {
    let dir = fixtures_dir();
    let jpg = std::fs::read(dir.join("progressive.jpg")).expect("缺少 progressive.jpg");
    assert_eq!(sniff(&jpg), Some(Format::Jpeg));
    // 尺寸还是要读得出来——"不支持"不等于"不认识"
    assert_eq!(dimensions(&jpg), Some((64, 48)));

    match decode(&jpg) {
        Err(DecodeError::Unsupported(msg)) => {
            assert!(
                msg.contains("渐进"),
                "拒绝理由要说人话，现在的理由是：{msg}"
            );
            println!("渐进式被如实拒绝：{msg}");
        }
        Err(other) => panic!("拒绝理由不该是 {other}"),
        Ok(_) => panic!("渐进式 JPEG 不该被解出来——解出来的东西一定是错的"),
    }
}

#[test]
fn truncated_and_corrupt_jpegs_fail_loudly_instead_of_hanging() {
    let dir = fixtures_dir();
    let jpg = std::fs::read(dir.join("ycbcr_420_q75.jpg")).unwrap();

    // 砍掉后半段：必须报错，而不是panic或死循环
    let half = &jpg[..jpg.len() / 2];
    let r = decode(half);
    assert!(r.is_err(), "截断的文件不该解出图来");

    // 只剩文件头
    let r = decode(&jpg[..4]);
    assert!(r.is_err(), "只剩魔法字节不该解出图来");

    // 随机改中间的字节：允许解出图，但绝不允许 panic
    let mut tampered = jpg.clone();
    for i in (100..tampered.len().min(300)).step_by(7) {
        tampered[i] ^= 0xA5;
    }
    let _ = decode(&tampered);
}
