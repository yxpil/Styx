//! # 图片素材库
//!
//! 角色能"发"的东西有两类，各自走各自的通道：
//!
//! - **表情包**（[`crate::sticker::StickerCatalog`]）：按**情绪**组织，
//!   提示词里给的是"开心 / 大哭 / 无语"这样的情绪词典。它回答的是
//!   "角色的心情长什么样"。
//! - **图片 / 照片**（本模块）：按**内容**组织，提示词里给的是图片的
//!   描述与标签。它回答的是"角色想给你看什么东西"。
//!
//! 之所以不合并成一个"素材库"：两者的提示词表达方式、合法性裁决、
//! 前端呈现尺寸都不一样。合并只会让"表情"和"照片"这两个概念都变糊。
//!
//! ## 三个来源
//!
//! | 来源 | 说明 | 能否被删除 |
//! |---|---|---|
//! | `library` | 启动时扫描素材目录得到，属于这个角色的"收藏" | 否 |
//! | `upload` | 对话中用户发过来的图，登记后角色以后可以再发回来 | 是 |
//! | `model` | 由图像分析等程序化路径写入 | 是 |
//!
//! ## 为什么用 `RwLock`
//!
//! 素材库被 [`crate::kernel::Kernel`] 与 HTTP 服务**同时持有**（两边都是
//! `Arc<MediaLibrary>`），而"用户发了一张图 → 登记进库"发生在服务端线程里、
//! 读取发生在内核线程里。没有内部可变性就只能整库重建，那会让正在进行的
//! 回合读到半截状态。
//!
//! 锁的粒度刻意很粗（整库一把锁）：这里的热路径只有"取一张图的元数据"，
//! 纳秒级；复杂化锁结构带来的收益远小于它引入的调试成本。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::error::{Result, StyxError};
use crate::text::truncate_to_tokens;

/// 认得的图片扩展名（也是对外提供文件服务时的白名单）。
pub const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// 素材库里最多保留多少张。
///
/// 存在的意义不是"省内存"，而是防止一个长时间运行的会话被不断上传的图
/// 撑到提示词装不下——每多一张就多一行清单，而清单是固定段，裁不掉。
pub const MAX_PHOTOS: usize = 400;

/// 单张图片文件名长度上限（防御畸形输入）。
pub const MAX_FILE_NAME: usize = 128;

/// 素材来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaSource {
    /// 启动时从素材目录扫描得到。
    Library,
    /// 对话中用户发过来的。
    Upload,
    /// 程序化写入（图像分析等）。
    Model,
}

impl MediaSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaSource::Library => "library",
            MediaSource::Upload => "upload",
            MediaSource::Model => "model",
        }
    }
}

/// 一张可用图片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Photo {
    /// 稳定编号（文件名去扩展名）。
    pub id: String,
    /// 真实文件名（含扩展名），用于拼 URL。
    pub file: String,
    /// 人类可读描述，进提示词与前端。
    pub caption: String,
    /// 标签，用于检索。
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: MediaSource,
    /// 加入时间（Unix 毫秒）。
    #[serde(default)]
    pub at: i64,
    /// 图像分析得到的一句话描述（由上层用 styx-vision 算好传进来）。
    ///
    /// 核心库不认识图像分析，所以这里只存一个字符串——避免
    /// `styx-core` 依赖 `styx-vision` 形成方向别扭的依赖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<String>,
}

impl Photo {
    /// 从文件名造一张图。
    pub fn from_file(file: &str) -> Option<Self> {
        let file = file.trim();
        if file.is_empty() || file.len() > MAX_FILE_NAME {
            return None;
        }
        let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
        let id = stem.trim().to_string();
        if id.is_empty() {
            return None;
        }
        let caption = caption_from_stem(&id);
        Some(Photo {
            id,
            file: file.to_string(),
            caption,
            tags: tags_from_stem(stem),
            source: MediaSource::Library,
            at: crate::event::now_millis(),
            facts: None,
        })
    }

    pub fn with_source(mut self, source: MediaSource) -> Self {
        self.source = source;
        self
    }

    pub fn with_caption(mut self, caption: impl Into<String>) -> Self {
        self.caption = caption.into();
        self
    }

    pub fn with_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_facts(mut self, facts: impl Into<String>) -> Self {
        self.facts = Some(facts.into());
        self
    }

    /// 提示词 / 前端展示用的一行。
    pub fn render(&self) -> String {
        let mut line = format!("{}（{}）", self.id, self.caption);
        if !self.tags.is_empty() {
            line.push_str(&format!(" [{}]", self.tags.join("/")));
        }
        if let Some(f) = &self.facts {
            line.push_str(&format!(" —— {}", truncate_to_tokens(f, 40)));
        }
        line
    }

    /// 判断一个键是否指向这张图。
    pub fn matches(&self, key: &str) -> bool {
        let key = key.trim().trim_matches(['"', '\'', '「', '」']);
        if key.is_empty() {
            return false;
        }
        let lower = key.to_lowercase();
        if self.id.to_lowercase() == lower || self.file.to_lowercase() == lower {
            return true;
        }
        // 允许模型写带扩展名的编号
        if let Some((stem, _)) = lower.rsplit_once('.') {
            if self.id.to_lowercase() == stem {
                return true;
            }
        }
        if !self.caption.is_empty() && self.caption.to_lowercase() == lower {
            return true;
        }
        self.tags.iter().any(|t| t.to_lowercase() == lower)
    }
}

#[derive(Default)]
struct Inner {
    photos: Vec<Photo>,
    by_id: BTreeMap<String, usize>,
}

impl Inner {
    fn reindex(&mut self) {
        self.by_id.clear();
        for (i, p) in self.photos.iter().enumerate() {
            self.by_id.insert(p.id.to_lowercase(), i);
        }
    }

    /// 查一张图：先精确命中，再退化成"包含"匹配。
    fn find(&self, key: &str) -> Option<Photo> {
        let lower = key.trim().to_lowercase();
        if lower.is_empty() {
            return None;
        }
        if let Some(&i) = self.by_id.get(&lower) {
            return self.photos.get(i).cloned();
        }
        self.photos.iter().find(|p| p.matches(key)).cloned()
    }
}

/// 图片素材库。
pub struct MediaLibrary {
    inner: RwLock<Inner>,
    dir: Option<PathBuf>,
}

impl std::fmt::Debug for MediaLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaLibrary")
            .field("len", &self.len())
            .field("dir", &self.dir)
            .finish()
    }
}

impl Default for MediaLibrary {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaLibrary {
    /// 空库。
    pub fn new() -> Self {
        MediaLibrary {
            inner: RwLock::new(Inner::default()),
            dir: None,
        }
    }

    /// 从文件名列表构建（测试与程序化构造的入口）。
    pub fn from_files<S: AsRef<str>>(files: &[S]) -> Self {
        let mut photos: Vec<Photo> = files.iter().filter_map(|f| Photo::from_file(f.as_ref())).collect();
        photos.sort_by(|a, b| a.id.cmp(&b.id));
        photos.dedup_by(|a, b| a.id == b.id);
        let mut inner = Inner {
            photos,
            by_id: BTreeMap::new(),
        };
        inner.reindex();
        MediaLibrary {
            inner: RwLock::new(inner),
            dir: None,
        }
    }

    /// 扫描一个目录。
    ///
    /// **目录不存在不是错误**——没准备素材的用户不该连程序都起不来。
    pub fn load_dir(dir: &Path) -> Self {
        let mut files: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if looks_like_image(&name) {
                    files.push(name);
                }
            }
        }
        files.sort();
        let mut lib = Self::from_files(&files);
        lib.dir = Some(dir.to_path_buf());
        lib
    }

    /// 素材目录（若有）。
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn len(&self) -> usize {
        self.inner.read().map(|i| i.photos.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 全部图片的快照。
    pub fn photos(&self) -> Vec<Photo> {
        self.inner
            .read()
            .map(|i| i.photos.clone())
            .unwrap_or_default()
    }

    /// 找一张图。允许用编号、文件名、描述或标签。
    pub fn find(&self, key: &str) -> Option<Photo> {
        self.inner.read().ok().and_then(|i| i.find(key))
    }

    /// 这个文件名在不在库里（对外提供文件服务时的准入判定）。
    pub fn has_file(&self, file: &str) -> bool {
        let file = file.trim();
        if file.is_empty() || file.len() > MAX_FILE_NAME {
            return false;
        }
        self.inner
            .read()
            .map(|i| i.photos.iter().any(|p| p.file == file))
            .unwrap_or(false)
    }

    /// 登记一张图。
    ///
    /// 同一个 `file` 再次登记会**更新**而不是新增：用户反复发同一张图，
    /// 不该在清单里出现 20 行。
    pub fn register(&self, photo: Photo) -> Result<Photo> {
        if photo.file.trim().is_empty() {
            return Err(StyxError::Other("素材缺少文件名".into()));
        }
        let mut inner = self
            .inner
            .write()
            .map_err(|_| StyxError::Other("素材库锁已损坏".into()))?;
        if let Some(&i) = inner.by_id.get(&photo.id.to_lowercase()) {
            let merged = Photo {
                // 保留原来源：上传过的图不会因为再登记一次就变成"收藏"
                source: inner.photos[i].source,
                at: inner.photos[i].at,
                ..photo
            };
            inner.photos[i] = merged.clone();
            return Ok(merged);
        }
        if inner.photos.len() >= MAX_PHOTOS {
            return Err(StyxError::Other(format!(
                "素材库已满（上限 {MAX_PHOTOS} 张）"
            )));
        }
        inner.photos.push(photo.clone());
        inner.reindex();
        Ok(photo)
    }

    /// 渲染"可用图片清单"（进提示词）。
    pub fn render_catalog(&self, cap: usize) -> String {
        let inner = match self.inner.read() {
            Ok(i) => i,
            Err(_) => return String::new(),
        };
        if inner.photos.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        for p in inner.photos.iter().take(cap) {
            out.push_str(&format!("- {}\n", p.render()));
        }
        if inner.photos.len() > cap {
            out.push_str(&format!(
                "（还有 {} 张未列出，需要时用你记得的编号直接发）\n",
                inner.photos.len() - cap
            ));
        }
        out
    }

    /// 输出格式提示里追加的一行。
    pub fn format_hint(&self) -> String {
        if self.is_empty() {
            String::new()
        } else {
            "\n[图片] <素材编号> ｜ 可选配文   ← 给对方看一张图（编号见图片清单）".to_string()
        }
    }

    /// 把用户发来的图**转述**成角色看得懂的一句话。
    pub fn describe_incoming(&self, who: &str, id: &str) -> Option<String> {
        let photo = self.find(id)?;
        let mut line = format!("{who}给你看了一张图片：{}", photo.caption);
        if let Some(f) = &photo.facts {
            line.push_str(&format!("（{f}）"));
        }
        Some(line)
    }
}

/// 文件名是否像是我们支持的图片。
pub fn looks_like_image(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty() && IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str())
        }
        None => false,
    }
}

/// 文件名主干的扩展名（小写）。
pub fn image_extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let ext = ext.to_lowercase();
    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(ext)
    } else {
        None
    }
}

/// 从文件名主干猜一个可读描述。
///
/// `sunset_over_lake_01` → `sunset over lake 01`。中文文件名原样保留。
fn caption_from_stem(stem: &str) -> String {
    let cleaned: String = stem
        .chars()
        .map(|c| match c {
            '_' | '-' | '+' => ' ',
            other => other,
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        stem.to_string()
    } else {
        cleaned
    }
}

/// 从文件名主干切标签。全是数字的片段不当标签（那是序号，不是语义）。
fn tags_from_stem(stem: &str) -> Vec<String> {
    stem.split(['_', '-', '+', ' '])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.chars().all(|c| c.is_ascii_digit()))
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_and_tags_come_from_the_file_name() {
        let p = Photo::from_file("sunset_over_lake_01.png").unwrap();
        assert_eq!(p.id, "sunset_over_lake_01");
        assert_eq!(p.file, "sunset_over_lake_01.png");
        assert_eq!(p.caption, "sunset over lake 01");
        assert_eq!(p.tags, vec!["sunset", "over", "lake"]);
        assert_eq!(p.source, MediaSource::Library);
        assert!(p.render().contains("sunset_over_lake_01"));
    }

    #[test]
    fn chinese_file_names_survive() {
        let p = Photo::from_file("老照片.png").unwrap();
        assert_eq!(p.id, "老照片");
        assert_eq!(p.caption, "老照片");
        assert_eq!(p.tags, vec!["老照片"]);
    }

    #[test]
    fn rejects_junk_names() {
        assert!(Photo::from_file("").is_none());
        assert!(Photo::from_file("   ").is_none());
        assert!(Photo::from_file(".png").is_none());
        assert!(Photo::from_file(&"x".repeat(MAX_FILE_NAME + 1)).is_none());
    }

    #[test]
    fn extension_helpers_are_case_insensitive() {
        assert!(looks_like_image("a.PNG"));
        assert!(looks_like_image("a.jpeg"));
        assert!(!looks_like_image("a.txt"));
        assert!(!looks_like_image("noext"));
        assert_eq!(image_extension("a.JPG").as_deref(), Some("jpg"));
        assert!(image_extension("a.exe").is_none());
    }

    #[test]
    fn from_files_sorts_and_dedupes() {
        let lib = MediaLibrary::from_files(&["b_02.png", "a_01.png", "a_01.jpg"]);
        let photos = lib.photos();
        assert_eq!(photos.len(), 2);
        assert_eq!(photos[0].id, "a_01");
        assert_eq!(photos[1].id, "b_02");
    }

    #[test]
    fn lookup_accepts_id_file_caption_and_tag() {
        let lib = MediaLibrary::from_files(&["sunset_lake_01.png"]);
        assert!(lib.find("sunset_lake_01").is_some());
        assert!(lib.find("sunset_lake_01.png").is_some());
        assert!(lib.find("sunset lake 01").is_some());
        assert!(lib.find("SUNSET").is_some(), "标签应大小写不敏感");
        assert!(lib.find("nope").is_none());
        assert!(lib.find("  ").is_none());
    }

    #[test]
    fn has_file_is_a_whitelist() {
        let lib = MediaLibrary::from_files(&["a.png"]);
        assert!(lib.has_file("a.png"));
        assert!(!lib.has_file("b.png"));
        assert!(!lib.has_file("../Cargo.toml"));
        assert!(!lib.has_file(""));
    }

    #[test]
    fn register_adds_then_updates_without_duplicating() {
        let lib = MediaLibrary::new();
        let uploaded = Photo::from_file("shot.png")
            .unwrap()
            .with_source(MediaSource::Upload)
            .with_caption("用户发来的截图");
        lib.register(uploaded).unwrap();
        assert_eq!(lib.len(), 1);

        // 再登记一次：更新描述，但不能多出一行
        let again = Photo::from_file("shot.png").unwrap();
        lib.register(again).unwrap();
        assert_eq!(lib.len(), 1);
        let p = lib.find("shot").unwrap();
        assert_eq!(
            p.source,
            MediaSource::Upload,
            "重新登记不该把上传来源改回收藏"
        );
    }

    #[test]
    fn register_respects_the_cap() {
        let lib = MediaLibrary::new();
        for i in 0..MAX_PHOTOS {
            lib.register(Photo::from_file(&format!("p{i:04}.png")).unwrap())
                .unwrap();
        }
        assert_eq!(lib.len(), MAX_PHOTOS);
        let over = lib.register(Photo::from_file("one_more.png").unwrap());
        assert!(over.is_err());
        assert_eq!(lib.len(), MAX_PHOTOS);
    }

    #[test]
    fn empty_library_stays_silent_in_prompts() {
        let lib = MediaLibrary::new();
        assert!(lib.render_catalog(10).is_empty());
        assert!(lib.format_hint().is_empty());
        assert!(lib.describe_incoming("陈默", "x").is_none());
    }

    #[test]
    fn catalog_truncation_says_so() {
        let files: Vec<String> = (0..12).map(|i| format!("pic_{i:02}.png")).collect();
        let lib = MediaLibrary::from_files(&files);
        let text = lib.render_catalog(5);
        assert_eq!(text.matches("- pic_").count(), 5);
        assert!(text.contains("还有 7 张未列出"));
        assert!(lib.format_hint().contains("[图片]"));
    }

    #[test]
    fn describe_incoming_mentions_caption_and_facts() {
        let lib = MediaLibrary::new();
        lib.register(
            Photo::from_file("old_photo.png")
                .unwrap()
                .with_caption("一张旧照片")
                .with_facts("偏暖的黑白颗粒感，四角有折痕"),
        )
        .unwrap();
        let line = lib.describe_incoming("陈默", "old_photo").unwrap();
        assert!(line.contains("陈默给你看了一张图片"));
        assert!(line.contains("一张旧照片"));
        assert!(line.contains("折痕"));
    }

    #[test]
    fn load_dir_tolerates_a_missing_directory() {
        let lib = MediaLibrary::load_dir(Path::new("definitely/not/here"));
        assert!(lib.is_empty());
        assert_eq!(lib.dir().unwrap(), Path::new("definitely/not/here"));
    }
}
