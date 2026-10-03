//! # 可插拔的视觉后端
//!
//! 图片理解被拆成四层，**上层永远不该成为单点**：
//!
//! ```text
//!   ① 本地事实    LocalFacts      零依赖手写解码 + 统计        ← 永远是基底，一定会跑
//!   ② 目标检测    RemoteVision    YOLO ONNX / 任意 HTTP 端点   ← 可选
//!   ③ 属性反推    RemoteTagger    WD14 tagger / CLIP 打分      ← 可选
//!   ④ 视觉模型    OpenAiVision    ollama 视觉模型 / 兼容端点    ← 可选
//! ```
//!
//! ②③④ 是**同一个档次**的：都可以没有，都可以换成别人写的端点。
//! 区别只在它们各说一句不同的话：
//!
//! | 层 | 回答的问题 | 一处典型输出 |
//! |---|---|---|
//! | ② 检测 | 有什么东西、在哪儿 | `人（左上，82% 把握）` |
//! | ③ 反推 | 画面整体有哪些属性 | `主体：1girl、solo；神情：smile` |
//! | ④ 视觉模型 | 这画面在讲什么 | `像是一间堆满旧书的房间` |
//!
//! ## 为什么坚持"本地事实"必须是基底
//!
//! 因为依赖模型会带来三种崩溃方式，而它们都会表现为"角色突然看不见图了"：
//! 端点没起、模型被换成一个不支持视觉的、推理超时或显存不够。
//! 这三种情况在本机几乎必然发生（用户会换模型、会关服务、会同时跑别的推理）。
//!
//! 所以设计上：
//! - [`crate::facts::analyze`] 的结果**先于**任何远程调用产生，并且一定会进提示词；
//! - 远程后端只能**追加**信息（[`Detection`]、[`TagHit`]、一句 caption），不能覆盖本地事实；
//! - 远程失败只写一条 `note`，回合照常进行。
//!
//! ## 合并而不是替换
//!
//! 一个真实的坏例子：YOLO 在昏暗的旧照片上什么都没检出来。如果描述是
//! "一张图片，没有可识别的物体"，角色就会得到完全错误的印象；而本地事实
//! 说的是"一张泛黄的黑白照片，整体偏暗、颗粒感明显，像是扫描的老照片"——
//! 后者对角色扮演有用得多。所以这四层的关系是**叠加**。

use serde::{Deserialize, Serialize};

use crate::facts::ImageFacts;
use crate::tags::{self, TagHit};

/// 一次目标检测的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// 英文标签（COCO 类别名等）。
    pub label: String,
    /// 中文标签（后端能给就给，给不了就空）。
    #[serde(default)]
    pub label_zh: String,
    /// 置信度 0..1。
    pub confidence: f32,
    /// 归一化的 `[x1, y1, x2, y2]`（0..1，左上原点）。
    #[serde(default)]
    pub r#box: Vec<f32>,
    /// 画面占比 0..1（用来判断"主体"还是"角落里的东西"）。
    #[serde(default)]
    pub area: f32,
}

impl Detection {
    pub fn display_name(&self) -> &str {
        if self.label_zh.is_empty() {
            &self.label
        } else {
            &self.label_zh
        }
    }

    /// 相对位置描述（左侧 / 中央 / 右上 …）。
    pub fn position_label(&self) -> &'static str {
        if self.r#box.len() < 4 {
            return "";
        }
        let cx = (self.r#box[0] + self.r#box[2]) / 2.0;
        let cy = (self.r#box[1] + self.r#box[3]) / 2.0;
        match (cx, cy) {
            (x, y) if x < 0.34 && y < 0.34 => "左上",
            (x, y) if x > 0.66 && y < 0.34 => "右上",
            (x, y) if x < 0.34 && y > 0.66 => "左下",
            (x, y) if x > 0.66 && y > 0.66 => "右下",
            (x, _) if x < 0.34 => "左侧",
            (x, _) if x > 0.66 => "右侧",
            (_, y) if y < 0.34 => "上方",
            (_, y) if y > 0.66 => "下方",
            _ => "画面中央",
        }
    }
}

/// 一个可以补充图片理解的后端。
///
/// 实现者只需要管"我能不能多说一点关于这张图的事"，不必关心图片从哪来、
/// 描述最终怎么拼——那是 [`Composer`] 的事。
pub trait VisionBackend: Send + Sync {
    /// 实现名（日志与状态展示用）。
    fn name(&self) -> &str;

    /// 后端是否可用。返回 false 时上层会跳过它并如实说明。
    fn health(&self) -> bool;

    /// 补充一段理解。
    ///
    /// 返回 `Ok(None)` 表示"我看过了，没什么可补充的"，与 `Err`（出错）
    /// 区分开：前者是正常的沉默，后者要进降级提示。
    fn describe(&self, image: &[u8], facts: &ImageFacts) -> crate::Result<Option<String>>;

    /// 目标检测（后端不支持就返回 `Ok(Vec::new())`）。
    fn detect(&self, _image: &[u8]) -> crate::Result<Vec<Detection>> {
        Ok(Vec::new())
    }

    /// 反向推断属性标签（"提示词反推"，后端不支持就返回 `Ok(Vec::new())`）。
    ///
    /// 默认实现是空——一个只做检测的 YOLO 端点不需要为此写任何代码。
    fn tag(&self, _image: &[u8]) -> crate::Result<Vec<TagHit>> {
        Ok(Vec::new())
    }
}

/// 把本地事实与若干可选的远程后端拼成最终的描述。
pub struct Composer {
    backends: Vec<Box<dyn VisionBackend>>,
}

impl std::fmt::Debug for Composer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Composer")
            .field("backends", &self.names())
            .finish()
    }
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    /// 只有本地事实（没有任何远程后端）。
    pub fn new() -> Self {
        Composer {
            backends: Vec::new(),
        }
    }

    pub fn with(mut self, backend: Box<dyn VisionBackend>) -> Self {
        self.backends.push(backend);
        self
    }

    pub fn names(&self) -> Vec<String> {
        self.backends.iter().map(|b| b.name().to_string()).collect()
    }

    /// 跑完本地事实 + 所有可用后端，返回合并后的描述与用到的后端名单。
    ///
    /// 关键性质：**这个函数一定返回一段描述**。哪怕所有远程后端都挂了，
    /// 本地事实也还在，调用方永远不需要处理"没有描述"的分支。
    pub fn describe(&self, image: &[u8]) -> (ImageFacts, String, Vec<String>) {
        let mut facts = crate::facts::analyze(image);
        let mut used: Vec<String> = Vec::new();

        for backend in &self.backends {
            if !backend.health() {
                facts
                    .notes
                    .push(format!("{} 不可用，已跳过", backend.name()));
                continue;
            }
            match backend.detect(image) {
                Ok(found) if !found.is_empty() => {
                    used.push(format!("{}(检测)", backend.name()));
                    let mut merged = std::mem::take(&mut facts.objects);
                    merged.extend(found);
                    merged.sort_by(|a, b| {
                        b.confidence
                            .partial_cmp(&a.confidence)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    merged.truncate(12);
                    facts.objects = merged;
                }
                Ok(_) => {}
                Err(e) => facts
                    .notes
                    .push(format!("{} 检测失败：{e}", backend.name())),
            }
            match backend.tag(image) {
                Ok(found) if !found.is_empty() => {
                    used.push(format!("{}(反推)", backend.name()));
                    let mut merged = std::mem::take(&mut facts.tags);
                    merged.extend(found);
                    // 归组、去重、限流都在 tags 层做——多个后端同时给标签时，
                    // 只有这样才不会被任何一个后端的词表风格带偏。
                    tags::prune(&mut merged, tags::DEFAULT_MIN_SCORE);
                    facts.tags = merged;
                }
                Ok(_) => {}
                Err(e) => facts
                    .notes
                    .push(format!("{} 反推失败：{e}", backend.name())),
            }
            match backend.describe(image, &facts) {
                Ok(Some(text)) if !text.trim().is_empty() => {
                    used.push(backend.name().to_string());
                    facts.captions.push(text);
                }
                Ok(_) => {}
                Err(e) => facts
                    .notes
                    .push(format!("{} 理解失败：{e}", backend.name())),
            }
        }

        let text = facts.describe();
        (facts, text, used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;

    struct Stub {
        name: &'static str,
        healthy: bool,
        caption: Option<&'static str>,
        objects: Vec<Detection>,
        tags: Vec<TagHit>,
    }

    impl VisionBackend for Stub {
        fn name(&self) -> &str {
            self.name
        }
        fn health(&self) -> bool {
            self.healthy
        }
        fn describe(&self, _i: &[u8], _f: &ImageFacts) -> Result<Option<String>> {
            Ok(self.caption.map(String::from))
        }
        fn detect(&self, _i: &[u8]) -> Result<Vec<Detection>> {
            Ok(self.objects.clone())
        }
        fn tag(&self, _i: &[u8]) -> Result<Vec<TagHit>> {
            Ok(self.tags.clone())
        }
    }

    fn det(label: &str, conf: f32) -> Detection {
        Detection {
            label: label.into(),
            label_zh: String::new(),
            confidence: conf,
            r#box: vec![0.0, 0.0, 1.0, 1.0],
            area: 1.0,
        }
    }

    /// 一个 2×2 的真实 PNG（和 decode 的测试同一份构造思路）。
    fn tiny_png() -> Vec<u8> {
        fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            png.extend_from_slice(&(body.len() as u32).to_be_bytes());
            png.extend_from_slice(kind);
            png.extend_from_slice(body);
            png.extend_from_slice(&0u32.to_be_bytes());
        }
        let raw = [
            0u8, 0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0, 0, 0x00, 0xff, 0xff, 0xff, 0xff,
        ];
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        // stored 块
        let mut z = vec![0x78, 0x01, 0x01];
        z.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        z.extend_from_slice(&raw);
        z.extend_from_slice(&0u32.to_be_bytes());
        chunk(&mut png, b"IDAT", &z);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn without_backends_local_facts_stand_alone() {
        let composer = Composer::new();
        let (facts, text, used) = composer.describe(&tiny_png());
        assert!(facts.decoded);
        assert!(used.is_empty());
        assert!(text.contains("2×2"), "{text}");
    }

    #[test]
    fn a_detector_result_is_merged_not_replacing() {
        let composer = Composer::new().with(Box::new(Stub {
            name: "yolo",
            healthy: true,
            caption: None,
            objects: vec![det("person", 0.9), det("book", 0.6)],
            tags: Vec::new(),
        }));
        let (facts, text, used) = composer.describe(&tiny_png());
        assert!(facts.decoded, "本地事实必须还在");
        assert_eq!(facts.objects.len(), 2);
        assert_eq!(used, vec!["yolo(检测)".to_string()]);
        assert!(text.contains("person"), "{text}");
    }

    #[test]
    fn detections_are_sorted_and_capped() {
        let many: Vec<Detection> = (0..20)
            .map(|i| det(&format!("obj{i}"), 0.1 * i as f32))
            .collect();
        let composer = Composer::new().with(Box::new(Stub {
            name: "yolo",
            healthy: true,
            caption: None,
            objects: many,
            tags: Vec::new(),
        }));
        let (facts, _, _) = composer.describe(&tiny_png());
        assert_eq!(facts.objects.len(), 12);
        assert_eq!(facts.objects[0].label, "obj19", "置信度高的排前面");
    }

    #[test]
    fn a_dead_backend_degrades_with_a_note_and_still_returns_text() {
        let composer = Composer::new().with(Box::new(Stub {
            name: "yolo",
            healthy: false,
            caption: Some("不该出现"),
            objects: vec![det("person", 0.9)],
            tags: Vec::new(),
        }));
        let (facts, text, used) = composer.describe(&tiny_png());
        assert!(used.is_empty());
        assert!(facts.objects.is_empty());
        assert!(facts.notes.iter().any(|n| n.contains("不可用")));
        // 最关键的一条：远程挂了也要有可用描述
        assert!(!text.is_empty());
        assert!(text.contains("2×2"));
    }

    #[test]
    fn captions_are_appended_to_the_local_description() {
        let composer = Composer::new().with(Box::new(Stub {
            name: "vlm",
            healthy: true,
            caption: Some("画面里似乎是一间堆满书的房间。"),
            objects: Vec::new(),
            tags: Vec::new(),
        }));
        let (facts, text, used) = composer.describe(&tiny_png());
        assert_eq!(used, vec!["vlm".to_string()]);
        assert_eq!(facts.captions.len(), 1);
        assert!(text.contains("堆满书"), "{text}");
    }

    #[test]
    fn detection_position_labels_are_readable() {
        let mut d = det("cat", 0.5);
        d.r#box = vec![0.05, 0.05, 0.2, 0.2];
        assert_eq!(d.position_label(), "左上");
        d.r#box = vec![0.8, 0.8, 0.95, 0.95];
        assert_eq!(d.position_label(), "右下");
        d.r#box = vec![0.4, 0.4, 0.6, 0.6];
        assert_eq!(d.position_label(), "画面中央");
        d.r#box = vec![];
        assert_eq!(d.position_label(), "");
    }

    #[test]
    fn display_name_prefers_chinese() {
        let mut d = det("book", 0.5);
        assert_eq!(d.display_name(), "book");
        d.label_zh = "书".into();
        assert_eq!(d.display_name(), "书");
    }

    fn tag(t: &str, score: f32) -> TagHit {
        TagHit::new(t, score)
    }

    #[test]
    fn reverse_inferred_tags_land_in_the_description() {
        let composer = Composer::new().with(Box::new(Stub {
            name: "wd14",
            healthy: true,
            caption: None,
            objects: Vec::new(),
            tags: vec![
                tag("1girl", 0.95),
                tag("long_hair", 0.9),
                tag("smile", 0.85),
            ],
        }));
        let (facts, text, used) = composer.describe(&tiny_png());
        assert_eq!(facts.tags.len(), 3);
        assert_eq!(used, vec!["wd14(反推)".to_string()]);
        assert!(text.contains("主体：1girl"), "{text}");
        assert!(text.contains("外貌：long hair"), "{text}");
    }

    #[test]
    fn tags_from_two_backends_are_merged_and_deduped() {
        let a = Composer::new()
            .with(Box::new(Stub {
                name: "wd14",
                healthy: true,
                caption: None,
                objects: Vec::new(),
                tags: vec![tag("1girl", 0.9), tag("long_hair", 0.8)],
            }))
            .with(Box::new(Stub {
                name: "clip",
                healthy: true,
                caption: None,
                objects: Vec::new(),
                tags: vec![tag("1girl", 0.7), tag("indoors", 0.6)],
            }));
        let (facts, _, used) = a.describe(&tiny_png());
        assert_eq!(facts.tags.len(), 3, "1girl 只该出现一次");
        assert_eq!(used.len(), 2, "两个后端都该被记下来");
    }

    #[test]
    fn a_semantic_tag_suppresses_the_texture_guess() {
        // 标签说"这是教室"，纹理再猜一句"像是风景"就会互相打架
        let composer = Composer::new().with(Box::new(Stub {
            name: "wd14",
            healthy: true,
            caption: None,
            objects: Vec::new(),
            tags: vec![tag("classroom", 0.9)],
        }));
        let (_, text, _) = composer.describe(&tiny_png());
        assert!(!text.contains("仅从这些特征看"), "{text}");
        assert!(text.contains("classroom"), "{text}");
    }

    #[test]
    fn a_failed_tagger_degrades_with_a_note_and_keeps_the_local_facts() {
        struct BadTagger;
        impl VisionBackend for BadTagger {
            fn name(&self) -> &str {
                "wd14"
            }
            fn health(&self) -> bool {
                true
            }
            fn describe(&self, _i: &[u8], _f: &ImageFacts) -> Result<Option<String>> {
                Ok(None)
            }
            fn tag(&self, _i: &[u8]) -> Result<Vec<TagHit>> {
                Err(crate::VisionError::Transport("boom".into()))
            }
        }
        let composer = Composer::new().with(Box::new(BadTagger));
        let (facts, text, used) = composer.describe(&tiny_png());
        assert!(used.is_empty());
        assert!(facts.tags.is_empty());
        assert!(facts.notes.iter().any(|n| n.contains("反推失败")));
        assert!(text.contains("2×2"), "本地事实必须还在：{text}");
    }
}
