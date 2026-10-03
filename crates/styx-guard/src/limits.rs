//! 服务层配额。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 服务层配额。
///
/// 每个字段的"不限"取值都是 `0`，且**默认全部不限**。这样打开
/// `styx.toml` 的人看到的是"没有额外限制"，而不是"被悄悄加了限制"——
/// 个人模式下这套东西的存在感应该为零。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    /// 同时在处理的连接数上限；`0` = 不限。
    pub max_connections: usize,
    /// 常驻会话数上限，超出时淘汰最久未动的；`0` = 不限。
    pub max_sessions: usize,
    /// 单行请求的最大字节数；`0` = 不限。
    pub max_line_bytes: usize,
    /// 拿不到准入名额时最多等多久（毫秒）；`0` = 立即拒绝。
    pub acquire_timeout_ms: u64,
    /// 关闭时等在飞请求收尾的上限（毫秒）。
    pub drain_timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 0,
            max_sessions: 0,
            max_line_bytes: 0,
            acquire_timeout_ms: 0,
            drain_timeout_ms: 5_000,
        }
    }
}

impl Limits {
    /// 对外自托管可以直接照用的一档。
    ///
    /// 这里的 `max_connections` 不是随手写的：内核线程默认 8 MB 栈，
    /// **同步模型下连接数上限就是线程数上限**——256 个线程光是虚拟地址
    /// 空间就占 2 GB。真要往上走，得先换连接复用，而不是把这个数字调大。
    pub fn production() -> Self {
        Limits {
            max_connections: 64,
            max_sessions: 256,
            max_line_bytes: 256 * 1024,
            acquire_timeout_ms: 1_000,
            drain_timeout_ms: 10_000,
        }
    }

    /// 是否限制连接数。
    pub fn limits_connections(&self) -> bool {
        self.max_connections > 0
    }

    /// 是否限制会话数。
    pub fn limits_sessions(&self) -> bool {
        self.max_sessions > 0
    }

    /// 是否限制单行长度。
    pub fn limits_line(&self) -> bool {
        self.max_line_bytes > 0
    }

    /// 准入等待时长。
    pub fn acquire_timeout(&self) -> Duration {
        Duration::from_millis(self.acquire_timeout_ms)
    }

    /// 收尾等待时长。
    pub fn drain_timeout(&self) -> Duration {
        Duration::from_millis(self.drain_timeout_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unlimited() {
        let l = Limits::default();
        assert!(!l.limits_connections(), "个人模式默认不该有连接上限");
        assert!(!l.limits_sessions());
        assert!(!l.limits_line());
        assert_eq!(l.acquire_timeout(), Duration::ZERO);
    }

    #[test]
    fn production_limits_everything_it_can() {
        let l = Limits::production();
        assert!(l.limits_connections());
        assert!(l.limits_sessions());
        assert!(l.limits_line());
        assert!(l.acquire_timeout() > Duration::ZERO);
        assert!(l.drain_timeout() > Duration::ZERO);
    }

    #[test]
    fn partial_toml_keeps_the_rest_unlimited() {
        let l: Limits = toml::from_str("max_connections = 8\n").unwrap();
        assert_eq!(l.max_connections, 8);
        assert_eq!(l.max_sessions, 0);
        assert_eq!(l.max_line_bytes, 0);
    }
}
