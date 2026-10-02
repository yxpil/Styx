//! 后端抽象：一次"拿着端点配置去要一段文本"的调用。
//!
//! 把 **调度**（选哪个端点、失败了怎么办）与 **传输**（怎么发 HTTP）分开，
//! 好处是调度逻辑可以完全离线地做单元测试——注入一个假后端即可，
//! 不必起 HTTP 服务，也不必联网。

use styx_core::ports::{ChatMessage, Completion, LlmOptions};

use crate::endpoint::Endpoint;
use crate::error::Result;

/// 一次补全的传输后端。
pub trait ChatBackend: Send + Sync {
    /// 后端名字（用于日志）。
    fn name(&self) -> &str;

    /// 用给定端点生成一次补全。
    fn chat(
        &self,
        endpoint: &Endpoint,
        messages: &[ChatMessage],
        opts: &LlmOptions,
    ) -> Result<Completion>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trait_is_object_safe() {
        // 只编译期验证：dyn ChatBackend 可用
        fn _takes(_b: &dyn ChatBackend) {}
    }
}
