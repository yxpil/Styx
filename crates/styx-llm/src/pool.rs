//! [`LlmPort`] 的多端点实现：把调度器与传输后端接起来。
//!
//! 行为约定（这些约定是有意为之，不是实现细节）：
//!
//! - **一次调用内不重复试同一个端点**。同一个端点在几秒内连挂两次，
//!   第二次多半还是挂，重试它只是浪费一次超时。
//! - **失败会记账**。连续失败到阈值触发熔断，冷却期结束自动回归；
//!   全部熔断时进入半开试探而不是直接失败。
//! - **模型名可以按端点覆写**：`LlmOptions.model` 为空时用端点自己的 model。
//!   这让"同一份提示词在不同端点跑不同模型"变成零成本。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use styx_core::error::{Result as StyxResult, StyxError};
use styx_core::ports::{ChatMessage, Completion, LlmOptions, LlmPort};

use crate::backend::ChatBackend;
use crate::endpoint::Endpoint;
use crate::error::LlmError;
use crate::scheduler::{EndpointStat, Scheduler, SchedulerSummary};

/// 多端点语言模型。
pub struct EndpointPool {
    scheduler: Mutex<Scheduler>,
    backend: Arc<dyn ChatBackend>,
    /// 单次调用最多尝试多少个端点；0 表示"有多少试多少"。
    max_attempts: usize,
    /// 最近一次命中的端点名（可观测性用）。
    last_endpoint: Mutex<Option<String>>,
    /// 最近一次调用耗时（毫秒）。
    last_latency_ms: Mutex<u64>,
}

impl std::fmt::Debug for EndpointPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointPool")
            .field("backend", &self.backend.name())
            .field("endpoints", &self.endpoint_count())
            .finish_non_exhaustive()
    }
}

impl EndpointPool {
    /// 用给定端点与后端新建。
    pub fn new(endpoints: Vec<Endpoint>, backend: Arc<dyn ChatBackend>) -> StyxResult<Self> {
        let scheduler = Scheduler::new(endpoints).map_err(into_styx)?;
        Ok(EndpointPool {
            scheduler: Mutex::new(scheduler),
            backend,
            max_attempts: 0,
            last_endpoint: Mutex::new(None),
            last_latency_ms: Mutex::new(0),
        })
    }

    /// 限制单次调用的最大尝试次数。
    pub fn with_max_attempts(mut self, n: usize) -> Self {
        self.max_attempts = n;
        self
    }

    /// 端点配置副本。
    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.scheduler
            .lock()
            .map(|s| s.endpoints().to_vec())
            .unwrap_or_default()
    }

    /// 端点状态快照。
    pub fn stats(&self) -> Vec<EndpointStat> {
        let now = Instant::now();
        self.scheduler
            .lock()
            .map(|s| s.stats(now))
            .unwrap_or_default()
    }

    /// 调度概览。
    pub fn summary(&self) -> SchedulerSummary {
        let now = Instant::now();
        self.scheduler
            .lock()
            .map(|s| s.summary(now))
            .unwrap_or(SchedulerSummary {
                endpoints: 0,
                healthy: 0,
                total_ok: 0,
                total_err: 0,
                average_latency_ms: 0,
            })
    }

    /// 手动清空所有端点的熔断状态。
    pub fn reset(&self) {
        if let Ok(mut s) = self.scheduler.lock() {
            for i in 0..s.len() {
                s.reset(i);
            }
        }
    }

    /// 最近命中的端点。
    pub fn last_endpoint(&self) -> Option<String> {
        self.last_endpoint.lock().ok().and_then(|e| e.clone())
    }

    /// 最近一次调用耗时。
    pub fn last_latency_ms(&self) -> u64 {
        *self.last_latency_ms.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 至少有一个健康端点（不触发半开）。
    pub fn has_healthy(&self) -> bool {
        let now = Instant::now();
        self.scheduler
            .lock()
            .map(|s| s.has_healthy(now))
            .unwrap_or(false)
    }
}

impl LlmPort for EndpointPool {
    fn name(&self) -> &str {
        "endpoint-pool"
    }

    fn health(&self) -> bool {
        self.summary().healthy > 0
    }

    fn endpoint_count(&self) -> usize {
        self.scheduler.lock().map(|s| s.len()).unwrap_or(0)
    }

    fn complete(&self, messages: &[ChatMessage], opts: &LlmOptions) -> StyxResult<Completion> {
        self.complete_inner(messages, opts).map_err(into_styx)
    }

    fn status(&self) -> String {
        let stats = self.stats();
        let healthy = stats.iter().filter(|s| s.healthy()).count();
        let mut s = format!(
            "{}/{} 个端点健康 · 后端 {}",
            healthy,
            stats.len(),
            self.backend.name()
        );
        if let Some(last) = self.last_endpoint() {
            s.push_str(&format!(
                " · 最近命中 {}（{}ms）",
                last,
                self.last_latency_ms()
            ));
        }
        s
    }
}

impl EndpointPool {
    fn complete_inner(&self, messages: &[ChatMessage], opts: &LlmOptions) -> crate::error::Result<Completion> {
        if messages.is_empty() {
            return Err(LlmError::Config("消息列表为空".into()));
        }
        let total = self.endpoint_count();
        if total == 0 {
            return Err(LlmError::NoEndpoint { total: 0 });
        }
        let attempts = if self.max_attempts == 0 {
            total
        } else {
            self.max_attempts.min(total)
        };

        let mut tried: Vec<usize> = Vec::new();
        let mut last_err: Option<String> = None;

        for _ in 0..attempts {
            let now = Instant::now();
            let idx = {
                let mut sch = self
                    .scheduler
                    .lock()
                    .map_err(|_| LlmError::Other("调度器锁被污染".into()))?;
                // 跳过本次调用已经试过的端点
                let mut chosen = None;
                for _ in 0..sch.len().max(1) {
                    match sch.pick(now) {
                        Some(i) if !tried.contains(&i) => {
                            chosen = Some(i);
                            break;
                        }
                        Some(_) => continue,
                        None => break,
                    }
                }
                chosen
            };
            let Some(idx) = idx else {
                break;
            };
            tried.push(idx);

            let ep = {
                let sch = self
                    .scheduler
                    .lock()
                    .map_err(|_| LlmError::Other("调度器锁被污染".into()))?;
                match sch.endpoint(idx) {
                    Some(e) => e.clone(),
                    None => break,
                }
            };

            // 端点没配模型时用端点默认值
            let mut eff = opts.clone();
            if eff.model.as_deref().map(|m| m.trim().is_empty()).unwrap_or(true) {
                eff.model = Some(ep.model.clone());
            }

            let started = Instant::now();
            match self.backend.chat(&ep, messages, &eff) {
                Ok(mut c) => {
                    if c.endpoint.is_empty() {
                        c.endpoint = ep.name.clone();
                    }
                    let latency = started.elapsed().as_millis() as u64;
                    if let Ok(mut sch) = self.scheduler.lock() {
                        sch.record_success(idx);
                    }
                    if let Ok(mut slot) = self.last_endpoint.lock() {
                        *slot = Some(ep.name.clone());
                    }
                    if let Ok(mut slot) = self.last_latency_ms.lock() {
                        *slot = latency;
                    }
                    return Ok(c);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if let Ok(mut sch) = self.scheduler.lock() {
                        sch.record_failure(idx, Instant::now(), msg.clone());
                    }
                    last_err = Some(format!("{}：{}", ep.name, msg));
                }
            }
        }

        Err(match last_err {
            Some(last) => LlmError::AllEndpointsFailed { last },
            None => LlmError::NoEndpoint { total },
        })
    }
}

/// `LlmError` → 内核错误。
pub fn into_styx(e: LlmError) -> StyxError {
    match e {
        LlmError::NoEndpoint { .. } => StyxError::PortUnavailable {
            port: "llm",
            reason: e.to_string(),
        },
        other => StyxError::Llm(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// 可按脚本挂掉的后端。
    struct ScriptedBackend {
        /// 每个端点连续失败多少次（0 表示一直成功）
        fail_plan: Mutex<std::collections::HashMap<String, usize>>,
        calls: Mutex<Vec<String>>,
        fail_counts: Mutex<std::collections::HashMap<String, usize>>,
        total: AtomicUsize,
    }

    impl ScriptedBackend {
        fn new(fail_plan: &[(&str, usize)]) -> Self {
            ScriptedBackend {
                fail_plan: Mutex::new(
                    fail_plan
                        .iter()
                        .map(|(k, v)| (k.to_string(), *v))
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
                fail_counts: Mutex::new(std::collections::HashMap::new()),
                total: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ChatBackend for ScriptedBackend {
        fn name(&self) -> &str {
            "scripted"
        }
        fn chat(
            &self,
            endpoint: &Endpoint,
            _m: &[ChatMessage],
            opts: &LlmOptions,
        ) -> crate::error::Result<Completion> {
            self.total.fetch_add(1, Ordering::SeqCst);
            self.calls.lock().unwrap().push(endpoint.name.clone());
            let budget = self
                .fail_plan
                .lock()
                .unwrap()
                .get(&endpoint.name)
                .copied()
                .unwrap_or(0);
            let mut counts = self.fail_counts.lock().unwrap();
            let used = counts.entry(endpoint.name.clone()).or_insert(0);
            if *used < budget {
                *used += 1;
                return Err(LlmError::Http {
                    endpoint: endpoint.name.clone(),
                    message: "模拟故障".into(),
                });
            }
            let model = opts.model.clone().unwrap_or_else(|| endpoint.model.clone());
            Ok(Completion::new(
                format!("来自 {} 的回复", endpoint.name),
                model,
                endpoint.name.clone(),
            ))
        }
    }

    fn ep(name: &str, weight: u32) -> Endpoint {
        Endpoint::new(name, "http://127.0.0.1:1/v1", format!("{name}-model"))
            .with_weight(weight)
    }

    fn msgs() -> Vec<ChatMessage> {
        vec![ChatMessage::user("你好")]
    }

    #[test]
    fn single_endpoint_round_trip() {
        let pool = EndpointPool::new(vec![ep("a", 1)], Arc::new(ScriptedBackend::new(&[]))).unwrap();
        let c = pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        assert!(c.text.contains("来自 a"));
        assert_eq!(c.endpoint, "a");
        assert_eq!(c.model, "a-model");
        assert_eq!(pool.endpoint_count(), 1);
        assert!(pool.health());
        assert!(pool.status().contains("1/1 个端点健康"));
    }

    #[test]
    fn fails_over_to_the_next_endpoint() {
        let backend = Arc::new(ScriptedBackend::new(&[("a", 10)]));
        let pool =
            EndpointPool::new(vec![ep("a", 1), ep("b", 1)], backend.clone()).unwrap();
        let c = pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        assert_eq!(c.endpoint, "b", "a 一直挂，应当切换到 b");
        assert!(!backend.calls().is_empty());
    }

    #[test]
    fn never_retries_the_same_endpoint_twice_in_one_call() {
        let backend = Arc::new(ScriptedBackend::new(&[("a", 10), ("b", 10), ("c", 10)]));
        let pool = EndpointPool::new(
            vec![ep("a", 1), ep("b", 1), ep("c", 1)],
            backend.clone(),
        )
        .unwrap();
        let err = pool.complete(&msgs(), &LlmOptions::default()).unwrap_err();
        assert!(err.to_string().contains("所有端点都失败了"), "got {err}");
        let calls = backend.calls();
        assert_eq!(calls.len(), 3, "每个端点只该试一次：{calls:?}");
        let mut uniq = calls.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 3);
    }

    #[test]
    fn circuit_breaker_skips_a_dead_endpoint_after_repeated_failures() {
        // a 永远挂；跑几轮后 a 应当被熔断，不再被选中
        let backend = Arc::new(ScriptedBackend::new(&[("a", 1000)]));
        let pool = EndpointPool::new(vec![ep("a", 1), ep("b", 1)], backend.clone()).unwrap();
        for _ in 0..4 {
            let c = pool.complete(&msgs(), &LlmOptions::default()).unwrap();
            assert_eq!(c.endpoint, "b");
        }
        let stats = pool.stats();
        let a = stats.iter().find(|s| s.name == "a").unwrap();
        assert!(!a.healthy(), "a 应当已熔断：{a:?}");
        assert!(a.cooldown_secs > 0);

        // 熔断后 a 不应再被调用
        let before = backend.calls().iter().filter(|c| *c == "a").count();
        pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        let after = backend.calls().iter().filter(|c| *c == "a").count();
        assert_eq!(before, after, "熔断中的端点不该被再次调用");
    }

    #[test]
    fn success_clears_failure_state_so_endpoint_returns() {
        // a 第一次挂，之后恢复正常
        let backend = Arc::new(ScriptedBackend::new(&[("a", 1)]));
        let pool = EndpointPool::new(vec![ep("a", 1), ep("b", 1)], backend).unwrap();
        let c1 = pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        assert_eq!(c1.endpoint, "b");
        // 失败计数应当已记录，但未达阈值
        let a = pool.stats().into_iter().find(|s| s.name == "a").unwrap();
        assert_eq!(a.err_total, 1);
        // 再跑几轮，a 恢复后会被选中
        let mut hit_a = false;
        for _ in 0..8 {
            if pool.complete(&msgs(), &LlmOptions::default()).unwrap().endpoint == "a" {
                hit_a = true;
                break;
            }
        }
        assert!(hit_a, "a 恢复后应当重新参与调度");
    }

    #[test]
    fn weighted_distribution_is_respected() {
        let backend = Arc::new(ScriptedBackend::new(&[]));
        let pool = EndpointPool::new(
            vec![ep("a", 3), ep("b", 1)],
            backend.clone(),
        )
        .unwrap();
        for _ in 0..40 {
            pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        }
        let calls = backend.calls();
        let a = calls.iter().filter(|c| *c == "a").count();
        let b = calls.iter().filter(|c| *c == "b").count();
        assert!((a as i64 - 30).abs() <= 2, "a={a}");
        assert!((b as i64 - 10).abs() <= 2, "b={b}");
    }

    #[test]
    fn model_can_be_overridden_per_options() {
        let pool =
            EndpointPool::new(vec![ep("a", 1)], Arc::new(ScriptedBackend::new(&[]))).unwrap();
        let opts = LlmOptions {
            model: Some("custom-model".into()),
            ..Default::default()
        };
        let c = pool.complete(&msgs(), &opts).unwrap();
        assert_eq!(c.model, "custom-model");
    }

    #[test]
    fn empty_options_fall_back_to_endpoint_model() {
        let pool =
            EndpointPool::new(vec![ep("a", 1)], Arc::new(ScriptedBackend::new(&[]))).unwrap();
        let c = pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        assert_eq!(c.model, "a-model");
        // 空白模型名也走回退
        let opts = LlmOptions {
            model: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(pool.complete(&msgs(), &opts).unwrap().model, "a-model");
    }

    #[test]
    fn empty_messages_are_rejected() {
        let pool =
            EndpointPool::new(vec![ep("a", 1)], Arc::new(ScriptedBackend::new(&[]))).unwrap();
        let err = pool.complete(&[], &LlmOptions::default()).unwrap_err();
        assert!(err.to_string().contains("消息列表为空"));
    }

    #[test]
    fn no_endpoints_is_a_config_error() {
        let err = EndpointPool::new(vec![], Arc::new(ScriptedBackend::new(&[]))).unwrap_err();
        // 空端点列表是「配错了」，不是「暂时不可用」：不能标成可降级的
        // 端口故障，否则运维看到的是角色默默退回 Mock，而不是一条明确的配置报错。
        assert!(
            err.to_string().contains("至少需要配置一个模型端点"),
            "{err}"
        );
        assert!(!err.is_degradable(), "{err}");
    }

    #[test]
    fn max_attempts_limits_failover() {
        let backend = Arc::new(ScriptedBackend::new(&[("a", 10), ("b", 10), ("c", 10)]));
        let pool = EndpointPool::new(
            vec![ep("a", 1), ep("b", 1), ep("c", 1)],
            backend.clone(),
        )
        .unwrap()
        .with_max_attempts(2);
        let err = pool.complete(&msgs(), &LlmOptions::default()).unwrap_err();
        assert!(err.to_string().contains("所有端点都失败了"));
        assert_eq!(backend.calls().len(), 2);
    }

    #[test]
    fn all_circuits_open_still_tries_something() {
        let backend = Arc::new(ScriptedBackend::new(&[]));
        let pool = EndpointPool::new(vec![ep("a", 1), ep("b", 1)], backend).unwrap();
        // 手动把两个端点都打挂
        {
            let mut sch = pool.scheduler.lock().unwrap();
            let now = Instant::now();
            for i in 0..2 {
                for _ in 0..3 {
                    sch.record_failure(i, now, "boom");
                }
            }
            assert!(!sch.has_healthy(now));
        }
        // 半开试探应当让调用成功，而不是直接报"没有可用端点"
        let c = pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        assert!(!c.text.is_empty());
    }

    #[test]
    fn reset_restores_every_endpoint() {
        let backend = Arc::new(ScriptedBackend::new(&[("a", 100)]));
        let pool = EndpointPool::new(vec![ep("a", 1), ep("b", 1)], backend).unwrap();
        for _ in 0..5 {
            pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        }
        assert!(pool.stats().iter().any(|s| !s.healthy()));
        pool.reset();
        assert!(pool.stats().iter().all(|s| s.healthy()));
        assert_eq!(pool.stats().iter().map(|s| s.err_total).sum::<u64>(), 0);
    }

    #[test]
    fn latency_and_last_endpoint_are_tracked() {
        let pool =
            EndpointPool::new(vec![ep("a", 1)], Arc::new(ScriptedBackend::new(&[]))).unwrap();
        pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        assert_eq!(pool.last_endpoint().as_deref(), Some("a"));
        assert!(pool.last_latency_ms() < 5_000);
    }

    #[test]
    fn slow_backend_does_not_deadlock_the_lock() {
        struct Slow;
        impl ChatBackend for Slow {
            fn name(&self) -> &str {
                "slow"
            }
            fn chat(
                &self,
                e: &Endpoint,
                _m: &[ChatMessage],
                _o: &LlmOptions,
            ) -> crate::error::Result<Completion> {
                std::thread::sleep(Duration::from_millis(20));
                Ok(Completion::new("慢但成功", "m", e.name.clone()))
            }
        }
        let pool = EndpointPool::new(vec![ep("a", 1)], Arc::new(Slow)).unwrap();
        let c = pool.complete(&msgs(), &LlmOptions::default()).unwrap();
        assert_eq!(c.text, "慢但成功");
    }
}
