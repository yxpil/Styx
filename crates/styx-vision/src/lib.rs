//! # styx-vision — 本地图像理解
//!
//! 让**没有视觉能力的语言模型**也能"看见"图片。
//!
//! 这不是一个"降级方案"，而是这套系统里图片理解的主力实现：它没有任何
//! 第三方依赖、不需要 GPU、不需要网络，因此在任何机器上都一定会跑起来。
//! 可选的 YOLO 检测与视觉模型只是叠加在它上面的额外信息。
//!
//! ## 四层结构
//!
//! ```text
//!   ┌─────────────────────────────────────────────────────────────┐
//!   │ ①  本地事实   facts::analyze                                │
//!   │    手写 inflate + PNG/BMP/JPEG 解码 → 尺寸/明暗/冷暖/主色/疏密│
//!   │    → 中文描述。零依赖，永远可用。                            │
//!   ├─────────────────────────────────────────────────────────────┤
//!   │ ②  目标检测   backend::RemoteVision（YOLOv8n ONNX sidecar） │
//!   │    可选。给出"画面里有什么、在哪个位置、多大"。              │
//!   ├─────────────────────────────────────────────────────────────┤
//!   │ ③  属性反推   backend::RemoteTagger（WD14 / CLIP 打分）     │
//!   │    可选。给出"提示词反推"式的属性标签：发色/神情/穿着/画风。 │
//!   ├─────────────────────────────────────────────────────────────┤
//!   │ ④  视觉模型   backend::OpenAiVision（ollama / 兼容端点）     │
//!   │    可选。给出真正的语义描述。                                │
//!   └─────────────────────────────────────────────────────────────┘
//!                          ↓ 由 backend::Composer 合并
//!                    一段交给语言模型的自然语言描述
//! ```
//!
//! ②③④ 全都是**可选且可替换**的。这个顺序是有意的：越靠下越"聪明"，
//! 也越贵、越容易失效。删掉后三层，① 依然给出一段可用的描述。
//!
//! ## 格式支持
//!
//! | 格式 | 能力 | 原因 |
//! |---|---|---|
//! | PNG | 完整像素 | 无损 + zlib，手写 inflate 就能拿到真像素 |
//! | BMP | 完整像素 | 无压缩直存，几乎是白送的 |
//! | JPEG | 完整像素（基线顺序） | `jpeg` 模块：Huffman + 反量化 + 浮点 8×8 IDCT + IJG 三角滤波升采样 |
//! | 渐进式 JPEG | 仅尺寸 | 同一张图要走多趟扫描，实现成本高于收益 |
//! | 其它 | 仅尺寸（若认得） | — |
//!
//! JPEG 值得特别说一句：真实用户发来的照片几乎全是 JPEG，只报个尺寸等于
//! 「看不见」。所以这里自己写了解码器，用 10 组基线夹具逐像素与 libjpeg 比对，
//! 误差在解码器容差内（多数像素完全相同，最大偏差 ≤3，来自浮点 IDCT 与
//! libjpeg 整数 IDCT 的规范允许差异）。
//!
//! 剩下的缺口由**浏览器**补上：前端的 Canvas 能解任意格式，上传时会一并
//! 提交同一套统计量。两条路都拿不到像素时，[`ImageFacts::decoded`] 为
//! `false`，描述里会明说"本地没能解出像素，不要据此推断画面内容"——
//! 而不是让模型把"没有信息"误读成"画面很普通"。
//!
//! ## 快速上手
//!
//! ```ignore
//! use styx_vision::{analyze, Composer};
//!
//! // 只要本地事实（零依赖路径）
//! let facts = analyze(b"not really an image");
//! assert!(!facts.decoded);
//!
//! // 带可选后端时：远程挂了也一定返回可用描述
//! let composer = Composer::new();
//! let (_, text, _) = composer.describe(b"not really an image");
//! assert!(!text.is_empty());
//! ```
//!
//! 上面这段在 `tests::the_documented_quick_start_actually_runs` 里是真跑的
//! （本仓库的 doctest 在部分沙箱里生不出 rustc，所以示例同时留一份可执行副本）。

pub mod backend;
pub mod base64;
pub mod decode;
pub mod facts;
pub mod inflate;
pub mod jpeg;
#[cfg(feature = "http")]
pub mod remote;
pub mod tags;

pub use backend::{Composer, Detection, VisionBackend};
pub use decode::{decode, dimensions, sniff, Bitmap, DecodeError, Format, Rgb};
pub use facts::{analyze, analyze_bitmap, ImageFacts, MAX_SAMPLES};
#[cfg(feature = "http")]
pub use remote::{OpenAiVision, RemoteTagger, RemoteVision};
pub use tags::{TagGroup, TagHit};

/// 视觉层的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisionError {
    /// 网络层失败（后端没起、超时、连不上）。
    Transport(String),
    /// 后端返回了看不懂的东西。
    Protocol(String),
    /// 解码失败。
    Decode(String),
}

impl std::fmt::Display for VisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VisionError::Transport(m) => write!(f, "无法连接视觉后端：{m}"),
            VisionError::Protocol(m) => write!(f, "视觉后端返回了意外的内容：{m}"),
            VisionError::Decode(m) => write!(f, "解码失败：{m}"),
        }
    }
}

impl std::error::Error for VisionError {}

/// 视觉层结果。
pub type Result<T, E = VisionError> = std::result::Result<T, E>;

impl From<DecodeError> for VisionError {
    fn from(e: DecodeError) -> Self {
        VisionError::Decode(e.to_string())
    }
}

#[cfg(feature = "http")]
impl From<styx_http::HttpError> for VisionError {
    fn from(e: styx_http::HttpError) -> Self {
        VisionError::Transport(e.to_string())
    }
}

/// crate 版本。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    /// crate 文档里的"快速上手"示例的可执行副本。
    ///
    /// 为什么不用 doctest：本仓库的 doctest 在部分沙箱里生不出 rustc
    /// （`Os error 231`），于是示例永远不会真的被跑。留一份真测试更划算。
    #[test]
    fn the_documented_quick_start_actually_runs() {
        use crate::{analyze, Composer};

        // 只要本地事实（零依赖路径）
        let facts = analyze(b"not really an image");
        assert!(!facts.decoded);
        assert!(facts.describe().contains("损坏") || facts.describe().contains("认不出"));

        // 带可选后端时：远程挂了也一定返回可用描述
        let composer = Composer::new();
        let (_, text, _) = composer.describe(b"not really an image");
        assert!(!text.is_empty());
    }
}
