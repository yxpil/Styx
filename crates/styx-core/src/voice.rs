//! # 语音端口
//!
//! 和记忆、联想一样，语音也是**端口而非实现**：内核只说"把这句话念出来"、
//! "这段录音里说了什么"，具体是 MOSS-TTS-Nano 还是别的引擎由上层注入。
//!
//! 这样做的直接好处是**离线可测**：`--no-voice` 时注入一个"假装念了"的
//! 假端口，整条回合链路照跑，前端照拿到降级提示，不必为了跑测试去装几 GB
//! 的模型。
//!
//! ## 为什么音频用 `Vec<u8>` 而不是流
//!
//! 角色的台词都是短句（几十个字），一次合成的 WAV 通常几十到几百 KB。
//! 引入流式接口会让 trait 变成 async 或要求 `Read`/`Write` 生命周期标注，
//! 而收益（省下几百毫秒的首字节延迟）在本地单人场景里几乎察觉不到。

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// 一个可用的音色。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceInfo {
    /// 音色标识（请求时原样回传）。
    pub id: String,
    /// 展示名。
    pub label: String,
    /// 语言，例如 `zh` / `en`。
    #[serde(default)]
    pub language: String,
    /// 参考音频路径（语音克隆用），可为空表示引擎内置音色。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

impl VoiceInfo {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        VoiceInfo {
            id: id.into(),
            label: label.into(),
            language: String::new(),
            reference: None,
        }
    }

    pub fn with_language(mut self, lang: impl Into<String>) -> Self {
        self.language = lang.into();
        self
    }

    pub fn with_reference(mut self, path: impl Into<String>) -> Self {
        self.reference = Some(path.into());
        self
    }
}

/// 一次合成请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechRequest {
    /// 要念的文本。
    pub text: String,
    /// 音色标识；为空表示用引擎默认音色。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// 语速倍率，1.0 为原速。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
}

impl SpeechRequest {
    pub fn new(text: impl Into<String>) -> Self {
        SpeechRequest {
            text: text.into(),
            voice: None,
            speed: None,
        }
    }

    pub fn with_voice(mut self, voice: impl Into<String>) -> Self {
        self.voice = Some(voice.into());
        self
    }

    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = Some(speed);
        self
    }

    /// 语速（夹到合理区间）。
    pub fn speed_or_default(&self) -> f32 {
        self.speed.unwrap_or(1.0).clamp(0.5, 2.0)
    }
}

/// 一段语音的识别结果。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// 识别出的文字。
    pub text: String,
    /// 语言（引擎可能不报）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 音频时长（毫秒），引擎不报则为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 置信度 0~1。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

impl Transcript {
    pub fn new(text: impl Into<String>) -> Self {
        Transcript {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// 语音合成端口。
pub trait SpeechSynthesizer: Send + Sync {
    /// 实现名（日志用）。
    fn name(&self) -> &str;

    /// 引擎名（`moss-tts-nano` / `stub` …），前端据此显示"谁在说话"。
    fn engine(&self) -> &str {
        self.name()
    }

    /// 是否可用。返回 false 时上层应给用户一条降级提示而不是报错。
    fn health(&self) -> bool;

    /// 可用音色。
    fn voices(&self) -> Vec<VoiceInfo> {
        Vec::new()
    }

    /// 合成一段 WAV（返回完整文件字节）。
    fn synthesize(&self, req: &SpeechRequest) -> Result<Vec<u8>>;

    /// 人类可读状态行。
    fn status(&self) -> String {
        self.name().to_string()
    }
}

/// 语音识别端口。
pub trait Transcriber: Send + Sync {
    fn name(&self) -> &str;

    fn engine(&self) -> &str {
        self.name()
    }

    fn health(&self) -> bool;

    /// 识别一段音频。`hint` 可以给语言或场景提示。
    fn transcribe(&self, audio: &[u8], hint: Option<&str>) -> Result<Transcript>;

    fn status(&self) -> String {
        self.name().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTts;
    impl SpeechSynthesizer for FakeTts {
        fn name(&self) -> &str {
            "fake-tts"
        }
        fn engine(&self) -> &str {
            "stub"
        }
        fn health(&self) -> bool {
            true
        }
        fn synthesize(&self, req: &SpeechRequest) -> Result<Vec<u8>> {
            Ok(req.text.as_bytes().to_vec())
        }
    }

    #[test]
    fn traits_are_object_safe() {
        let tts: Box<dyn SpeechSynthesizer> = Box::new(FakeTts);
        let asr: &dyn Transcriber = &FakeAsr;
        assert_eq!(tts.name(), "fake-tts");
        assert_eq!(asr.engine(), "stub");
        assert!(tts.health());
    }

    struct FakeAsr;
    impl Transcriber for FakeAsr {
        fn name(&self) -> &str {
            "fake-asr"
        }
        fn engine(&self) -> &str {
            "stub"
        }
        fn health(&self) -> bool {
            true
        }
        fn transcribe(&self, audio: &[u8], _hint: Option<&str>) -> Result<Transcript> {
            Ok(Transcript::new(format!("{} 字节", audio.len())))
        }
    }

    #[test]
    fn speech_request_clamps_speed() {
        let slow = SpeechRequest::new("你好").with_speed(0.01);
        assert!((slow.speed_or_default() - 0.5).abs() < 1e-6);
        let fast = SpeechRequest::new("你好").with_speed(9.0);
        assert!((fast.speed_or_default() - 2.0).abs() < 1e-6);
        assert!((SpeechRequest::new("你好").speed_or_default() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn transcript_emptiness_ignores_whitespace() {
        assert!(Transcript::new("   ").is_empty());
        assert!(!Transcript::new("嗯。").is_empty());
        assert!(Transcript::default().is_empty());
    }

    #[test]
    fn voice_info_builder() {
        let v = VoiceInfo::new("linxia", "林夏").with_language("zh");
        assert_eq!(v.id, "linxia");
        assert_eq!(v.language, "zh");
        assert!(v.reference.is_none());
    }
}
