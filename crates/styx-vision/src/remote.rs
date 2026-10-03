//! # 可选的远程视觉后端
//!
//! 两种后端，覆盖两种真实的部署方式：
//!
//! | 后端 | 对端 | 用途 |
//! |---|---|---|
//! | [`RemoteVision`] | 本仓库的 `services/vision` sidecar | YOLOv8n **ONNX** 目标检测（CPU 即可，模型 12MB） |
//! | [`OpenAiVision`] | ollama / vLLM / one-api 等任意 OpenAI 兼容端点 | 真正的语义描述（"画面里是一间堆满旧书的房间"） |
//!
//! ## 它们都**不是**必需品
//!
//! 两个后端都有两个共同性质，这是刻意的设计约束：
//!
//! 1. [`VisionBackend::health`] 是**廉价的**（一次带缓存的短超时探测），
//!    上层可以在每张图上都问一遍而不心疼；
//! 2. 失败只产生一条 `note`，[`crate::analyze`] 的本地事实照常进提示词。
//!
//! 换句话说：**删掉这两个文件，系统只会少说几句话，不会少一个功能。**

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use styx_http::{default_transport, HttpRequest, HttpTransport};

use crate::backend::{Detection, VisionBackend};
use crate::base64;
use crate::facts::ImageFacts;
use crate::tags::TagHit;
use crate::{Result, VisionError};

/// 健康探测结果缓存多久。
///
/// 为什么需要缓存：一张图要问一次 `health()`，而连拍十几张时等于把探测
/// 请求放大十几倍。10 秒既能让"刚把 sidecar 起起来"很快被感知到，
/// 又不至于把探测打成主要流量。
const HEALTH_TTL: Duration = Duration::from_secs(10);

/// 默认请求超时。
///
/// 检测比语言模型快得多（CPU 上 yolov8n 一张图几十毫秒），
/// 给 8 秒已经非常宽松；而回合等待是有体感的，不能在这里挂太久。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);

/// 一次带缓存的健康检查。
struct HealthCache {
    state: Mutex<Option<(Instant, bool)>>,
}

impl HealthCache {
    fn new() -> Self {
        HealthCache {
            state: Mutex::new(None),
        }
    }

    /// 取缓存值；过期或不存在就调用 `probe`。
    fn get(&self, probe: impl FnOnce() -> bool) -> bool {
        if let Ok(guard) = self.state.lock() {
            if let Some((at, ok)) = *guard {
                if at.elapsed() < HEALTH_TTL {
                    return ok;
                }
            }
        }
        let ok = probe();
        if let Ok(mut guard) = self.state.lock() {
            *guard = Some((Instant::now(), ok));
        }
        ok
    }
}

/// 指向本仓库 `services/vision` sidecar 的目标检测后端。
///
/// ## 协议
///
/// ```text
/// GET  {base}/health
///      → {"ok": true, "detector": "yolov8n", "classes": 80}
///
/// POST {base}/detect
///      {"image": "<base64>"}
///      → {"objects": [{"label":"person","label_zh":"人",
///                      "confidence":0.87,"box":[0.1,0.2,0.5,0.9]}],
///         "caption": "可选的一句描述"}
/// ```
///
/// `box` 是**归一化**坐标（0..1，左上原点）。用归一化而不是像素坐标，
/// 是因为归一化坐标在描述"在左上角"这类位置时不需要知道原图尺寸，
/// 而且天然避免了"不同后端用不同坐标约定"的坑。
pub struct RemoteVision {
    base: String,
    transport: Arc<dyn HttpTransport>,
    timeout: Duration,
    health: HealthCache,
    /// 检出阈值：低于它的直接丢掉。
    threshold: f32,
}

impl std::fmt::Debug for RemoteVision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteVision")
            .field("base", &self.base)
            .field("threshold", &self.threshold)
            .finish()
    }
}

impl RemoteVision {
    /// 用默认传输与超时构造。
    pub fn new(base: impl Into<String>) -> Self {
        RemoteVision {
            base: base.into().trim_end_matches('/').to_string(),
            transport: default_transport(),
            timeout: DEFAULT_TIMEOUT,
            health: HealthCache::new(),
            threshold: 0.35,
        }
    }

    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_threshold(mut self, threshold: f32) -> Self {
        self.threshold = threshold.clamp(0.0, 1.0);
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// 真的探测一次（不走缓存）。
    fn probe(&self) -> bool {
        let req = HttpRequest::get(self.url("/health")).with_timeout(Duration::from_millis(1200));
        match self.transport.send(&req) {
            Ok(resp) if resp.is_success() => {
                // 对端说 ok 才信；否则"服务起了但模型没加载"会被误判成可用
                resp.json()
                    .ok()
                    .and_then(|v| v.get("ok").and_then(|x| x.as_bool()))
                    .unwrap_or(true)
            }
            _ => false,
        }
    }
}

impl VisionBackend for RemoteVision {
    fn name(&self) -> &str {
        "detector"
    }

    fn health(&self) -> bool {
        self.health.get(|| self.probe())
    }

    fn describe(&self, image: &[u8], _facts: &ImageFacts) -> Result<Option<String>> {
        // 检测后端不再额外给 caption：它已经在 detect() 里把信息给全了，
        // 这里再返回一句话只会让描述里出现重复内容。
        // 但如果对端确实给了 caption（比如 sidecar 后面还挂了个 VLM），
        // 那就要带出来。
        let body = detect_body(image);
        let req = HttpRequest::post_json(self.url("/detect"), body).with_timeout(self.timeout);
        let resp = self.transport.send(&req)?;
        if !resp.is_success() {
            return Err(VisionError::Transport(format!(
                "sidecar 返回 {}",
                resp.status
            )));
        }
        let json = resp.json()?;
        let caption = json
            .get("caption")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let _ = image;
        Ok(caption)
    }

    fn detect(&self, image: &[u8]) -> Result<Vec<Detection>> {
        let req = HttpRequest::post_json(self.url("/detect"), detect_body(image))
            .with_timeout(self.timeout);
        let resp = self.transport.send(&req)?;
        if !resp.is_success() {
            return Err(VisionError::Transport(format!(
                "sidecar 返回 {}",
                resp.status
            )));
        }
        let json = resp.json()?;
        let mut out = parse_objects(&json);
        out.retain(|d| d.confidence >= self.threshold);
        Ok(out)
    }
}

fn detect_body(image: &[u8]) -> String {
    // 手拼而不是 serde_json::to_string：这里只有两个字段，
    // 而 base64 的输出字符集不含需要转义的字符，直接拼是安全的。
    format!(r#"{{"image":"{}"}}"#, base64::encode(image))
}

/// 解析 sidecar 返回的目标列表。
///
/// 刻意做得**很宽容**：接受顶层数组、`{"objects":[...]}`、以及
/// `{"detections":[...]}` 三种写法，字段名也接受几种常见别名。
/// 理由是这类 sidecar 经常被换掉（换成 ultralytics 的官方 server、
/// 换成某个现成的 YOLO HTTP 服务），苛刻的解析只会让人平白多写适配层。
pub fn parse_objects(json: &serde_json::Value) -> Vec<Detection> {
    let list = json
        .get("objects")
        .or_else(|| json.get("detections"))
        .or_else(|| json.get("results"))
        .unwrap_or(json);
    let items = match list {
        serde_json::Value::Array(items) => items,
        _ => return Vec::new(),
    };

    let mut out = Vec::new();
    for item in items {
        let obj = match item.as_object() {
            Some(o) => o,
            None => continue,
        };
        let label = obj
            .get("label")
            .or_else(|| obj.get("name"))
            .or_else(|| obj.get("class"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if label.trim().is_empty() {
            continue;
        }
        let label_zh = obj
            .get("label_zh")
            .or_else(|| obj.get("label_cn"))
            .or_else(|| obj.get("zh"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let confidence = obj
            .get("confidence")
            .or_else(|| obj.get("score"))
            .or_else(|| obj.get("conf"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as f32;

        let raw_box: Vec<f32> = obj
            .get("box")
            .or_else(|| obj.get("bbox"))
            .or_else(|| obj.get("xyxy"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_f64())
                    .map(|x| x as f32)
                    .collect()
            })
            .unwrap_or_default();

        // 像素坐标也能接受：超过 1 就当成像素值并按对端给的尺寸归一化
        let box_norm = normalize_box(&raw_box, obj, json);

        let area = if box_norm.len() == 4 {
            ((box_norm[2] - box_norm[0]).max(0.0) * (box_norm[3] - box_norm[1]).max(0.0))
                .clamp(0.0, 1.0)
        } else {
            0.0
        };

        out.push(Detection {
            label: label.trim().to_string(),
            label_zh,
            confidence: confidence.clamp(0.0, 1.0),
            r#box: box_norm,
            area,
        });
    }
    out
}

fn normalize_box(
    raw: &[f32],
    obj: &serde_json::Map<String, serde_json::Value>,
    root: &serde_json::Value,
) -> Vec<f32> {
    if raw.len() != 4 {
        return Vec::new();
    }
    let max = raw.iter().cloned().fold(0.0f32, f32::max);
    if max <= 1.5 {
        return raw.iter().map(|v| v.clamp(0.0, 1.0)).collect();
    }
    // 像素坐标：从对象或根节点取尺寸
    let dim = |key: &str| -> Option<f32> {
        obj.get(key)
            .or_else(|| root.get(key))
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
    };
    let (w, h) = match (dim("width"), dim("height")) {
        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w, h),
        _ => return Vec::new(),
    };
    vec![
        (raw[0] / w).clamp(0.0, 1.0),
        (raw[1] / h).clamp(0.0, 1.0),
        (raw[2] / w).clamp(0.0, 1.0),
        (raw[3] / h).clamp(0.0, 1.0),
    ]
}

// ------------------------------------------------------- 属性反推（提示词反推）

/// 默认最多要多少个标签。
///
/// 40 是"够用且不浪费"的取值：WD14 的词表有上万条，一张图能命中几百个
/// 0.1 分以上的标签；真正的信息集中在前二三十个里，后面全是噪声。
/// 最终交给模型的数量由 [`crate::tags::prune`] 再压到十几个。
const DEFAULT_TAG_LIMIT: usize = 40;

/// 指向 `services/vision` sidecar 的**属性反推**后端。
///
/// 这就是 Stable Diffusion 生态里说的"提示词反推"：把一张图变成一串
/// `1girl, solo, long_hair, blue_eyes, school_uniform, smile, indoors, anime`。
/// 对角色扮演的价值比目标检测高——它给的是画面**属性**，而不是"有什么东西"。
///
/// ## 协议
///
/// ```text
/// GET  {base}/health
///      → {"ok": true, "tagger": "wd14", "vocabulary": "danbooru"}
///
/// POST {base}/tag
///      {"image": "<base64>", "limit": 40}
///      → {"tags": [{"tag":"long_hair","tag_zh":"长发","score":0.91}],
///         "model": "wd14-v1-4"}          ← 可选，会作为标签来源记下来
/// ```
///
/// ## 为什么要把 `model` 记下来
///
/// 因为反推模型的**词表决定了它的偏见**。WD14 是在 Danbooru 上训的，
/// 给它一张真人照片，它会斩钉截铁地说 `1girl`。把模型名带进描述里，
/// 语言模型才有机会知道"这批标签偏二次元，别当真"。少这一句，
/// 一个业余模型就能让角色把用户的照片认错还不自知。
pub struct RemoteTagger {
    base: String,
    transport: Arc<dyn HttpTransport>,
    timeout: Duration,
    health: HealthCache,
    /// 低于它的标签直接丢掉（在 [`VisionBackend::tag`] 里就丢掉，
    /// 不留给上层——省得上层还要再判一次）。
    min_score: f32,
    limit: usize,
    /// 对端自报的模型名；`None` 时用 `"tagger"`。
    source: Option<String>,
}

impl std::fmt::Debug for RemoteTagger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteTagger")
            .field("base", &self.base)
            .field("min_score", &self.min_score)
            .field("source", &self.source)
            .finish()
    }
}

impl RemoteTagger {
    pub fn new(base: impl Into<String>) -> Self {
        RemoteTagger {
            base: base.into().trim_end_matches('/').to_string(),
            transport: default_transport(),
            timeout: DEFAULT_TIMEOUT,
            health: HealthCache::new(),
            min_score: crate::tags::DEFAULT_MIN_SCORE,
            limit: DEFAULT_TAG_LIMIT,
            source: None,
        }
    }

    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_min_score(mut self, min_score: f32) -> Self {
        self.min_score = min_score.clamp(0.0, 1.0);
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.clamp(1, 500);
        self
    }

    /// 手动指定来源名（对端不报 `model` 时用）。
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn min_score(&self) -> f32 {
        self.min_score
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// 请求体单独暴露便于测试。
    pub fn request_body(&self, image: &[u8]) -> String {
        format!(
            r#"{{"image":"{}","limit":{}}}"#,
            base64::encode(image),
            self.limit
        )
    }

    fn probe(&self) -> bool {
        let req = HttpRequest::get(self.url("/health")).with_timeout(Duration::from_millis(1200));
        match self.transport.send(&req) {
            Ok(resp) if resp.is_success() => resp
                .json()
                .ok()
                .and_then(|v| v.get("ok").and_then(|x| x.as_bool()))
                .unwrap_or(true),
            _ => false,
        }
    }
}

impl VisionBackend for RemoteTagger {
    fn name(&self) -> &str {
        "tagger"
    }

    fn health(&self) -> bool {
        self.health.get(|| self.probe())
    }

    /// 反推后端不产出自然语言：它的话都在 `tag()` 里了，这里再返回一句
    /// 只会让描述里出现两遍同样的内容。
    fn describe(&self, _image: &[u8], _facts: &ImageFacts) -> Result<Option<String>> {
        Ok(None)
    }

    fn tag(&self, image: &[u8]) -> Result<Vec<TagHit>> {
        let req = HttpRequest::post_json(self.url("/tag"), self.request_body(image))
            .with_timeout(self.timeout);
        let resp = self.transport.send(&req)?;
        if !resp.is_success() {
            return Err(VisionError::Transport(format!(
                "tagger 返回 {}",
                resp.status
            )));
        }
        let json = resp.json()?;
        // 对端如果报了模型名，就把它当来源记进每个标签里——
        // 这是"这批标签可不可信"的唯一线索。
        let source = json
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| self.source.clone())
            .unwrap_or_else(|| "tagger".to_string());
        let mut out = parse_tags(&json);
        for t in &mut out {
            if t.source.is_empty() {
                t.source = source.clone();
            }
        }
        out.retain(|t| t.score >= self.min_score);
        Ok(out)
    }
}

/// 解析 sidecar 返回的标签列表。
///
/// 宽容程度和 [`parse_objects`] 一致，但要额外容忍两种**字典**写法——
/// 它们是这类模型最常见的原生输出格式：
///
/// ```json
/// {"tags": {"1girl": 0.99, "solo": 0.97}}
/// {"labels": ["1girl", "solo"]}
/// ```
///
/// 名字的别名也照收：`tag` / `label` / `name` / `class`，
/// `score` / `confidence` / `prob` / `probability` / `value`。
pub fn parse_tags(json: &serde_json::Value) -> Vec<TagHit> {
    let list = json
        .get("tags")
        .or_else(|| json.get("labels"))
        .or_else(|| json.get("predictions"))
        .or_else(|| json.get("results"))
        .unwrap_or(json);

    let mut out = Vec::new();

    match list {
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(t) = tag_from_value(item) {
                    out.push(t);
                }
            }
        }
        // {"1girl": 0.99, "solo": 0.97}
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if let Some(score) = value.as_f64() {
                    out.push(normalize_tag(key, None, score as f32));
                }
            }
        }
        _ => {}
    }
    out
}

/// 从单个数组元素里取出一个标签。
fn tag_from_value(item: &serde_json::Value) -> Option<TagHit> {
    match item {
        // 白名单：`{"labels":["1girl","solo"]}` 这种没有分数的写法，
        // 当成满分处理——至少比丢掉强。
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(normalize_tag(s, None, 1.0))
            }
        }
        serde_json::Value::Object(obj) => {
            let name = pick_str(obj, &["tag", "label", "name", "class"])?;
            let zh = pick_str(obj, &["tag_zh", "label_zh", "zh", "name_zh"]);
            let score = pick_f32(
                obj,
                &["score", "confidence", "prob", "probability", "value"],
            )
            .unwrap_or(1.0);
            Some(normalize_tag(&name, zh.as_deref(), score))
        }
        _ => None,
    }
}

/// 统一成一个 [`TagHit`]。
///
/// 关键的一步是把空格换成下划线：`"long hair"` 与 `"long_hair"` 是同一个
/// 标签，不统一的话两个后端各给一遍就会在描述里出现两次。
fn normalize_tag(tag: &str, zh: Option<&str>, score: f32) -> TagHit {
    let tag = tag.trim().replace(' ', "_");
    let mut hit = TagHit::new(tag, score);
    if let Some(zh) = zh {
        hit = hit.with_zh(zh.trim());
    }
    hit
}

fn pick_str(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = obj.get(*k).and_then(|v| v.as_str()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn pick_f32(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<f32> {
    for k in keys {
        if let Some(v) = obj.get(*k).and_then(|v| v.as_f64()) {
            return Some((v as f32).clamp(0.0, 1.0));
        }
    }
    None
}

// ------------------------------------------------------------ OpenAI 兼容视觉

/// 通过 OpenAI 兼容端点做图片理解（ollama / vLLM / one-api …）。
///
/// 走的是标准的多模态 `content` 数组：
/// ```json
/// {"messages":[{"role":"user","content":[
///    {"type":"text","text":"..."},
///    {"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}]}]}
/// ```
/// 这条格式 ollama 与所有 OpenAI 兼容网关都认，所以不需要为每个后端写适配。
pub struct OpenAiVision {
    base: String,
    model: String,
    prompt: String,
    transport: Arc<dyn HttpTransport>,
    timeout: Duration,
    health: HealthCache,
    api_key: Option<String>,
}

impl std::fmt::Debug for OpenAiVision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiVision")
            .field("base", &self.base)
            .field("model", &self.model)
            .finish()
    }
}

/// 默认提示词。
///
/// 措辞上有两条硬要求：**只描述看得见的**、**不确定就说不确定**。
/// 视觉模型最常见的失败方式不是"看不见"，而是"看见了一点就编一整套"，
/// 而角色扮演会把这个编造当成事实继续演下去。
pub const DEFAULT_VISION_PROMPT: &str = "用两三句中文描述这张图片：画面里有什么、\
在什么环境、整体氛围如何。只描述你确实看到的，不确定的地方就说不确定，\
不要推测拍摄者、地点或故事。不要使用列表或 Markdown。";

impl OpenAiVision {
    /// `base` 例如 `http://127.0.0.1:11434/v1`。
    pub fn new(base: impl Into<String>, model: impl Into<String>) -> Self {
        OpenAiVision {
            base: base.into().trim_end_matches('/').to_string(),
            model: model.into(),
            prompt: DEFAULT_VISION_PROMPT.to_string(),
            transport: default_transport(),
            timeout: Duration::from_secs(60),
            health: HealthCache::new(),
            api_key: None,
        }
    }

    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// 构造请求体（单独暴露便于测试）。
    pub fn request_body(&self, image: &[u8]) -> String {
        let url = base64::data_url(base64::mime_of(image), image);
        // prompt 与 base64 都不含需要转义的字符（prompt 是我们自己的常量或用户配置，
        // data URL 的字符集是 [A-Za-z0-9+/=:;,.])，所以直接拼 JSON 是安全的。
        format!(
            r#"{{"model":"{}","messages":[{{"role":"user","content":[{{"type":"text","text":"{}"}},{{"type":"image_url","image_url":{{"url":"{}"}}}}]}}],"max_tokens":300}}"#,
            self.model, self.prompt, url
        )
    }
}

impl VisionBackend for OpenAiVision {
    fn name(&self) -> &str {
        "vlm"
    }

    fn health(&self) -> bool {
        self.health.get(|| {
            let req = HttpRequest::get(format!("{}/models", self.base))
                .with_timeout(Duration::from_millis(1500));
            let req = match &self.api_key {
                Some(k) => req.with_bearer(k),
                None => req,
            };
            matches!(self.transport.send(&req), Ok(r) if r.is_success())
        })
    }

    fn describe(&self, image: &[u8], _facts: &ImageFacts) -> Result<Option<String>> {
        let req = HttpRequest::post_json(
            format!("{}/chat/completions", self.base),
            self.request_body(image),
        )
        .with_timeout(self.timeout);
        let req = match &self.api_key {
            Some(k) => req.with_bearer(k),
            None => req,
        };
        let resp = self.transport.send(&req)?;
        if !resp.is_success() {
            return Err(VisionError::Transport(format!(
                "视觉模型返回 {}",
                resp.status
            )));
        }
        let json = resp.json()?;
        let text = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use styx_http::{HttpRequest as Req, HttpResponse, HttpTransport};

    struct NeverHttp;
    impl HttpTransport for NeverHttp {
        fn name(&self) -> &str {
            "never"
        }
        fn send(&self, _r: &Req) -> styx_http::Result<HttpResponse> {
            Err(styx_http::HttpError::Io("connexion refused".into()))
        }
    }

    struct AlwaysJson(String);
    impl HttpTransport for AlwaysJson {
        fn name(&self) -> &str {
            "always"
        }
        fn send(&self, _r: &Req) -> styx_http::Result<HttpResponse> {
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: self.0.clone(),
            })
        }
    }

    struct EchoBody(Mutex<Vec<String>>);
    impl HttpTransport for EchoBody {
        fn name(&self) -> &str {
            "echo"
        }
        fn send(&self, r: &Req) -> styx_http::Result<HttpResponse> {
            self.0
                .lock()
                .unwrap()
                .push(r.body.clone().unwrap_or_default());
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: "{\"objects\":[]}".to_string(),
            })
        }
    }

    fn facts() -> ImageFacts {
        crate::analyze(b"x")
    }

    #[test]
    fn a_dead_sidecar_is_reported_as_unhealthy_and_errors_are_soft() {
        let v = RemoteVision::new("http://127.0.0.1:9").with_transport(Arc::new(NeverHttp));
        assert!(!v.health());
        assert!(v.detect(b"img").is_err());
        assert_eq!(v.name(), "detector");
    }

    #[test]
    fn health_is_cached_so_repeated_checks_do_not_storm_the_backend() {
        // 用一个"第一次成功、之后失败"的传输来验证缓存确实生效
        struct Flaky(std::sync::atomic::AtomicUsize);
        impl HttpTransport for Flaky {
            fn name(&self) -> &str {
                "flaky"
            }
            fn send(&self, _r: &Req) -> styx_http::Result<HttpResponse> {
                let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Ok(HttpResponse {
                        status: 200,
                        headers: Default::default(),
                        body: "{\"ok\":true}".to_string(),
                    })
                } else {
                    Err(styx_http::HttpError::Io("down".into()))
                }
            }
        }
        let t = Arc::new(Flaky(std::sync::atomic::AtomicUsize::new(0)));
        let v = RemoteVision::new("http://x").with_transport(t.clone());
        assert!(v.health(), "第一次探测成功");
        assert!(v.health(), "缓存必须复用，不该再发请求");
        assert!(v.health());
        assert_eq!(t.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn health_false_when_the_backend_says_not_ready() {
        let v = RemoteVision::new("http://x")
            .with_transport(Arc::new(AlwaysJson(r#"{"ok":false}"#.into())));
        assert!(!v.health(), "服务起了但模型没加载，不该算可用");
    }

    #[test]
    fn parses_normalized_boxes() {
        let json = json!({
            "objects": [
                {"label": "person", "label_zh": "人", "confidence": 0.91,
                 "box": [0.1, 0.2, 0.5, 0.9]},
                {"label": "book", "confidence": 0.4, "box": [0.6, 0.6, 0.8, 0.8]}
            ]
        });
        let found = parse_objects(&json);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].label, "person");
        assert_eq!(found[0].display_name(), "人");
        assert!((found[0].confidence - 0.91).abs() < 1e-3);
        assert!((found[0].area - 0.28).abs() < 1e-3, "{:?}", found[0].area);
        // 框的纵向中心是 0.55，属于画面中部，所以只能说"左侧"而不是"左上"
        assert_eq!(found[0].position_label(), "左侧");
    }

    #[test]
    fn parses_pixel_boxes_when_dimensions_are_given() {
        let json = json!({
            "width": 1000, "height": 500,
            "objects": [{"label": "cat", "score": 0.8, "bbox": [100, 50, 500, 250]}]
        });
        let found = parse_objects(&json);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].r#box, vec![0.1, 0.1, 0.5, 0.5]);
    }

    #[test]
    fn parser_accepts_several_shapes_and_skips_junk() {
        // 顶层就是数组
        let flat = json!([{"name": "person", "conf": 0.7, "xyxy": [0.0, 0.0, 1.0, 1.0]}]);
        assert_eq!(parse_objects(&flat).len(), 1);

        // detections 字段
        let alt = json!({"detections": [{"class": "dog", "confidence": 0.5}]});
        assert_eq!(parse_objects(&alt).len(), 1);

        // 缺标签、缺坐标的条目要被跳过而不是造出空条目
        let junk = json!({"objects": [
            {"confidence": 0.9},
            {"label": "  "},
            {"label": "ok", "confidence": 0.5}
        ]});
        let found = parse_objects(&junk);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "ok");
        assert!(found[0].r#box.is_empty());
        assert_eq!(found[0].area, 0.0);
    }

    #[test]
    fn threshold_filters_weak_detections() {
        let v = RemoteVision::new("http://x")
            .with_transport(Arc::new(AlwaysJson(
                r#"{"objects":[{"label":"a","confidence":0.9},{"label":"b","confidence":0.1}]}"#
                    .into(),
            )))
            .with_threshold(0.5);
        let found = v.detect(b"img").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "a");
    }

    #[test]
    fn detect_body_carries_the_base64_image() {
        let sink = Arc::new(EchoBody(Mutex::new(Vec::new())));
        let v = RemoteVision::new("http://x").with_transport(sink.clone());
        v.detect(&[0xff, 0xd8, 0xff]).unwrap();
        let sent = sink.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains(r#""image":"/9j/"#), "{}", sent[0]);
    }

    #[test]
    fn caption_is_passed_through_when_present() {
        let v = RemoteVision::new("http://x").with_transport(Arc::new(AlwaysJson(
            r#"{"objects":[],"caption":"一间堆满旧书的房间"}"#.into(),
        )));
        let got = v.describe(b"img", &facts()).unwrap();
        assert_eq!(got.as_deref(), Some("一间堆满旧书的房间"));
        // 没有 caption 时返回 None（正常的沉默），而不是空字符串
        let empty = RemoteVision::new("http://x")
            .with_transport(Arc::new(AlwaysJson(r#"{"objects":[]}"#.into())));
        assert!(empty.describe(b"img", &facts()).unwrap().is_none());
    }

    #[test]
    fn vision_model_request_uses_the_multimodal_content_array() {
        let v = OpenAiVision::new("http://127.0.0.1:11434/v1", "qwen-vl");
        let body = v.request_body(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["model"], "qwen-vl");
        let content = &parsed["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"]
            .as_str()
            .unwrap()
            .contains("只描述你确实看到的"));
        assert_eq!(content[1]["type"], "image_url");
        assert!(content[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    #[test]
    fn vision_model_health_and_description() {
        let ok = OpenAiVision::new("http://x", "m").with_transport(Arc::new(AlwaysJson(
            r#"{"choices":[{"message":{"content":"  一间旧书店  "}}]}"#.into(),
        )));
        assert!(ok.health());
        assert_eq!(
            ok.describe(b"i", &facts()).unwrap().as_deref(),
            Some("一间旧书店")
        );

        let down = OpenAiVision::new("http://x", "m").with_transport(Arc::new(NeverHttp));
        assert!(!down.health());
        assert!(down.describe(b"i", &facts()).is_err());
    }

    #[test]
    fn an_empty_model_answer_is_not_a_caption() {
        let v = OpenAiVision::new("http://x", "m").with_transport(Arc::new(AlwaysJson(
            r#"{"choices":[{"message":{"content":"   "}}]}"#.into(),
        )));
        assert!(v.describe(b"i", &facts()).unwrap().is_none());
    }

    // ---------------------------------------------------------- 属性反推

    #[test]
    fn parses_the_object_array_shape() {
        let json = json!({
            "tags": [
                {"tag": "long_hair", "tag_zh": "长发", "score": 0.91},
                {"label": "solo", "confidence": 0.88}
            ]
        });
        let got = parse_tags(&json);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].tag, "long_hair");
        assert_eq!(got[0].tag_zh, "长发");
        assert!((got[0].score - 0.91).abs() < 1e-3);
        assert_eq!(got[1].tag, "solo", "label 也是认的别名");
    }

    #[test]
    fn parses_the_dictionary_shape() {
        // WD14 这类模型的**原生**输出就是一个 tag→score 的字典
        let json = json!({"tags": {"1girl": 0.99, "long_hair": 0.93, "smile": 0.61}});
        let got = parse_tags(&json);
        assert_eq!(got.len(), 3);
        assert!(got.iter().any(|t| t.tag == "1girl" && t.score > 0.98));
    }

    #[test]
    fn parses_a_bare_string_list_and_assumes_full_confidence() {
        let json = json!({"labels": ["1girl", "solo"]});
        let got = parse_tags(&json);
        assert_eq!(got.len(), 2);
        assert!((got[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_top_level_array_works_too() {
        let json = json!([{"tag": "cat", "score": 0.8}]);
        assert_eq!(parse_tags(&json).len(), 1);
    }

    #[test]
    fn spaces_and_underscores_collapse_into_one_tag() {
        // 两个后端各给一遍 "long hair" / "long_hair"，不该出现两次
        let json = json!({"tags": [{"tag": "long hair", "score": 0.9},
                                   {"tag": "long_hair", "score": 0.8}]});
        let got = parse_tags(&json);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].tag, "long_hair");
        assert_eq!(got[1].tag, "long_hair");
        let mut merged = got;
        crate::tags::prune(&mut merged, 0.0);
        assert_eq!(merged.len(), 1);
        assert!((merged[0].score - 0.9).abs() < 1e-3, "留下的该是高分那个");
    }

    #[test]
    fn junk_entries_are_skipped_not_fatal() {
        let json = json!({"tags": [null, 42, "  ", {"nope": 1}, {"tag": "ok", "score": 0.5}]});
        let got = parse_tags(&json);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].tag, "ok");
    }

    #[test]
    fn the_tagger_records_which_model_produced_the_tags() {
        let t = RemoteTagger::new("http://x").with_transport(Arc::new(AlwaysJson(
            r#"{"tags":[{"tag":"1girl","score":0.9}],"model":"wd14-v1-4"}"#.into(),
        )));
        let got = t.tag(b"i").unwrap();
        assert_eq!(got[0].source, "wd14-v1-4");
        // 有了模型名，描述层才有机会提醒"这模型偏二次元"
        assert!(got[0].from_anime_tuned_source());
    }

    #[test]
    fn the_source_falls_back_to_the_configured_name() {
        let t = RemoteTagger::new("http://x")
            .with_transport(Arc::new(AlwaysJson(
                r#"{"tags":[{"tag":"cat","score":0.9}]}"#.into(),
            )))
            .with_source("my-tagger");
        assert_eq!(t.tag(b"i").unwrap()[0].source, "my-tagger");
        // 也没配的话至少留一个能看的名字
        let bare = RemoteTagger::new("http://x").with_transport(Arc::new(AlwaysJson(
            r#"{"tags":[{"tag":"cat","score":0.9}]}"#.into(),
        )));
        assert_eq!(bare.tag(b"i").unwrap()[0].source, "tagger");
    }

    #[test]
    fn weak_tags_are_filtered_at_the_backend() {
        let t = RemoteTagger::new("http://x")
            .with_transport(Arc::new(AlwaysJson(
                r#"{"tags":[{"tag":"strong","score":0.9},{"tag":"weak","score":0.1}]}"#.into(),
            )))
            .with_min_score(0.4);
        let got = t.tag(b"i").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].tag, "strong");
    }

    #[test]
    fn the_tagger_request_carries_the_image_and_a_limit() {
        let t = RemoteTagger::new("http://x").with_limit(17);
        let body = t.request_body(&[1, 2, 3]);
        assert!(body.contains(r#""limit":17"#), "{body}");
        assert!(body.contains("AQID"), "{body}");
    }

    #[test]
    fn a_dead_tagger_is_soft_and_says_nothing_in_prose() {
        let t = RemoteTagger::new("http://127.0.0.1:9").with_transport(Arc::new(NeverHttp));
        assert!(!t.health());
        assert!(t.tag(b"i").is_err());
        assert_eq!(t.name(), "tagger");
        // describe 永远返回"没什么可补充的"——它的话都在 tag() 里
        assert!(t.describe(b"i", &facts()).unwrap().is_none());
    }
}
