//! 表情包目录：把一堆 PNG 变成角色**能用**的东西。
//!
//! # 为什么表情包要有"目录"这层
//!
//! 直接把文件名丢给模型（`happy_01.png`）是没有意义的：模型不知道
//! `bubbletea` 是什么表情、更不知道该在什么时候用它。而如果让上层
//! 硬编码"什么情绪配哪张图"，那换一套图就全废。
//!
//! 所以这里做三件事，把"资源"翻译成"语义"：
//!
//! 1. **从文件名读结构**：`happy_01.png` → 情绪 `happy` + 序号 `01`。
//!    命名规范本身就是元数据，不需要额外的配置文件；
//! 2. **用内置情绪词典补语义**：`happy` → 「开心」+ 一句画面描述 + 情绪效价/唤醒。
//!    认不出来的情绪不报错，退化成"一张名为 xxx 的贴图"——
//!    也就是说**用户随便扔 30 张图进来也能用**，只是描述朴素一点；
//! 3. **渲染成两段文本**：一段进提示词（告诉角色"我有这些图可以发"），
//!    一段用于把用户发来的图翻译成角色看得懂的一句话。
//!
//! # 情绪效价 / 唤醒
//!
//! 每张图带 `valence`（-1 难过 ~ +1 高兴）与 `arousal`（0 平静 ~ 1 激动）。
//! 这不是为了"自动演情绪"——角色的情绪由模型通过 `[情]` 标签决定——
//! 而是给"情绪传染"一个**可关闭、可调、有上限**的默认倾向：
//! 对方递过来一张大哭的表情，角色至少不该无动于衷。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::state::StateDelta;

/// 支持的图片扩展名（小写比较）。
pub const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// 一张表情包。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sticker {
    /// 稳定标识（文件名去掉扩展名），例如 `happy_01`。模型就是用它来选图。
    pub id: String,
    /// 文件名（含扩展名），前端据此取图。
    pub file: String,
    /// 情绪键（文件名里的非数字部分），例如 `happy`。
    pub emotion: String,
    /// 中文标签，例如「开心」。
    pub label: String,
    /// 一句画面描述，供模型判断"这张图放在这里合不合适"。
    pub description: String,
    /// 检索标签。
    pub tags: Vec<String>,
    /// 情绪效价 -1.0 ~ +1.0。
    pub valence: f32,
    /// 情绪唤醒 0.0 ~ 1.0。
    pub arousal: f32,
    /// 序号（文件名尾部的 `_NN`）；没有则为 0。
    pub index: u32,
}

impl Sticker {
    /// 一行摘要（进内存记忆 / 事件流用）。
    pub fn summary(&self) -> String {
        format!("{}（{}）", self.label, self.description)
    }

    /// 这张图对"看到它的人"的情绪影响。
    ///
    /// `strength` 是传染系数：0 表示完全不影响（角色自己做主），
    /// 值越大越容易被对方的情绪带走。结果刻意压得很小——
    /// 它只是给状态机一个倾向，最终演成什么样仍然由模型与角色卡决定。
    pub fn affect(&self, strength: f32) -> StateDelta {
        let s = strength.clamp(0.0, 1.0);
        if s <= f32::EPSILON {
            return StateDelta::default();
        }
        StateDelta {
            valence: Some(round3(self.valence * s)),
            arousal: Some(round3(self.arousal * s)),
            ..Default::default()
        }
    }

    /// 是否命中一个查询词（id / 文件名 / 情绪 / 中文标签 / 标签）。
    fn matches(&self, needle: &str) -> bool {
        let n = needle.trim().to_lowercase();
        if n.is_empty() {
            return false;
        }
        if self.id.to_lowercase() == n || self.emotion.to_lowercase() == n {
            return true;
        }
        if extensionless(&self.file).to_lowercase() == n || self.file.to_lowercase() == n {
            return true;
        }
        if self.label.to_lowercase() == n {
            return true;
        }
        self.tags.iter().any(|t| t.to_lowercase() == n)
    }
}

/// 表情包目录。
#[derive(Debug, Clone, Default)]
pub struct StickerCatalog {
    stickers: Vec<Sticker>,
    by_id: BTreeMap<String, usize>,
}

impl StickerCatalog {
    /// 从文件名列表构建（不碰磁盘，便于测试）。
    ///
    /// 顺序按 `(index, id)` 排：让 `happy_01` 排在 `happy_11` 前面，
    /// 前端按这个顺序铺网格时才是作者期望的顺序，而不是字典序。
    pub fn from_files<S: AsRef<str>>(names: &[S]) -> Self {
        let mut stickers: Vec<Sticker> = names
            .iter()
            .filter_map(|n| build_sticker(n.as_ref()))
            .collect();
        stickers.sort_by(|a, b| a.index.cmp(&b.index).then_with(|| a.id.cmp(&b.id)));

        let mut by_id = BTreeMap::new();
        for (i, s) in stickers.iter().enumerate() {
            by_id.insert(s.id.to_lowercase(), i);
        }
        StickerCatalog { stickers, by_id }
    }

    /// 扫描目录里的图片。
    ///
    /// 目录不存在只返回"空目录"，不报错：表情包是可选装饰，
    /// 一个没有表情包的 Styx 依然应该能开演。
    pub fn load_dir(dir: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return StickerCatalog::default();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        // read_dir 的顺序是文件系统给的，不稳定；排一下让行为可复现
        names.sort();
        StickerCatalog::from_files(&names)
    }

    pub fn is_empty(&self) -> bool {
        self.stickers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.stickers.len()
    }

    /// 全部表情包（已排序）。
    pub fn all(&self) -> &[Sticker] {
        &self.stickers
    }

    /// 按 id（也接受文件名 / 情绪 / 中文标签）查找。
    pub fn find(&self, key: &str) -> Option<&Sticker> {
        let k = key.trim().to_lowercase();
        if k.is_empty() {
            return None;
        }
        if let Some(i) = self.by_id.get(&k) {
            return self.stickers.get(*i);
        }
        self.stickers.iter().find(|s| s.matches(&k))
    }

    /// 查找并返回文件名（前端取图用）。
    pub fn file_of(&self, key: &str) -> Option<&str> {
        self.find(key).map(|s| s.file.as_str())
    }

    /// 文件名是否属于本目录（防止把任意路径当图片读出去）。
    pub fn has_file(&self, file: &str) -> bool {
        self.stickers.iter().any(|s| s.file == file)
    }

    /// 渲染给模型看的清单。
    ///
    /// `cap` 是条数上限：表情包很多时不至于把提示词预算吃光，
    /// 被截断的部分会明说，模型不会以为自己"只有这些图"。
    pub fn render_catalog(&self, cap: usize) -> String {
        let total = self.stickers.len();
        let cap = cap.max(1);
        let shown = total.min(cap);
        let mut s = String::new();
        for st in self.stickers.iter().take(shown) {
            s.push_str(&format!("- {} {}：{}\n", st.id, st.label, st.description));
        }
        if total > shown {
            s.push_str(&format!("（还有 {} 张未列出）\n", total - shown));
        }
        s
    }

    /// 提示词里的"我的表情包"整段。
    pub fn prompt_block(&self, cap: usize) -> String {
        if self.is_empty() {
            return String::new();
        }
        format!(
            "\n## 我有的表情包\n\
             需要的时候我可以用一张贴图代替或补充语言（图片会显示在对方那边）。可选：\n\
             {}\
             用法：单独一行写 `[表情] <编号>`，例如 `[表情] happy_01`。\n\
             规则：一次最多一张；只有在情绪真的到了那一刻才用，不要每轮都发；\
             用了表情包之后，[说] 的台词要更短甚至可以不说话。\n",
            self.render_catalog(cap)
        )
    }

    /// 输出格式说明里追加的一行（没有表情包时返回空串）。
    pub fn format_hint(&self) -> String {
        if self.is_empty() {
            String::new()
        } else {
            "\n[表情] <表情包编号>   ← 可选，从「我有的表情包」里选一张\n".to_string()
        }
    }

    /// 把"对方发来的表情包"翻译成角色看得懂的一句话。
    ///
    /// 刻意用第三人称陈述句而不是 `[贴图:happy_01]` 这种机器格式：
    /// 它要作为用户输入进提示词，越是像人话，模型越知道该怎么接。
    pub fn describe_incoming(&self, who: &str, id: &str) -> Option<String> {
        let st = self.find(id)?;
        let who = if who.trim().is_empty() { "对方" } else { who.trim() };
        Some(format!(
            "（{}发来一张表情包：{}——{}。）",
            who,
            st.label,
            st.description
        ))
    }
}

/// 解析一个文件名。非图片扩展名返回 `None`。
fn build_sticker(name: &str) -> Option<Sticker> {
    let ext = extension(name)?;
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let stem = extensionless(name);
    if stem.is_empty() {
        return None;
    }
    let (emotion, index) = split_index(&stem);
    let (label, description, tags, valence, arousal) = lookup(&emotion);

    let mut all_tags = tags.to_vec();
    all_tags.push(emotion.clone());

    Some(Sticker {
        id: stem.clone(),
        file: name.to_string(),
        emotion,
        label,
        description,
        tags: all_tags,
        valence,
        arousal,
        index,
    })
}

/// `happy_01` → `("happy", 1)`；`狗头` → `("狗头", 0)`。
///
/// 只有尾部**全是数字**时才算序号——否则 `gif_2x` 这类名字会被拆错。
pub fn split_index(stem: &str) -> (String, u32) {
    match stem.rsplit_once('_') {
        Some((head, tail))
            if !head.is_empty() && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) =>
        {
            (head.to_string(), tail.parse::<u32>().unwrap_or(0))
        }
        _ => (stem.to_string(), 0),
    }
}

fn extension(name: &str) -> Option<String> {
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_lowercase())
}

fn extensionless(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()) => {
            stem.to_string()
        }
        _ => name.to_string(),
    }
}

fn round3(v: f32) -> f32 {
    (v * 1000.0).round() / 1000.0
}

/// 内置情绪词典：认得出就用，认不出就兜底。
///
/// `(情绪键, 中文标签, 画面描述, 附加标签, 效价, 唤醒)`
type Builtin = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    f32,
    f32,
);

const BUILTINS: [Builtin; 30] = [
    ("happy", "开心", "笑得眼睛弯成月牙，整个人都在发亮", &["愉快", "正面"], 0.80, 0.60),
    ("angry", "生气", "眉头拧成一团，头顶冒着火苗", &["愤怒", "负面"], -0.70, 0.85),
    ("cry", "哭泣", "眼泪大颗大颗往下掉，鼻尖红红的", &["难过", "负面"], -0.75, 0.50),
    ("shy", "害羞", "脸红到耳根，手指绞在一起", &["脸红", "正面"], 0.50, 0.60),
    ("surprised", "惊讶", "眼睛瞪得浑圆，嘴巴张成一个圆", &["吃惊", "意外"], 0.20, 0.85),
    ("sleepy", "困", "眼皮直打架，脑袋一点一点往下垂", &["疲惫"], -0.10, 0.15),
    ("confused", "疑惑", "歪着脑袋，头顶浮着一个问号", &["不解"], -0.10, 0.40),
    ("proud", "得意", "下巴微抬，嘴角压不住地往上翘", &["骄傲", "正面"], 0.60, 0.55),
    ("grievance", "委屈", "嘴唇抿成一条线，眼眶里转着泪", &["难过"], -0.60, 0.45),
    ("cheer", "加油", "握紧拳头举过头顶，旁边点着火星", &["鼓励", "正面"], 0.65, 0.70),
    ("love", "爱意", "眼里冒着一串心形泡泡，脸贴得很近", &["喜欢", "正面"], 0.85, 0.60),
    ("thanks", "感谢", "双手合十，用力地点头", &["礼貌", "正面"], 0.60, 0.40),
    ("bye", "再见", "挥着手往后退，脚步轻快", &["告别"], 0.10, 0.35),
    ("speechless", "无语", "嘴角抽动，额角挂着一滴汗", &["无奈"], -0.30, 0.30),
    ("thinking", "思考", "支着下巴望向远处，头顶飘着一串省略号", &["犹豫"], 0.00, 0.35),
    ("wicked", "坏笑", "眯着眼笑，露出一颗小虎牙", &["狡黠"], 0.20, 0.55),
    ("eating", "干饭", "捧着一碗饭，两颊塞得鼓鼓的", &["吃饭", "满足"], 0.50, 0.35),
    ("bubbletea", "奶茶", "举着一杯珍珠奶茶，吸管咬在嘴里", &["喝奶茶", "愉快"], 0.60, 0.40),
    ("scared", "害怕", "缩成一团，牙齿在打颤", &["恐惧", "负面"], -0.75, 0.85),
    ("cutesy", "卖萌", "两手托腮，眼睛眨巴眨巴", &["可爱", "正面"], 0.60, 0.45),
    ("wailing", "大哭", "张着嘴号啕大哭，眼泪喷出来", &["崩溃", "负面"], -0.85, 0.90),
    ("disgusted", "嫌弃", "皱着鼻子把脸别到一边", &["厌恶", "负面"], -0.60, 0.50),
    ("fingerheart", "比心", "两只手在胸前拼出一颗心", &["喜欢", "正面"], 0.80, 0.50),
    ("cheers", "干杯", "举起杯子碰过来，眼睛弯弯的", &["庆祝", "正面"], 0.70, 0.65),
    ("peeking", "偷看", "从门缝后面探出半个脑袋", &["好奇"], 0.20, 0.50),
    ("dizzy", "晕", "眼睛转成蚊香圈，身子在轻轻摇晃", &["混乱"], -0.30, 0.60),
    ("frustrated", "烦躁", "抓着头发，脚边堆着一团乱线", &["恼火", "负面"], -0.60, 0.75),
    ("praying", "祈祷", "双手合十闭着眼，头顶有一道柔光", &["恳求"], 0.10, 0.30),
    ("stomping", "跺脚", "用力跺着脚，脸涨得通红", &["生气", "负面"], -0.60, 0.85),
    ("facepalm", "扶额", "一手拍在额头上，无力地叹了口气", &["无奈"], -0.35, 0.40),
];

/// 查词典；查不到就兜底成"一张叫 xxx 的贴图"。
fn lookup(emotion: &str) -> (String, String, Vec<String>, f32, f32) {
    let key = emotion.to_lowercase();
    for (k, label, desc, tags, v, a) in BUILTINS {
        if k == key {
            return (
                label.to_string(),
                desc.to_string(),
                tags.iter().map(|t| t.to_string()).collect(),
                v,
                a,
            );
        }
    }
    (
        emotion.to_string(),
        format!("一张“{emotion}”的贴图"),
        Vec::new(),
        0.0,
        0.4,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        [
            "happy_01.png",
            "bubbletea_19.png",
            "cry_03.png",
            "stomping_32.png",
            "angry_02.png",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn parses_emotion_and_index_from_filename() {
        assert_eq!(split_index("happy_01"), ("happy".to_string(), 1));
        assert_eq!(split_index("facepalm_31"), ("facepalm".to_string(), 31));
        // 尾部不是数字就不拆：`gif_2x` 不该被切成 `gif` + `2x`
        assert_eq!(split_index("gif_2x"), ("gif_2x".to_string(), 0));
        assert_eq!(split_index("狗头"), ("狗头".to_string(), 0));
        assert_eq!(split_index("_07"), ("_07".to_string(), 0));
    }

    #[test]
    fn builds_a_catalog_sorted_by_index() {
        let c = StickerCatalog::from_files(&names());
        assert_eq!(c.len(), 5);
        let ids: Vec<&str> = c.all().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["happy_01", "angry_02", "cry_03", "bubbletea_19", "stomping_32"]
        );
    }

    #[test]
    fn known_emotions_get_labels_and_descriptions() {
        let c = StickerCatalog::from_files(&names());
        let happy = c.find("happy_01").unwrap();
        assert_eq!(happy.label, "开心");
        assert!(happy.description.contains("眼睛"));
        assert!(happy.valence > 0.5);
        assert!(happy.tags.contains(&"正面".to_string()));
    }

    #[test]
    fn unknown_emotions_degrade_gracefully() {
        // 用户随便扔进来的图不该让目录崩掉，也不该被静默丢弃
        let c = StickerCatalog::from_files(&["不明生物_07.png", "README.md", "note.txt"]);
        assert_eq!(c.len(), 1, "非图片文件要被忽略");
        let st = c.find("不明生物_07").unwrap();
        assert_eq!(st.label, "不明生物");
        assert!(st.description.contains("不明生物"));
        assert_eq!(st.valence, 0.0);
    }

    #[test]
    fn supports_common_image_extensions() {
        let c = StickerCatalog::from_files(&[
            "a_1.PNG",
            "b_2.jpg",
            "c_3.jpeg",
            "d_4.gif",
            "e_5.webp",
            "f_6.bmp",
        ]);
        assert_eq!(c.len(), 5, "bmp 不在支持列表里");
        assert!(c.find("a_1").is_some(), "扩展名大小写应当被接受");
    }

    #[test]
    fn find_accepts_id_file_emotion_and_label() {
        let c = StickerCatalog::from_files(&names());
        assert_eq!(c.find("happy_01").unwrap().id, "happy_01");
        assert_eq!(c.find("happy_01.png").unwrap().id, "happy_01");
        assert_eq!(c.find("HAPPY").unwrap().id, "happy_01");
        assert_eq!(c.find("开心").unwrap().id, "happy_01");
        assert!(c.find("不存在").is_none());
        assert!(c.find("   ").is_none());
    }

    #[test]
    fn file_lookup_is_whitelisted() {
        let c = StickerCatalog::from_files(&names());
        assert!(c.has_file("happy_01.png"));
        assert!(!c.has_file("../../etc/passwd"));
        assert!(!c.has_file("happy_01.png.bak"));
        assert_eq!(c.file_of("开心"), Some("happy_01.png"));
    }

    #[test]
    fn catalog_renders_for_prompt_with_cap() {
        let c = StickerCatalog::from_files(&names());
        let block = c.prompt_block(4);
        assert!(block.contains("## 我有的表情包"));
        assert!(block.contains("- happy_01 开心"));
        assert!(block.contains("还有 1 张未列出"), "超出上限要明说");
        assert!(block.contains("[表情] <编号>"));
        assert!(c.format_hint().contains("[表情]"));
    }

    #[test]
    fn empty_catalog_renders_nothing() {
        let c = StickerCatalog::default();
        assert!(c.is_empty());
        assert!(c.prompt_block(10).is_empty());
        assert!(c.format_hint().is_empty());
        assert!(c.describe_incoming("陈默", "happy_01").is_none());
    }

    #[test]
    fn incoming_sticker_becomes_a_sentence() {
        let c = StickerCatalog::from_files(&names());
        let line = c.describe_incoming("陈默", "cry_03").unwrap();
        assert!(line.starts_with("（陈默发来一张表情包：哭泣"));
        assert!(line.ends_with("）"));
        // 认不出的 id 不该凭空编出一句话
        assert!(c.describe_incoming("陈默", "nope_99").is_none());
    }

    #[test]
    fn affect_is_scaled_and_clamped() {
        let c = StickerCatalog::from_files(&names());
        let happy = c.find("happy_01").unwrap();
        let d = happy.affect(0.1);
        assert!((d.valence.unwrap() - 0.08).abs() < 1e-6);
        assert!((d.arousal.unwrap() - 0.06).abs() < 1e-6);

        // 0 强度 = 完全不传染（角色自己决定情绪）
        assert!(happy.affect(0.0).is_empty());
        // 超过 1 的系数被夹住，不会把状态一次性推爆
        let strong = happy.affect(99.0);
        assert!(strong.valence.unwrap() <= 0.8 + 1e-6);
    }

    #[test]
    fn builtin_dictionary_is_self_consistent() {
        for (k, label, desc, tags, v, a) in BUILTINS {
            assert!(!k.is_empty());
            assert!(!label.is_empty(), "{k}");
            assert!(!desc.is_empty(), "{k}");
            assert!(!tags.is_empty(), "{k}");
            assert!((-1.0..=1.0).contains(&v), "{k} 效价越界");
            assert!((0.0..=1.0).contains(&a), "{k} 唤醒越界");
        }
        // 词典键必须唯一，否则后面的条目永远查不到
        let mut keys: Vec<&str> = BUILTINS.iter().map(|b| b.0).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "内置词典里有重复的情绪键");
    }

    #[test]
    fn load_dir_ignores_missing_directory() {
        let c = StickerCatalog::load_dir(Path::new("这个目录不存在-xyz"));
        assert!(c.is_empty());
    }
}
