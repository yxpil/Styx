//! 访问令牌与暴露面判断。
//!
//! 这一层只做两件事：**比对一个令牌**，和**判断一个监听地址有多暴露**。
//! 不做用户、角色、权限——那需要一套身份体系，而这里真正要回答的问题是
//! 一个更小的：*这个端口，谁能连？*
//!
//! 之所以把"暴露面判断"也放进来：安全上最常见的失误不是"认证写错了"，
//! 是**根本没意识到这个端口是开放的**。把地址分类做成一个可以断言的
//! 函数，启动时就能把它喊出来。

use std::net::IpAddr;

/// 访问令牌。
///
/// 空令牌不构造实例——"没有令牌"和"令牌是空串"必须是同一件事，
/// 否则很容易写出一个永远通过的校验。
#[derive(Clone)]
pub struct Token {
    secret: String,
}

impl std::fmt::Debug for Token {
    /// 刻意不打印内容。
    ///
    /// 令牌最常见的泄漏途径不是被攻击者拿到，是**被自己人打进日志**：
    /// 一个 `dbg!`、一次 panic 的 `{:?}`、一行启动信息就够。
    /// 从类型上堵住这条路，比每次都记得手动避开要可靠。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("secret", &"<已隐藏>")
            .field("len", &self.secret.len())
            .finish()
    }
}

impl Token {
    /// 从配置里取令牌；空串（或全空白）表示**不启用**。
    pub fn new(secret: impl Into<String>) -> Option<Token> {
        let secret = secret.into();
        if secret.trim().is_empty() {
            return None;
        }
        Some(Token { secret })
    }

    /// 令牌长度。
    ///
    /// 刻意**不叫** `len`：那个名字会招来 clippy 的 `len_without_is_empty`，
    /// 而一个恒为 `false` 的 `is_empty` 只是为了让 lint 闭嘴——空令牌
    /// 根本构造不出来，"有没有令牌"由 `Option<Token>` 回答。
    pub fn secret_len(&self) -> usize {
        self.secret.len()
    }

    /// 是否短得让人不放心。
    ///
    /// 阈值取 16 是有意的保守：这不是密码学下界，而是"至少不是
    /// 随手敲的几个字符"。真正该做的是生成随机串。
    pub fn is_weak(&self) -> bool {
        self.secret.len() < 16
    }

    /// 比对候选令牌（常量时间）。
    pub fn verify(&self, candidate: &str) -> bool {
        constant_time_eq(self.secret.as_bytes(), candidate.as_bytes())
    }

    /// 取出原文。
    ///
    /// 名字起得这么直白，是为了让它在 review 里格外扎眼——这个函数只有
    /// 一个正当用途：生成之后**打印一次**给人看。
    pub fn reveal(&self) -> &str {
        &self.secret
    }

    /// 生成一个随机令牌（32 个十六进制字符）。
    ///
    /// 用 `SystemTime` + 进程/线程信息做种，而不是引一个随机数库：
    /// 这里要的是"不可猜"，不是"密码学强度"，而且**这个值只在启动时
    /// 生成一次并打印给人看**，没有在线攻击面。
    pub fn generate() -> Token {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        let addr = &nanos as *const _ as u128;
        let mut state = nanos ^ (pid << 64) ^ addr.rotate_left(17);
        let mut out = String::with_capacity(32);
        for _ in 0..32 {
            // xorshift64* 的一步，够用且无依赖。
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let nibble = (state % 16) as u8;
            out.push(std::char::from_digit(nibble as u32, 16).unwrap_or('0'));
        }
        Token { secret: out }
    }
}

/// 常量时间比较。
///
/// 逐字节异或累加，**不提前返回**——`==` 会在第一个不同字节处停下，
/// 于是耗时随"猜对了几位前缀"变化，理论上可以被逐位试出来。
///
/// 长度不同时直接返回 `false`：长度本身不是秘密（令牌长度是固定的），
/// 而且真去补齐只会让代码更绕。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 监听地址的暴露程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// 只有本机能连。
    Loopback,
    /// 局域网可达。
    Private,
    /// 任何能路由到这台机器的人都能连。
    Public,
}

impl Exposure {
    /// 判一个监听地址（`host:port` 或裸 IP，可带方括号的 IPv6）。
    pub fn of(addr: &str) -> Exposure {
        let host = host_of(addr);
        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => {
                if v4.is_loopback() {
                    Exposure::Loopback
                } else if v4.is_private() || v4.is_link_local() {
                    Exposure::Private
                } else if v4.is_unspecified() {
                    // `0.0.0.0` 是"所有网卡"，也就是最暴露的那一档。
                    Exposure::Public
                } else {
                    Exposure::Public
                }
            }
            Ok(IpAddr::V6(v6)) => {
                if v6.is_loopback() {
                    Exposure::Loopback
                } else if v6.is_unspecified() {
                    Exposure::Public
                } else {
                    // fc00::/7 唯一本地地址
                    let seg = v6.segments()[0];
                    if seg & 0xfe00 == 0xfc00 {
                        Exposure::Private
                    } else {
                        Exposure::Public
                    }
                }
            }
            Err(_) => {
                let lower = host.to_ascii_lowercase();
                if lower == "localhost" {
                    Exposure::Loopback
                } else {
                    // 主机名解析不了就别猜"大概是内网"——按最坏情况算。
                    Exposure::Public
                }
            }
        }
    }
}

/// 从 `host:port` 里剥出 host（处理 IPv6 的方括号）。
fn host_of(addr: &str) -> &str {
    let addr = addr.trim();
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match addr.rsplit_once(':') {
        // 只有一个冒号才是 host:port；多个冒号说明是没带括号的 IPv6。
        Some((host, _)) if !host.contains(':') => host,
        _ => addr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_token_means_disabled() {
        assert!(Token::new("").is_none());
        assert!(Token::new("   ").is_none());
        assert!(Token::new("secret").is_some());
    }

    #[test]
    fn verify_accepts_only_the_exact_token() {
        let t = Token::new("correct-horse-battery").unwrap();
        assert!(t.verify("correct-horse-battery"));
        assert!(!t.verify("correct-horse-batterz"));
        assert!(!t.verify("correct-horse-battery "));
        assert!(!t.verify(""));
        assert!(!t.verify("correct-horse-battery-and-more"));
    }

    #[test]
    fn debug_never_leaks_the_secret() {
        let t = Token::new("super-secret-value-1234").unwrap();
        let shown = format!("{t:?}");
        assert!(!shown.contains("super-secret"), "{shown}");
        assert!(shown.contains("已隐藏"), "{shown}");
    }

    #[test]
    fn weak_tokens_are_flagged() {
        assert!(Token::new("abc").unwrap().is_weak());
        assert!(!Token::new("0123456789abcdef").unwrap().is_weak());
    }

    #[test]
    fn generated_tokens_are_long_and_unique() {
        let a = Token::generate();
        let b = Token::generate();
        assert_eq!(a.secret_len(), 32);
        assert!(!a.is_weak());
        assert_ne!(a.secret, b.secret, "两次生成不该一样");
    }

    #[test]
    fn loopback_addresses_are_recognised() {
        assert_eq!(Exposure::of("127.0.0.1:8770"), Exposure::Loopback);
        assert_eq!(Exposure::of("127.1.2.3:1"), Exposure::Loopback);
        assert_eq!(Exposure::of("[::1]:8770"), Exposure::Loopback);
        assert_eq!(Exposure::of("localhost:8770"), Exposure::Loopback);
    }

    #[test]
    fn unspecified_address_is_the_most_exposed() {
        assert_eq!(Exposure::of("0.0.0.0:8770"), Exposure::Public);
        assert_eq!(Exposure::of("[::]:8770"), Exposure::Public);
    }

    #[test]
    fn private_ranges_are_recognised() {
        assert_eq!(Exposure::of("192.168.1.10:80"), Exposure::Private);
        assert_eq!(Exposure::of("10.0.0.5:80"), Exposure::Private);
        assert_eq!(Exposure::of("172.16.0.1:80"), Exposure::Private);
        assert_eq!(Exposure::of("172.31.255.254:80"), Exposure::Private);
        // 172.32 已经出了私有段
        assert_eq!(Exposure::of("172.32.0.1:80"), Exposure::Public);
        assert_eq!(Exposure::of("fc00::1"), Exposure::Private);
    }

    #[test]
    fn public_and_unresolvable_hosts_are_public() {
        assert_eq!(Exposure::of("8.8.8.8:80"), Exposure::Public);
        assert_eq!(Exposure::of("example.com:80"), Exposure::Public);
        // 解析不了就别猜成内网
        assert_eq!(Exposure::of("not-a-real-host:80"), Exposure::Public);
    }
}
