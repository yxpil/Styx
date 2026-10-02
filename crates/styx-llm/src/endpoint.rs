//! 端点配置：一个"能生成文本的地方"。
//!
//! Styx 把"模型"抽象成端点（endpoint）而不是"模型名"，因为现实里同一个模型
//! 往往有多个入口：官方 API、中转站、本机 Ollama、公司内网的 vLLM。
//! 端点还带有**调度属性**（权重、熔断阈值、冷却时间），
//! 这就是 [RLBCM](https://github.com/yxpil/RLBCM) 那类负载均衡模块在客户端侧的对应物。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 一个模型端点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// 端点的可读名字（出现在日志与回合报告里）。
    pub name: String,
    /// 基础地址，例如 `https://api.openai.com/v1` 或 `http://127.0.0.1:11434/v1`。
    pub base_url: String,
    /// API key（本机模型通常留空）。
    #[serde(default)]
    pub api_key: String,
    /// 默认模型名。
    pub model: String,
    /// 调度权重（越大越常被选中）。
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 超时（秒）。
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 追加请求头（例如某些中转站要求 `X-Title`）。
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// 连续失败多少次后熔断。
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    /// 熔断后的冷却时间（秒）。
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// 自定义 chat 路径；留空时按 `base_url` 自动推导。
    #[serde(default)]
    pub chat_path: Option<String>,
}

fn default_weight() -> u32 {
    1
}
fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    60
}
fn default_max_failures() -> u32 {
    3
}
fn default_cooldown() -> u64 {
    30
}

impl Endpoint {
    /// 一个最简端点。
    pub fn new(name: impl Into<String>, base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Endpoint {
            name: name.into(),
            base_url: base_url.into(),
            api_key: String::new(),
            model: model.into(),
            weight: 1,
            enabled: true,
            timeout_secs: 60,
            headers: Vec::new(),
            max_failures: 3,
            cooldown_secs: 30,
            chat_path: None,
        }
    }

    /// 本机 Ollama（OpenAI 兼容层）。
    pub fn ollama(model: impl Into<String>) -> Self {
        Endpoint::new("ollama", "http://127.0.0.1:11434/v1", model)
    }

    /// 本机 LM Studio。
    pub fn lmstudio(model: impl Into<String>) -> Self {
        Endpoint::new("lmstudio", "http://127.0.0.1:1234/v1", model)
    }

    /// OpenAI 官方。
    pub fn openai(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let mut e = Endpoint::new("openai", "https://api.openai.com/v1", model);
        e.api_key = api_key.into();
        e
    }

    /// 设置权重。
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight.max(1);
        self
    }

    /// 设置 API key。
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = key.into();
        self
    }

    /// 超时。
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }

    /// 冷却。
    pub fn cooldown(&self) -> Duration {
        Duration::from_secs(self.cooldown_secs)
    }

    /// 解析出实际的 `chat/completions` 地址。
    ///
    /// 推导规则（覆盖了现实里最常见的几种写法）：
    ///
    /// | `base_url` | 结果 |
    /// |---|---|
    /// | `https://api.openai.com/v1` | `…/v1/chat/completions` |
    /// | `https://api.deepseek.com` | `…/v1/chat/completions` |
    /// | `http://127.0.0.1:11434/v1` | `…/v1/chat/completions` |
    /// | `http://h:3000/proxy/v1/chat/completions` | 原样使用 |
    ///
    /// 需要更特殊的路径时用 [`Endpoint::chat_path`] 直接指定。
    pub fn chat_url(&self) -> String {
        if let Some(p) = &self.chat_path {
            if p.starts_with("http://") || p.starts_with("https://") {
                return p.clone();
            }
            return format!(
                "{}/{}",
                self.base_url.trim_end_matches('/'),
                p.trim_start_matches('/')
            );
        }
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            return base.to_string();
        }
        if base.contains("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }

    /// 校验配置是否可用。
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.name.trim().is_empty() {
            return Err(crate::error::LlmError::Config("端点 name 不能为空".into()));
        }
        if self.model.trim().is_empty() {
            return Err(crate::error::LlmError::Config(format!(
                "端点 `{}` 缺少 model",
                self.name
            )));
        }
        let base = self.base_url.trim();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(crate::error::LlmError::Config(format!(
                "端点 `{}` 的 base_url 必须以 http:// 或 https:// 开头，当前是 {:?}",
                self.name, self.base_url
            )));
        }
        Ok(())
    }
}

/// 从既有配置批量构造端点的便利方法。
pub fn from_env(names: &[&str]) -> Vec<Endpoint> {
    let mut out = Vec::new();
    for name in names {
        let key = format!("STYX_LLM_{}", name.to_uppercase().replace('-', "_"));
        let Ok(url) = std::env::var(format!("{key}_BASE_URL")) else {
            continue;
        };
        let model = std::env::var(format!("{key}_MODEL")).unwrap_or_else(|_| "gpt-4o-mini".into());
        let mut ep = Endpoint::new(*name, url, model);
        if let Ok(k) = std::env::var(format!("{key}_API_KEY")) {
            ep.api_key = k;
        }
        out.push(ep);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_url_derivation() {
        let cases = [
            ("https://api.openai.com/v1", "https://api.openai.com/v1/chat/completions"),
            ("https://api.deepseek.com", "https://api.deepseek.com/v1/chat/completions"),
            ("http://127.0.0.1:11434/v1", "http://127.0.0.1:11434/v1/chat/completions"),
            (
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
            ),
            (
                "http://h:3000/proxy/v1/chat/completions",
                "http://h:3000/proxy/v1/chat/completions",
            ),
            ("http://h:3000/", "http://h:3000/v1/chat/completions"),
        ];
        for (base, want) in cases {
            let e = Endpoint::new("t", base, "m");
            assert_eq!(e.chat_url(), want, "base={base}");
        }
    }

    #[test]
    fn explicit_chat_path_wins() {
        let mut e = Endpoint::new("t", "https://h/v1", "m");
        e.chat_path = Some("/custom/generate".into());
        assert_eq!(e.chat_url(), "https://h/v1/custom/generate");

        e.chat_path = Some("https://other/abs".into());
        assert_eq!(e.chat_url(), "https://other/abs");
    }

    #[test]
    fn validate_rejects_bad_config() {
        let e = Endpoint::new("", "https://h/v1", "m");
        assert!(e.validate().is_err());

        let e = Endpoint::new("x", "https://h/v1", "");
        assert!(e.validate().is_err());

        let e = Endpoint::new("x", "ftp://h/v1", "m");
        assert!(e.validate().is_err());

        assert!(Endpoint::new("x", "https://h/v1", "m").validate().is_ok());
    }

    #[test]
    fn presets_are_sane() {
        assert!(Endpoint::ollama("qwen2.5").chat_url().contains("11434/v1/chat/completions"));
        assert!(Endpoint::lmstudio("m").chat_url().contains("1234/v1/chat/completions"));
        let o = Endpoint::openai("gpt-4o-mini", "sk-x");
        assert_eq!(o.api_key, "sk-x");
        assert!(o.validate().is_ok());
    }

    #[test]
    fn weight_is_floored_at_one() {
        let e = Endpoint::new("x", "http://h", "m").with_weight(0);
        assert_eq!(e.weight, 1);
    }

    #[test]
    fn serde_round_trip_with_defaults() {
        let json = r#"{"name":"a","base_url":"http://h/v1","model":"m"}"#;
        let e: Endpoint = serde_json::from_str(json).unwrap();
        assert_eq!(e.weight, 1);
        assert!(e.enabled);
        assert_eq!(e.timeout_secs, 60);
        assert_eq!(e.max_failures, 3);
        assert_eq!(e.cooldown_secs, 30);
        assert!(e.api_key.is_empty());
    }
}
