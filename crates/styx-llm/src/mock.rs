//! 离线 Mock 模型。
//!
//! 存在的意义有三：
//!
//! 1. **冒烟测试**：`styx demo` 不联网也能把整条链路跑通一遍
//!    （召回 → 联想 → 装配 → 生成 → 解析 → 守护 → 结算 → 落库）；
//! 2. **CI**：GitHub Actions 里不该依赖任何外部 API key；
//! 3. **对照实验**：怀疑"是模型的问题还是内核的问题"时，
//!    换成 Mock 跑一遍就能把变量隔离掉。
//!
//! Mock 的输出**刻意使用真实标签格式**，因此它同时也在验证
//! [`styx_core::reply::Reply`] 的解析器与 [`styx_core::Guard`] 的审计逻辑。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use styx_core::ports::{ChatMessage, Completion, LlmOptions, LlmPort};
use styx_core::text::summarize;

use crate::backend::ChatBackend;
use crate::endpoint::Endpoint;
use crate::error::Result;
use crate::pool::EndpointPool;

/// 一个不联网、可复现的语言模型。
pub struct MockBackend {
    script: Vec<String>,
    cursor: AtomicUsize,
    /// 前 N 次调用故意失败（用于演示多端点故障转移）。
    fail_first: AtomicUsize,
    label: String,
}

impl std::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBackend")
            .field("script_len", &self.script.len())
            .field("label", &self.label)
            .finish()
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBackend {
    /// 用内置角色扮演脚本。
    pub fn new() -> Self {
        MockBackend {
            script: default_script(),
            cursor: AtomicUsize::new(0),
            fail_first: AtomicUsize::new(0),
            label: "mock".into(),
        }
    }

    /// 自定义脚本（会循环使用）。
    pub fn with_script<I, S>(script: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let script: Vec<String> = script.into_iter().map(Into::into).collect();
        MockBackend {
            script: if script.is_empty() {
                default_script()
            } else {
                script
            },
            cursor: AtomicUsize::new(0),
            fail_first: AtomicUsize::new(0),
            label: "mock".into(),
        }
    }

    /// 让前 `n` 次调用失败，用来演示故障转移。
    pub fn with_fail_first(mut self, n: usize) -> Self {
        self.fail_first = AtomicUsize::new(n);
        self
    }

    /// 换展示名。
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

impl ChatBackend for MockBackend {
    fn name(&self) -> &str {
        &self.label
    }

    fn chat(
        &self,
        endpoint: &Endpoint,
        messages: &[ChatMessage],
        opts: &LlmOptions,
    ) -> Result<Completion> {
        let left = self.fail_first.load(Ordering::SeqCst);
        if left > 0 {
            self.fail_first.store(left - 1, Ordering::SeqCst);
            return Err(crate::error::LlmError::Http {
                endpoint: endpoint.name.clone(),
                message: "（Mock）模拟端点故障".into(),
            });
        }

        let turn = self.cursor.fetch_add(1, Ordering::SeqCst);
        let template = &self.script[turn % self.script.len()];

        // `{{echo}}` 就是把用户最后一句话原样（截断后）嵌回去。
        //
        // 这里刻意**不**做关键词抽取：占位符叫 echo，就该回声。
        // 用关键词拼出来的东西（「想看、看看」）读起来像随机片段，
        // 反而让离线 demo 显得很傻。
        let last_user = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let excerpt = summarize(&last_user, 24);
        let text = template.replace("{{echo}}", &excerpt);

        let model = opts.model.clone().unwrap_or_else(|| endpoint.model.clone());

        let mut completion = Completion::new(text, model, endpoint.name.clone());
        // 给一个粗糙但稳定的 token 估算，让回合报告不至于全是 0
        completion.prompt_tokens = messages
            .iter()
            .map(|m| m.content.chars().count())
            .sum::<usize>() as u64
            / 2;
        completion.completion_tokens = 40;
        Ok(completion)
    }
}

/// 内置脚本：4 段轮换，都带完整的标签结构。
fn default_script() -> Vec<String> {
    vec![
        r#"[做] 她把手里的书合上，指腹在书脊上停了半秒，才抬眼。
[说] 你来了。
[想] 又是为了那件事。这次躲不掉了。
[情] 心情=戒备 效价=-0.05 唤醒=+0.10 紧张=+0.12
[忆] 对方主动提起了「{{echo}}」 | 标签=对话 | 重要度=0.6"#
            .to_string(),
        r#"[做] 她把杯子往桌子里侧推了半寸，像是顺手，又像是不想让人碰。
[说] 那个东西，我不打算解释第二遍。
[想] 说出口的话就收不回来了。可是不说，也一样。
[情] 心情=不悦 效价=-0.12 唤醒=+0.08 紧张=+0.10
[关系] 对方 好感=-0.05 信任=-0.02"#
            .to_string(),
        r#"[做] 窗外的雨声忽然大了一点。她没有回头。
[说] 你要是想坐，就坐。
[说] 只是别碰最上面那一格。
[想] 「{{echo}}」——他怎么会知道这个词。
[情] 心情=平静 效价=+0.06 唤醒=-0.05 精力=-0.05"#
            .to_string(),
        r#"[做] 她终于转过身，靠着柜台，把袖子往上卷了一截。
[说] 好吧。
[说] 你想知道什么，我问，你答。
[想] 主动权得拿回来。哪怕只拿回一点点。
[情] 心情=紧绷 效价=+0.10 唤醒=+0.15 紧张=+0.18
[忆] 双方约定交换信息 | 标签=约定,信任 | 重要度=0.8
[场] 时间=深夜 局面=雨还在下，店里只剩两个人"#
            .to_string(),
    ]
}

/// 一个开箱即用的离线模型（单端点 + Mock 后端）。
pub fn offline_llm() -> Arc<dyn LlmPort> {
    offline_pool()
}

/// 离线多端点池：一个正常端点 + 一个先挂一次再恢复的端点，
/// 用来在 `demo` 里顺带演示故障转移。
pub fn offline_pool() -> Arc<EndpointPool> {
    let healthy = MockBackend::new().with_label("mock-primary");
    let flaky = MockBackend::new()
        .with_label("mock-standby")
        .with_fail_first(1);
    let pool = EndpointPool::new(
        vec![
            Endpoint::new("primary", "http://mock.local/v1", "mock-roleplay").with_weight(3),
            Endpoint::new("standby", "http://mock.local/v1", "mock-roleplay").with_weight(1),
        ],
        Arc::new(MockMultiplexer { healthy, flaky }),
    )
    .expect("Mock 端点配置一定是合法的");
    Arc::new(pool)
}

/// 按端点名把请求分派给不同的 Mock 实例。
struct MockMultiplexer {
    healthy: MockBackend,
    flaky: MockBackend,
}

impl ChatBackend for MockMultiplexer {
    fn name(&self) -> &str {
        "mock-multiplexer"
    }

    fn chat(
        &self,
        endpoint: &Endpoint,
        messages: &[ChatMessage],
        opts: &LlmOptions,
    ) -> Result<Completion> {
        if endpoint.name == "standby" {
            self.flaky.chat(endpoint, messages, opts)
        } else {
            self.healthy.chat(endpoint, messages, opts)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep() -> Endpoint {
        Endpoint::new("ep", "http://mock.local/v1", "mock-model")
    }

    fn msgs(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage::user(text)]
    }

    #[test]
    fn produces_parseable_roleplay_output() {
        let b = MockBackend::new();
        let c = b
            .chat(&ep(), &msgs("我想看看那本旧相册"), &LlmOptions::default())
            .unwrap();
        assert!(!c.text.is_empty());
        // 输出必须能被内核的解析器读懂
        let reply = styx_core::Reply::parse(&c.text).unwrap();
        assert!(!reply.speech.is_empty(), "应当解析出台词：{}", c.text);
        assert!(!reply.is_empty());
    }

    #[test]
    fn first_script_entry_records_a_memory() {
        let b = MockBackend::new();
        let c = b
            .chat(&ep(), &msgs("相册"), &LlmOptions::default())
            .unwrap();
        let reply = styx_core::Reply::parse(&c.text).unwrap();
        assert_eq!(reply.memories.len(), 1);
        assert!(
            reply.memories[0].text.contains("相册"),
            "{:?}",
            reply.memories
        );
    }

    #[test]
    fn script_cycles() {
        let b = MockBackend::with_script(vec!["[说] 一", "[说] 二"]);
        let a = b.chat(&ep(), &msgs("x"), &LlmOptions::default()).unwrap();
        let c = b.chat(&ep(), &msgs("x"), &LlmOptions::default()).unwrap();
        let d = b.chat(&ep(), &msgs("x"), &LlmOptions::default()).unwrap();
        assert_eq!(a.text, "[说] 一");
        assert_eq!(c.text, "[说] 二");
        assert_eq!(d.text, "[说] 一");
    }

    #[test]
    fn echo_placeholder_is_substituted() {
        let b = MockBackend::with_script(vec!["[说] 你提到「{{echo}}」"]);
        let c = b
            .chat(&ep(), &msgs("我想看看那本旧相册"), &LlmOptions::default())
            .unwrap();
        assert!(!c.text.contains("{{echo}}"), "占位符应当被替换：{}", c.text);
        // 回声必须是用户输入里真实存在的片段——不是占位符、也不是凭空生成
        let echo = c
            .text
            .split('「')
            .nth(1)
            .and_then(|s| s.split('」').next())
            .expect("回复里应当有引号包住的回声");
        assert!(!echo.is_empty(), "{}", c.text);
        assert!(
            "我想看看那本旧相册".contains(echo),
            "回声应当来自用户输入，实际 {echo:?}"
        );
    }

    #[test]
    fn fail_first_produces_errors_then_recovers() {
        let b = MockBackend::new().with_fail_first(2);
        assert!(b.chat(&ep(), &msgs("x"), &LlmOptions::default()).is_err());
        assert!(b.chat(&ep(), &msgs("x"), &LlmOptions::default()).is_err());
        assert!(b.chat(&ep(), &msgs("x"), &LlmOptions::default()).is_ok());
    }

    #[test]
    fn empty_script_falls_back_to_default() {
        let b = MockBackend::with_script(Vec::<String>::new());
        assert!(!b.script.is_empty());
    }

    #[test]
    fn offline_pool_reports_endpoints_and_recovers() {
        let pool = offline_pool();
        assert_eq!(pool.endpoint_count(), 2);
        assert!(pool.health());

        // standby 第一次被选中时会挂，调度应当切到 primary 并把它记进 stats。
        // 需要多打几次：SWRR 按 3:1 轮询，standby 每四轮才轮到一次。
        let mut failures = 0;
        for _ in 0..8 {
            match pool.complete(&msgs("你好"), &LlmOptions::default()) {
                Ok(c) => assert!(!c.text.is_empty()),
                Err(_) => failures += 1,
            }
        }
        assert_eq!(failures, 0, "单个端点抖一下不该让兜底池整体失败");

        let stats = pool.stats();
        assert_eq!(stats.len(), 2);
        assert!(
            stats.iter().any(|s| s.err_total > 0),
            "应当记录到一次故障：{stats:?}"
        );
    }

    #[test]
    fn offline_llm_is_object_safe() {
        let llm: Arc<dyn LlmPort> = offline_llm();
        assert!(llm.health());
        let c = llm
            .complete(&msgs("随便说点什么"), &LlmOptions::default())
            .unwrap();
        assert!(!c.text.is_empty());
    }
}
