//! # 画面特征与自然语言描述
//!
//! 这一层要回答的问题是：**怎么让一个没有视觉能力的语言模型"看懂"一张图。**
//!
//! 思路不是"猜图里有什么"（那需要真正的视觉模型），而是给出**人类的观看笔记**：
//! 尺寸与构图、明暗、冷暖、鲜艳程度、是不是黑白、画面是疏是密、主色是哪几种。
//! 这些恰好都是角色扮演里真正会被用到的信息——
//!
//! - "泛黄的黑白照片" → 角色会想到年代感、旧事、逝去的人；
//! - "整体偏暗、极简的深灰画面" → 角色会读出压抑；
//! - "饱和鲜艳的暖色块" → 角色会读出活泼、卡通、轻松。
//!
//! 所以描述里刻意**不做**"这是一只猫"这样的语义断言：那是幻觉的来源。
//! 我们只陈述能算出来的事实，并明确标注不确定性（"看起来像是照片或插画"）。
//!
//! 语义那一半交给[可选的标签层](crate::tags)与[可选的视觉模型](crate::backend)：
//! 它们是**追加**的，而且带来源与置信度，所以"猜"和"看"在文本上永远分得清。
//!
//! ## 为什么事实要用序列化结构承载
//!
//! 因为同一条链路有三个入口：
//! 1. 本地解码（PNG / BMP）；
//! 2. 前端 Canvas（JPEG 等本地解不了的格式，浏览器能解）；
//! 3. 未来可能的其它分析器。
//!
//! 三者产出的都是 [`ImageFacts`]，谁先算出算谁的，缺字段就少说一句话——
//! 而不是让上层去区分"这份数据是谁给的"。

use serde::{Deserialize, Serialize};

use crate::backend::Detection;
use crate::decode::{decode, dimensions, sniff, Bitmap, DecodeError, Rgb};
use crate::tags::{self, TagHit};

/// 参与统计的像素数上限。
///
/// 400 万像素的照片全采样要跑几千万次浮点运算，对一回合的等待感来说是
/// 不该有的开销；而画面特征这种"总体印象"用十几万个采样点已经和全采样
/// 的结果几乎一致。
pub const MAX_SAMPLES: usize = 120_000;

/// 一张图片算出来的客观特征。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageFacts {
    pub width: usize,
    pub height: usize,
    /// `png` / `bmp` / `jpeg`。
    pub format: String,
    /// 像素是否真的解出来了。`false` 时后面那些数值都没有意义。
    pub decoded: bool,
    /// 平均亮度 0..1。
    #[serde(default)]
    pub mean_luma: f32,
    /// 对比度 0..1（亮度标准差归一）。
    #[serde(default)]
    pub contrast: f32,
    /// 平均饱和度 0..1。
    #[serde(default)]
    pub saturation: f32,
    /// 色彩丰富度 0..1（量化后不同色占采样点的比例）。
    #[serde(default)]
    pub colorfulness: f32,
    /// 边缘密度 0..1（相邻像素亮度跳变的比例）。
    #[serde(default)]
    pub edge_density: f32,
    /// 是否基本是黑白 / 灰阶。
    #[serde(default)]
    pub grayscale: bool,
    /// 主色（`#rrggbb`，最多 4 个，按占比降序）。
    #[serde(default)]
    pub dominant: Vec<String>,
    /// 色调倾向：`暖色` / `冷色` / `绿色系` / `紫色系` / `中性`。
    #[serde(default)]
    pub hue_family: String,
    /// 目标检测结果（由可选的后端**追加**，本地分析不会产生它）。
    #[serde(default)]
    pub objects: Vec<Detection>,
    /// 反向推断出的属性标签（同样是追加的，来自可选的标签后端）。
    ///
    /// 与 `objects` 的区别：`objects` 是"有什么东西、在哪儿"，
    /// `tags` 是"画面整体有哪些属性"（发色、神情、穿着、构图、画风）。
    /// 详见 [`crate::tags`]。
    #[serde(default)]
    pub tags: Vec<TagHit>,
    /// 视觉模型给出的补充描述（同样是追加的，可以有 0 到多条）。
    #[serde(default)]
    pub captions: Vec<String>,
    /// 降级与限制说明（会一并交给模型，让它知道自己拿到的是什么）。
    #[serde(default)]
    pub notes: Vec<String>,
}

impl ImageFacts {
    /// 只知道尺寸时的最小事实（JPEG 的常见情形）。
    fn size_only(width: usize, height: usize, format: &str) -> Self {
        ImageFacts {
            width,
            height,
            format: format.to_string(),
            decoded: false,
            mean_luma: 0.0,
            contrast: 0.0,
            saturation: 0.0,
            colorfulness: 0.0,
            edge_density: 0.0,
            grayscale: false,
            dominant: Vec::new(),
            hue_family: String::new(),
            objects: Vec::new(),
            tags: Vec::new(),
            captions: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// 长宽比的可读描述，例如 `4:3 横构图`。
    pub fn aspect_label(&self) -> String {
        if self.width == 0 || self.height == 0 {
            return "比例未知".to_string();
        }
        let ratio = self.width as f32 / self.height as f32;
        let orientation = if (ratio - 1.0).abs() < 0.02 {
            "正方形"
        } else if ratio > 1.0 {
            "横构图"
        } else {
            "竖构图"
        };
        // 常见比例（宽/高）
        const KNOWN: [(f32, &str); 6] = [
            (1.0, "1:1"),
            (4.0 / 3.0, "4:3"),
            (3.0 / 2.0, "3:2"),
            (16.0 / 10.0, "16:10"),
            (16.0 / 9.0, "16:9"),
            (21.0 / 9.0, "21:9"),
        ];
        let normalized = if ratio >= 1.0 { ratio } else { 1.0 / ratio };
        for (value, name) in KNOWN {
            if (normalized - value).abs() < 0.04 {
                return format!("{name} {orientation}");
            }
        }
        format!("约 {normalized:.2}:1 {orientation}")
    }

    /// 明暗档位的可读描述。
    pub fn brightness_label(&self) -> &'static str {
        match self.mean_luma {
            v if v < 0.2 => "整体很暗",
            v if v < 0.36 => "整体偏暗",
            v if v < 0.62 => "明暗适中",
            // 0.88 而不是 0.80：一张浅米色的图 luma 约 0.85，说它"几乎全亮"
            // 是错的，"几乎全亮"应该留给真正接近过曝的画面。
            v if v < 0.88 => "整体明亮",
            _ => "几乎全亮",
        }
    }

    /// 对比度的可读描述。
    pub fn contrast_label(&self) -> &'static str {
        match self.contrast {
            v if v < 0.12 => "几乎没有明暗反差",
            v if v < 0.25 => "反差偏弱",
            v if v < 0.45 => "反差适中",
            v if v < 0.65 => "反差强烈",
            _ => "反差极其强烈（高对比黑白或剪影）",
        }
    }

    /// 饱和度的可读描述。
    pub fn saturation_label(&self) -> &'static str {
        match self.saturation {
            v if v < 0.06 => "几乎没有颜色",
            v if v < 0.18 => "颜色很淡",
            v if v < 0.35 => "色调克制",
            v if v < 0.55 => "颜色鲜明",
            _ => "颜色非常浓烈",
        }
    }

    /// 画面疏密的可读描述。
    pub fn detail_label(&self) -> &'static str {
        match self.edge_density {
            v if v < 0.04 => "画面非常干净（大片纯色）",
            v if v < 0.12 => "画面简洁",
            v if v < 0.25 => "画面元素密度中等",
            v if v < 0.4 => "画面细节密集",
            _ => "画面十分杂乱或充满细密纹理",
        }
    }

    /// 主色的中文并列描述，例如 `暖黄、米白`。
    pub fn palette_label(&self) -> String {
        if self.dominant.is_empty() {
            return String::new();
        }
        let mut names: Vec<String> = Vec::new();
        for hex in &self.dominant {
            if let Some(n) = name_of_hex(hex) {
                // 同一个粗粒度名字只报一次。
                //
                // 不这么做的话，四个深浅不同的近白会渲染成
                // "近白、近白、近白、淡红"——读起来像四种颜色，实际只有两种，
                // 而且这段文字要进对话历史，白占字符。色值本身在后面照旧全给，
                // 所以丢掉的只是重复的形容词，不是信息。
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        if names.is_empty() {
            self.dominant.join(" / ")
        } else {
            format!("{}（{}）", names.join("、"), self.dominant.join(" / "))
        }
    }

    /// 一句话摘要：给事件流、列表与提示词共用。
    ///
    /// 长度刻意压在一行左右——它会出现在每一条图片事件的 `text` 里，
    /// 而事件是要进对话历史的，太长会挤掉真正重要的上下文。
    pub fn summary_line(&self) -> String {
        let mut s = format!(
            "{}×{} 的{}图片（{}）",
            self.width,
            self.height,
            if self.grayscale { "黑白" } else { "彩色" },
            self.aspect_label()
        );
        if self.decoded {
            s.push_str(&format!(
                "：{}，{}，{}",
                self.brightness_label(),
                self.contrast_label(),
                self.saturation_label()
            ));
            let palette = self.palette_label();
            if !palette.is_empty() {
                s.push_str(&format!("，主色是{palette}"));
            }
        }
        if !self.objects.is_empty() {
            let names: Vec<&str> = self
                .objects
                .iter()
                .take(4)
                .map(|d| d.display_name())
                .collect();
            s.push_str(&format!("；认出{}", names.join("、")));
        }
        if let Some(first) = self.captions.first() {
            s.push_str(&format!("；{}", first));
        }
        s
    }

    /// 给语言模型看的完整描述。
    ///
    /// 这是"本地视觉"的真正产物。写法上有三条自我约束：
    /// - **不说画面里有什么**（本地没这个能力，说了就是编）——
    ///   但后端报上来的东西要照实转述，只是标明是谁说的；
    /// - **把不确定说成不确定**（"看起来像是"而不是"这是"）；
    /// - **把限制也告诉模型**（"本地没能解出像素，只有尺寸"），
    ///   否则模型会把"没信息"当成"画面很普通"。
    ///
    /// ## 一个踩过的坑：宽度不一的描述不能说早退就早退
    ///
    /// 这里曾经写成「解不出像素 → 早早 return」。看起来很合理，实际是个
    /// 严重的错误：`decoded=false` 时（典型情况是 JPEG）**检测到的 5 个物体、
    /// 反推出来的 14 个标签全被丢掉了**——而它们明明来自真正看见图的后端。
    ///
    /// 所以现在的结构是：先写"尺寸"这条永远拿得到的硬事实，再**有条件地**
    /// 写"像素统计"那一组（只在真解出来时才有意义），然后**无条件地**
    /// 汇总所有后端追加的信息。唯一与 `decoded` 有关的判断是
    /// "能不能基于纹理猜类型"——没像素的时候猜，就是纯编。
    pub fn describe(&self) -> String {
        if self.width == 0 || self.height == 0 {
            let mut s = "一张图片（尺寸都读不出来，文件可能已损坏）".to_string();
            if !self.notes.is_empty() {
                s.push_str(&format!("（{}）", self.notes.join("；")));
            }
            return s;
        }

        let mut s: Vec<String> = Vec::new();

        if self.decoded {
            s.push(format!(
                "一张 {}×{} 的{}图片（{}）。",
                self.width,
                self.height,
                if self.grayscale { "黑白" } else { "彩色" },
                self.aspect_label()
            ));
            s.push(format!(
                "{}（平均亮度 {:.2}），{}，{}。",
                self.brightness_label(),
                self.mean_luma,
                self.contrast_label(),
                self.saturation_label()
            ));
            let palette = self.palette_label();
            if !palette.is_empty() {
                s.push(format!("主要颜色是{palette}。"));
            }
            if !self.hue_family.is_empty() && self.hue_family != "中性" {
                s.push(format!("整体色调偏{}。", self.hue_family));
            }
            s.push(format!("{}。", self.detail_label()));
        } else {
            // 只认识文件头时的开场：把"我没看见"说在明处。
            s.push(format!(
                "一张 {}×{}（{}）的 {} 图片。",
                self.width,
                self.height,
                self.aspect_label(),
                self.format.to_uppercase()
            ));
            s.push(
                "（本地没能解出它的像素，下面只有尺寸与比例；不要据此推断画面内容。）".to_string(),
            );
        }

        // ---- 以下都与"本地有没有解出像素"无关 ----
        // 后端是真的看过这张图的，它给的东西必须转述出去。
        if !self.objects.is_empty() {
            s.push(format!(
                "画面里能认出的东西：{}。",
                objects_sentence(&self.objects)
            ));
        }

        // 标签层（提示词反推）。措辞上刻意把它和"看见"区分开：
        // 它是一次分类的结果，不是事实断言。
        if !self.tags.is_empty() {
            let mut seg = format!(
                "反推出来的画面标签（自动标注，未必准确）：{}。",
                tags::summary(&self.tags)
            );
            for c in tags::caveats(&self.tags) {
                seg.push_str(&format!("（{c}。）"));
            }
            s.push(seg);
        }

        for (i, caption) in self.captions.iter().enumerate() {
            s.push(if i == 0 {
                format!("对画面的理解：{caption}")
            } else {
                format!("另一种说法：{caption}")
            });
        }

        // 只有在**既解出了像素、又完全拿不到语义信息**时，才给一句基于纹理的
        // 推测。两个条件都必要：没有像素就无从推测（那是纯编）；有检测结果、
        // 标签或视觉模型结论时也不该再猜（"标签说这是教室、纹理猜这是风景"
        // 会直接毁掉可信度）。
        if self.decoded
            && self.captions.is_empty()
            && self.objects.is_empty()
            && self.tags.is_empty()
        {
            s.push(format!("仅从这些特征看，看起来像是{}。", self.guess_kind()));
        }

        if !self.notes.is_empty() {
            s.push(format!("（{}）", self.notes.join("；")));
        }
        s.join("")
    }

    /// 基于统计特征猜"这是哪一类图"。
    ///
    /// 只在**特征组合非常典型**时才下判断，其余一律走"不确定"分支。
    /// 这个函数的正确用法是"给模型一个语气上的倾向"，不是"给模型一个事实"。
    fn guess_kind(&self) -> String {
        let colorful = self.colorfulness;
        let edges = self.edge_density;
        if self.grayscale {
            if edges > 0.2 {
                return "扫描的老照片、旧报纸或线稿".to_string();
            }
            if self.contrast > 0.5 && edges < 0.08 {
                return "高对比的黑白图形或文字截图".to_string();
            }
            return "黑白的照片或插图".to_string();
        }
        if colorful < 0.02 && edges < 0.05 {
            return "纯色块或极简的图形、图标".to_string();
        }
        if colorful < 0.05 && edges > 0.2 {
            return "界面截图或文字较多的画面".to_string();
        }
        if self.saturation > 0.4 && edges > 0.2 {
            return "彩色插画、动漫图或色彩浓烈的摄影".to_string();
        }
        if self.saturation < 0.2 && edges > 0.15 {
            return "色调偏素净的摄影（可能是风景或室内）".to_string();
        }
        if edges < 0.06 {
            return "构图简洁的图片（留白较多）".to_string();
        }
        "一张普通的彩色照片或插画".to_string()
    }
}

/// 分析一段图片字节。
pub fn analyze(data: &[u8]) -> ImageFacts {
    let Some(format) = sniff(data) else {
        return ImageFacts {
            width: 0,
            height: 0,
            format: "unknown".into(),
            decoded: false,
            notes: vec!["认不出这是什么图片格式".into()],
            ..size_zero()
        };
    };
    let format_name = format.as_str();
    let Some((w, h)) = dimensions(data) else {
        return ImageFacts {
            width: 0,
            height: 0,
            format: format_name.into(),
            decoded: false,
            notes: vec!["读不出尺寸，文件可能已损坏".into()],
            ..size_zero()
        };
    };

    match decode(data) {
        Ok(bitmap) => {
            let mut facts = analyze_bitmap(&bitmap);
            facts.format = format_name.to_string();
            // 手机竖拍：EXIF 说转 90° 的话，把宽高换过来更贴近人看到的
            if matches!(format, crate::decode::Format::Jpeg) {
                if let Some(o) = crate::decode::jpeg_orientation(data) {
                    if (5..=8).contains(&o) {
                        std::mem::swap(&mut facts.width, &mut facts.height);
                        facts.notes.push("已按 EXIF 方向摆正".into());
                    }
                }
            }
            facts
        }
        Err(e) => {
            let mut facts = ImageFacts::size_only(w, h, format_name);
            facts.notes.push(match &e {
                DecodeError::Unsupported(_) => e.to_string(),
                _ => format!("解码失败：{e}"),
            });
            facts
        }
    }
}

fn size_zero() -> ImageFacts {
    ImageFacts {
        width: 0,
        height: 0,
        format: String::new(),
        decoded: false,
        mean_luma: 0.0,
        contrast: 0.0,
        saturation: 0.0,
        colorfulness: 0.0,
        edge_density: 0.0,
        grayscale: false,
        dominant: Vec::new(),
        hue_family: String::new(),
        objects: Vec::new(),
        tags: Vec::new(),
        captions: Vec::new(),
        notes: Vec::new(),
    }
}

/// 把检测结果说成一句人话。
fn objects_sentence(objects: &[Detection]) -> String {
    let parts: Vec<String> = objects
        .iter()
        .take(8)
        .map(|d| {
            let pos = d.position_label();
            let share = if d.area > 0.0 {
                format!("，约占画面 {:.0}%", (d.area * 100.0).min(100.0))
            } else {
                String::new()
            };
            if pos.is_empty() {
                format!(
                    "{}（{:.0}% 把握{}）",
                    d.display_name(),
                    d.confidence * 100.0,
                    share
                )
            } else {
                format!(
                    "{}（{pos}，{:.0}% 把握{share}）",
                    d.display_name(),
                    d.confidence * 100.0
                )
            }
        })
        .collect();
    parts.join("、")
}

/// 从已经解出的位图算特征。
///
/// 单独暴露出来是为了让前端的 Canvas 路径能复用同一套阈值——同一张图
/// 无论走哪条路分析，得到的描述都该是一样的。
pub fn analyze_bitmap(bitmap: &Bitmap) -> ImageFacts {
    let mut facts = size_zero();
    facts.width = bitmap.width;
    facts.height = bitmap.height;
    if bitmap.is_empty() {
        return facts;
    }
    facts.decoded = true;

    let total = bitmap.width * bitmap.height;
    let stride = (total / MAX_SAMPLES).max(1);
    let sample: Vec<Rgb> = bitmap.pixels.iter().step_by(stride).copied().collect();
    if sample.is_empty() {
        return facts;
    }
    let n = sample.len() as f32;

    let mut luma_sum = 0f32;
    let mut luma_sq = 0f32;
    let mut sat_sum = 0f32;
    let mut chroma_sum = 0f32;
    let mut hue_x = 0f32;
    let mut hue_y = 0f32;
    for c in &sample {
        let l = c.luma();
        luma_sum += l;
        luma_sq += l * l;
        sat_sum += c.saturation();
        chroma_sum += (c.r.max(c.g).max(c.b) - c.r.min(c.g).min(c.b)) as f32;
        let s = c.saturation();
        if s > 0.08 {
            let rad = c.hue().to_radians();
            hue_x += rad.cos() * s;
            hue_y += rad.sin() * s;
        }
    }
    facts.mean_luma = (luma_sum / n).clamp(0.0, 1.0);
    let variance = (luma_sq / n - facts.mean_luma * facts.mean_luma).max(0.0);
    // 亮度标准差上限约 0.5（黑白各半），除以 2 再映射到 0..1
    facts.contrast = (variance.sqrt() * 2.0).clamp(0.0, 1.0);
    facts.saturation = (sat_sum / n).clamp(0.0, 1.0);
    facts.grayscale = chroma_sum / n < 12.0;

    // 主色：按 4bit/通道量化计数，再取每桶的真实平均色
    let mut buckets: std::collections::BTreeMap<u16, (u32, u32, u32, u32)> = Default::default();
    for c in &sample {
        let key = ((c.r as u16 >> 4) << 8) | ((c.g as u16 >> 4) << 4) | (c.b as u16 >> 4);
        let e = buckets.entry(key).or_insert((0, 0, 0, 0));
        e.0 += 1;
        e.1 += c.r as u32;
        e.2 += c.g as u32;
        e.3 += c.b as u32;
    }
    facts.colorfulness = (buckets.len() as f32 / n).clamp(0.0, 1.0);

    let mut ranked: Vec<(u32, Rgb)> = buckets
        .values()
        .map(|(count, r, g, b)| {
            let c = *count;
            (c, Rgb::new((r / c) as u8, (g / c) as u8, (b / c) as u8))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0));
    let mut chosen: Vec<Rgb> = Vec::new();
    for (_, color) in ranked {
        // 去重：已经很接近的颜色不再重复列
        if chosen.iter().any(|c| color_distance(c, &color) < 48) {
            continue;
        }
        chosen.push(color);
        if chosen.len() >= 4 {
            break;
        }
    }
    facts.dominant = chosen.iter().map(|c| c.hex()).collect();

    // 色调倾向用色相向量平均（对饱和度加权，灰色自然被压掉）
    let mag = (hue_x * hue_x + hue_y * hue_y).sqrt() / n;
    facts.hue_family = if facts.grayscale || mag < 0.02 {
        "中性".to_string()
    } else {
        let deg = hue_y.atan2(hue_x).to_degrees().rem_euclid(360.0);
        match deg {
            d if !(75.0..315.0).contains(&d) => "暖色", // 红/橙/黄/品红
            d if d < 165.0 => "绿色系",
            d if d < 255.0 => "冷色", // 青/蓝
            _ => "紫色系",
        }
        .to_string()
    };

    // 边缘密度：横竖相邻采样点的亮度跳变比例
    let mut jumps = 0u32;
    let mut pairs = 0u32;
    let step = (bitmap.width / 64).max(1);
    let row_step = (bitmap.height / 64).max(1);
    for y in (0..bitmap.height).step_by(row_step) {
        for x in (0..bitmap.width.saturating_sub(step)).step_by(step) {
            let a = bitmap.get(x, y).luma();
            let b = bitmap.get(x + step, y).luma();
            pairs += 1;
            if (a - b).abs() > 0.125 {
                jumps += 1;
            }
        }
    }
    if pairs > 0 {
        facts.edge_density = (jumps as f32 / pairs as f32).clamp(0.0, 1.0);
    }

    facts
}

fn color_distance(a: &Rgb, b: &Rgb) -> u32 {
    let dr = a.r.abs_diff(b.r) as u32;
    let dg = a.g.abs_diff(b.g) as u32;
    let db = a.b.abs_diff(b.b) as u32;
    // 加权欧氏距离的整数近似（人眼对绿最敏感）
    (dr * dr * 3 + dg * dg * 6 + db * db) / 10
}

/// 把 `#rrggbb` 说成一个中文颜色词。
fn name_of_hex(hex: &str) -> Option<String> {
    let hex = hex.trim_start_matches('#');
    if hex.len() < 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    let c = Rgb::new(r, g, b);
    let luma = c.luma();
    let light = c.lightness();
    let s = c.saturation();

    if s < 0.10 {
        return Some(
            match luma {
                v if v < 0.15 => "近黑",
                v if v < 0.35 => "深灰",
                v if v < 0.6 => "中灰",
                v if v < 0.82 => "浅灰",
                _ => "近白",
            }
            .to_string(),
        );
    }
    let hue = c.hue();
    // 深浅用 HSL 的 L，不用 luma —— 否则纯红会被叫成"暗红"。
    let warm = if light > 0.75 {
        "淡"
    } else if light < 0.3 {
        "暗"
    } else {
        ""
    };
    let base = match hue {
        h if h < 15.0 || h >= 345.0 => "红",
        h if h < 45.0 => "橙",
        h if h < 70.0 => "黄",
        h if h < 95.0 => "黄绿",
        h if h < 150.0 => "绿",
        h if h < 190.0 => "青",
        h if h < 250.0 => "蓝",
        h if h < 290.0 => "靛",
        h if h < 345.0 => "紫",
        _ => "红",
    };
    Some(format!("{warm}{base}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Rgb as C;

    fn bmp_of(pixels: Vec<C>, w: usize, h: usize) -> Bitmap {
        let mut b = Bitmap {
            width: w,
            height: h,
            pixels: vec![C::default(); w * h],
        };
        for (i, p) in pixels.into_iter().enumerate() {
            if i < b.pixels.len() {
                b.pixels[i] = p;
            }
        }
        b
    }

    #[test]
    fn a_solid_gray_image_is_flat_and_neutral() {
        let bitmap = bmp_of(vec![C::new(128, 128, 128); 64], 8, 8);
        let f = analyze_bitmap(&bitmap);
        assert!(f.decoded);
        assert!(f.grayscale, "纯灰必须判成黑白");
        assert!(f.contrast < 0.05, "纯色没有反差：{}", f.contrast);
        assert!(f.saturation < 0.02);
        assert!(f.edge_density < 0.02, "纯色没有边缘：{}", f.edge_density);
        assert_eq!(f.hue_family, "中性");
        assert_eq!(f.dominant.len(), 1);
        assert!(f.render_dominant_contains("中灰"), "{:?}", f.dominant);
    }

    #[test]
    fn a_bright_warm_image_reads_as_bright_and_warm() {
        let bitmap = bmp_of(vec![C::new(240, 220, 150); 100], 10, 10);
        let f = analyze_bitmap(&bitmap);
        assert!(!f.grayscale);
        assert!(f.mean_luma > 0.8, "{}", f.mean_luma);
        assert_eq!(f.hue_family, "暖色");
        assert!(f.describe().contains("整体明亮"));
        assert!(f.describe().contains("暖色"));
    }

    #[test]
    fn a_dark_cool_image_reads_as_dark_and_cool() {
        let bitmap = bmp_of(vec![C::new(20, 30, 80); 100], 10, 10);
        let f = analyze_bitmap(&bitmap);
        assert!(f.mean_luma < 0.25, "{}", f.mean_luma);
        assert_eq!(f.hue_family, "冷色");
        assert!(f.describe().contains("暗"));
    }

    #[test]
    fn a_half_black_half_white_image_has_high_contrast() {
        let mut px = vec![C::new(0, 0, 0); 50];
        px.extend(vec![C::new(255, 255, 255); 50]);
        let f = analyze_bitmap(&bmp_of(px, 10, 10));
        assert!(f.contrast > 0.8, "{}", f.contrast);
        assert_eq!(f.dominant.len(), 2, "黑白两色都该出现在主色里");
    }

    #[test]
    fn checkerboard_has_high_edge_density() {
        let mut px = Vec::new();
        for y in 0..16 {
            for x in 0..16 {
                px.push(if (x + y) % 2 == 0 {
                    C::new(0, 0, 0)
                } else {
                    C::new(255, 255, 255)
                });
            }
        }
        let f = analyze_bitmap(&bmp_of(px, 16, 16));
        assert!(f.edge_density > 0.3, "{}", f.edge_density);
        assert!(f.detail_label().contains("密集") || f.detail_label().contains("杂乱"));
    }

    #[test]
    fn aspect_labels_cover_the_common_shapes() {
        let mk = |w, h| {
            let mut f = size_zero();
            f.width = w;
            f.height = h;
            f
        };
        assert_eq!(mk(100, 100).aspect_label(), "1:1 正方形");
        assert_eq!(mk(640, 480).aspect_label(), "4:3 横构图");
        assert_eq!(mk(1920, 1080).aspect_label(), "16:9 横构图");
        assert_eq!(mk(480, 640).aspect_label(), "4:3 竖构图");
        assert!(mk(1000, 137).aspect_label().contains("约"));
    }

    #[test]
    fn undecoded_facts_stay_honest() {
        let mut f = size_zero();
        f.width = 1024;
        f.height = 768;
        f.format = "jpeg".into();
        f.notes.push("JPEG 需要完整的 DCT 解码器".into());
        let d = f.describe();
        assert!(d.contains("1024×768"));
        assert!(d.contains("4:3"));
        assert!(d.contains("本地没能解出它的像素"));
        assert!(d.contains("不要据此推断画面内容"));
        // 关键：不能因为没数据就说"画面很普通"
        assert!(!d.contains("普通"));
    }

    #[test]
    fn a_broken_file_says_so_instead_of_pretending() {
        let f = analyze(b"this is not an image at all");
        assert!(!f.decoded);
        assert_eq!(f.format, "unknown");
        assert!(f.describe().contains("损坏") || f.describe().contains("认不出"));
    }

    #[test]
    fn missing_pixels_do_not_swallow_what_the_backends_reported() {
        // 回归。这里曾经写成「解不出像素 → 早早 return」，于是**后端明明看见
        // 了图**、报了 5 个物体和若干标签，却因为本地解码器不认识这个格式
        // 而被整个丢掉。真实触发场景：用户发 JPEG，检测和反推都成功，
        // 但模型收到的描述里只有"一张 810×1080 的图片"。
        let mut f = ImageFacts::size_only(810, 1080, "jpeg");
        f.objects.push(Detection {
            label: "bus".into(),
            label_zh: "公交车".into(),
            confidence: 0.84,
            r#box: vec![0.04, 0.21, 0.99, 0.72],
            area: 0.48,
        });
        f.tags.push(TagHit::new("outdoors", 0.46));

        assert!(!f.decoded);
        let text = f.describe();
        assert!(text.contains("公交车"), "检测结果必须被转述：{text}");
        assert!(text.contains("outdoors"), "反推标签必须被转述：{text}");
        assert!(text.contains("810×1080"), "尺寸这条硬事实要在：{text}");
        // 而"我其实没看见像素"这件事也必须说清楚
        assert!(text.contains("没能解出"), "{text}");
        // 没像素时绝不能靠纹理猜类型——那是纯编
        assert!(!text.contains("仅从这些特征看"), "{text}");
    }

    #[test]
    fn texture_guessing_needs_both_pixels_and_a_lack_of_semantics() {
        // 有像素、没语义 → 可以猜
        let f = analyze_bitmap(&bmp_of(vec![C::new(240, 220, 150); 100], 10, 10));
        assert!(f.decoded && f.objects.is_empty() && f.tags.is_empty());
        assert!(f.describe().contains("仅从这些特征看"), "{}", f.describe());

        // 有像素、有语义 → 不许再猜（两句打架会毁掉可信度）
        let mut g = analyze_bitmap(&bmp_of(vec![C::new(240, 220, 150); 100], 10, 10));
        g.objects.push(Detection {
            label: "person".into(),
            label_zh: "人".into(),
            confidence: 0.9,
            r#box: vec![0.2, 0.2, 0.8, 0.8],
            area: 0.36,
        });
        assert!(!g.describe().contains("仅从这些特征看"), "{}", g.describe());
    }

    #[test]
    fn repeated_colour_names_collapse_but_the_hex_values_stay() {
        // 回归：一张白底插图的主色是四个深浅不同的近白加一个淡红，
        // 早先会渲染成"近白、近白、近白、淡红"——看起来像四种颜色。
        let mut f = analyze_bitmap(&bmp_of(vec![C::new(255, 255, 255); 100], 10, 10));
        f.dominant = vec![
            "#fdfbfb".into(),
            "#f9e8e8".into(),
            "#fbf2ec".into(),
            "#f5d7d9".into(),
        ];
        let label = f.palette_label();
        assert_eq!(label.matches("近白").count(), 1, "{label}");
        assert!(label.contains("淡红"), "{label}");
        // 去重只能去掉重复的形容词，色值一个都不能少。
        for hex in &f.dominant {
            assert!(label.contains(hex), "{hex} 丢了：{label}");
        }
    }

    #[test]
    fn palette_label_names_the_colours() {
        // 用一目了然的黄（hue≈50°）。琥珀色 #ffc83c 的 hue 是 43°，
        // 正好压在橙/黄的边界上，不适合拿来当断言基准。
        let f = analyze_bitmap(&bmp_of(vec![C::new(255, 220, 40); 100], 10, 10));
        let label = f.palette_label();
        assert!(label.contains("黄"), "{label}");
        assert!(label.contains('#'), "十六进制值也要给出：{label}");
    }

    #[test]
    fn pure_red_is_red_not_dark_red() {
        // 回归：luma 给纯红只有 0.30，早先的实现会把它叫成"暗红"。
        let f = analyze_bitmap(&bmp_of(vec![C::new(255, 0, 0); 100], 10, 10));
        assert_eq!(f.palette_label(), "红（#ff0000）");
        // 而真正的深红仍然要读成"暗红"
        let dark = analyze_bitmap(&bmp_of(vec![C::new(110, 0, 0); 100], 10, 10));
        assert!(
            dark.palette_label().contains("暗红"),
            "{}",
            dark.palette_label()
        );
    }

    #[test]
    fn colour_naming_covers_the_spectrum() {
        assert_eq!(name_of_hex("#ff0000").unwrap(), "红");
        assert_eq!(name_of_hex("#00ff00").unwrap(), "绿");
        assert_eq!(name_of_hex("#0000ff").unwrap(), "蓝");
        assert_eq!(name_of_hex("#ffffff").unwrap(), "近白");
        assert_eq!(name_of_hex("#000000").unwrap(), "近黑");
        assert!(name_of_hex("#ffd700").unwrap().contains('黄'));
        assert!(name_of_hex("bad").is_none());
    }

    #[test]
    fn sampling_scales_to_large_images() {
        // 一张 400×400 的图（16 万像素）采样后仍应给出稳定结论
        let bitmap = bmp_of(vec![C::new(10, 200, 30); 400 * 400], 400, 400);
        let f = analyze_bitmap(&bitmap);
        assert!(f.decoded);
        assert_eq!(f.hue_family, "绿色系");
        assert!(f.mean_luma > 0.4 && f.mean_luma < 0.8, "{}", f.mean_luma);
    }

    #[test]
    fn description_never_claims_to_know_the_subject() {
        let f = analyze_bitmap(&bmp_of(vec![C::new(120, 90, 60); 400], 20, 20));
        let d = f.describe();
        // 只允许出现"看起来像是"这种带保留的说法
        assert!(d.contains("看起来像是"), "{d}");
        for forbidden in ["里面有一只", "画的是", "内容是"] {
            assert!(!d.contains(forbidden), "不该断言画面内容：{d}");
        }
    }
}

#[cfg(test)]
impl ImageFacts {
    /// 只给测试用的小断言助手。
    fn render_dominant_contains(&self, needle: &str) -> bool {
        self.palette_label().contains(needle)
    }
}
