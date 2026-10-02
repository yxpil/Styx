//! 端点调度：**平滑加权轮询（SWRR）+ 熔断 + 半开试探**。
//!
//! 这是 [RLBCM](https://github.com/yxpil/RLBCM) 那类负载均衡思想在模型调用上的落地，
//! 但刻意保持"纯逻辑、无 IO"——所有时间来自调用方传入的 `Instant`，
//! 因此熔断与冷却可以精确地做单元测试，不必 `sleep`。
//!
//! ## 三个机制
//!
//! 1. **平滑加权轮询**：权重 3 和 1 的两个端点，调度序列是 `A A B A` 而不是
//!    `A A A B`——避免加权轮询在某一瞬间把全部压力压到同一个端点上
//!    （nginx 的做法，也叫 SWRR）。
//! 2. **熔断**：连续失败到阈值就把它踢出候选池一段时间。
//!    这里用**连续**失败而不是累计失败，因为一次偶发超时不该让端点下线。
//! 3. **半开试探**：全部端点都在冷却时，不直接报错，而是挑一个最快到期的
//!    放行一次。恢复能力比"保守地一直失败"更重要。

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::endpoint::Endpoint;

/// 一个端点的运行状态。
#[derive(Debug, Clone)]
pub struct EndpointStat {
    pub name: String,
    pub model: String,
    pub weight: u32,
    pub enabled: bool,
    pub failures: u32,
    /// 冷却剩余秒数（0 表示健康）。
    pub cooldown_secs: u64,
    pub ok_total: u64,
    pub err_total: u64,
    pub last_error: Option<String>,
}

impl EndpointStat {
    /// 是否健康。
    pub fn healthy(&self) -> bool {
        self.enabled && self.cooldown_secs == 0
    }

    /// 一行描述。
    pub fn render(&self) -> String {
        let state = if !self.enabled {
            "已禁用".to_string()
        } else if self.cooldown_secs > 0 {
            format!("熔断中（{}s 后恢复）", self.cooldown_secs)
        } else {
            "健康".to_string()
        };
        let mut line = format!(
            "{} [{}] 权重{} 成功{} 失败{} {}",
            self.name, self.model, self.weight, self.ok_total, self.err_total, state
        );
        // 排查故障时最想知道的就是"上次为什么挂"，所以别把它藏起来
        if let Some(e) = &self.last_error {
            line.push_str(&format!(" — 最近错误：{e}"));
        }
        line
    }
}

#[derive(Debug, Clone, Default)]
struct State {
    failures: u32,
    cooldown_until: Option<Instant>,
    ok_total: u64,
    err_total: u64,
    last_error: Option<String>,
}

/// 端点调度器（纯逻辑）。
#[derive(Debug)]
pub struct Scheduler {
    endpoints: Vec<Endpoint>,
    states: Vec<State>,
    /// SWRR 的当前权重。
    current: Vec<i64>,
}

impl Scheduler {
    /// 新建。至少需要一个端点。
    pub fn new(endpoints: Vec<Endpoint>) -> crate::error::Result<Self> {
        if endpoints.is_empty() {
            return Err(crate::error::LlmError::Config(
                "至少需要配置一个模型端点".into(),
            ));
        }
        for e in &endpoints {
            e.validate()?;
        }
        let n = endpoints.len();
        Ok(Scheduler {
            endpoints,
            states: vec![State::default(); n],
            current: vec![0; n],
        })
    }

    /// 端点数量。
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// 是否为空（永远为 false）。
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// 端点配置。
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    /// 取某个端点的配置。
    pub fn endpoint(&self, i: usize) -> Option<&Endpoint> {
        self.endpoints.get(i)
    }

    /// 有没有健康的端点。
    pub fn has_healthy(&self, now: Instant) -> bool {
        !self.healthy_indices(now).is_empty()
    }

    fn healthy_indices(&self, now: Instant) -> Vec<usize> {
        self.endpoints
            .iter()
            .enumerate()
            .filter(|(i, e)| {
                e.enabled
                    && self.states[*i]
                        .cooldown_until
                        .map(|t| now >= t)
                        .unwrap_or(true)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// 选一个端点。
    ///
    /// - 优先在健康端点里做 SWRR；
    /// - 全部不健康时进入**半开**：挑一个冷却最快到期的放行一次；
    /// - 全部被显式禁用则返回 `None`。
    pub fn pick(&mut self, now: Instant) -> Option<usize> {
        let healthy = self.healthy_indices(now);
        if healthy.is_empty() {
            return self.half_open(now);
        }

        // SWRR：每个健康端点加上自己的权重，取最大者，再减去权重总和
        let total: i64 = healthy
            .iter()
            .map(|i| self.endpoints[*i].weight as i64)
            .sum();
        for i in &healthy {
            self.current[*i] += self.endpoints[*i].weight as i64;
        }
        let mut best = healthy[0];
        for i in &healthy {
            if self.current[*i] > self.current[best] {
                best = *i;
            }
        }
        self.current[best] -= total;
        Some(best)
    }

    fn half_open(&mut self, now: Instant) -> Option<usize> {
        let mut candidate: Option<(usize, Instant)> = None;
        for (i, e) in self.endpoints.iter().enumerate() {
            if !e.enabled {
                continue;
            }
            let Some(until) = self.states[i].cooldown_until else {
                continue;
            };
            match candidate {
                Some((_, best_until)) if until >= best_until => {}
                _ => candidate = Some((i, until)),
            }
        }
        let (i, _) = candidate?;
        // 清掉冷却，放它试一次；失败会被重新计时
        self.states[i].cooldown_until = None;
        self.states[i].failures = 0;
        let _ = now;
        Some(i)
    }

    /// 记录一次成功。
    pub fn record_success(&mut self, i: usize) {
        if let Some(s) = self.states.get_mut(i) {
            s.failures = 0;
            s.cooldown_until = None;
            s.ok_total += 1;
            s.last_error = None;
        }
    }

    /// 记录一次失败；达到阈值则熔断。
    pub fn record_failure(&mut self, i: usize, now: Instant, err: impl Into<String>) {
        let Some(e) = self.endpoints.get(i) else {
            return;
        };
        let max_failures = e.max_failures.max(1);
        let cooldown = e.cooldown();
        if let Some(s) = self.states.get_mut(i) {
            s.failures += 1;
            s.err_total += 1;
            s.last_error = Some(err.into());
            if s.failures >= max_failures {
                s.cooldown_until = Some(now + cooldown);
            }
        }
    }

    /// 手动清空某个端点的熔断状态。
    pub fn reset(&mut self, i: usize) {
        if let Some(s) = self.states.get_mut(i) {
            *s = State::default();
        }
    }

    /// 全部端点的状态快照。
    pub fn stats(&self, now: Instant) -> Vec<EndpointStat> {
        self.endpoints
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let s = &self.states[i];
                let cooldown = s
                    .cooldown_until
                    .map(|t| {
                        if now >= t {
                            return 0;
                        }
                        // 向上取整到秒：还剩 0.4s 显示 1s（显示 0 会让人以为已经恢复），
                        // 但正好剩 30s 就该是 30s，不能无条件 +1 变成 31s。
                        let left = t - now;
                        left.as_secs() + u64::from(left.subsec_nanos() > 0)
                    })
                    .unwrap_or(0);
                EndpointStat {
                    name: e.name.clone(),
                    model: e.model.clone(),
                    weight: e.weight,
                    enabled: e.enabled,
                    failures: s.failures,
                    cooldown_secs: cooldown,
                    ok_total: s.ok_total,
                    err_total: s.err_total,
                    last_error: s.last_error.clone(),
                }
            })
            .collect()
    }
}

/// 供 `serde` 导出的调度概览。
#[derive(Debug, Clone, Serialize)]
pub struct SchedulerSummary {
    pub endpoints: usize,
    pub healthy: usize,
    pub total_ok: u64,
    pub total_err: u64,
    pub average_latency_ms: u64,
}

impl Scheduler {
    /// 概览（`latency` 由上层传入，调度器不管测量）。
    pub fn summary(&self, now: Instant) -> SchedulerSummary {
        let stats = self.stats(now);
        SchedulerSummary {
            endpoints: stats.len(),
            healthy: stats.iter().filter(|s| s.healthy()).count(),
            total_ok: stats.iter().map(|s| s.ok_total).sum(),
            total_err: stats.iter().map(|s| s.err_total).sum(),
            average_latency_ms: 0,
        }
    }
}

/// 便于测试：把秒换成 `Duration`。
pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(name: &str, weight: u32) -> Endpoint {
        Endpoint::new(name, "http://127.0.0.1:1/v1", "m").with_weight(weight)
    }

    #[test]
    fn smooth_weighted_round_robin_interleaves() {
        let mut s = Scheduler::new(vec![ep("a", 3), ep("b", 1)]).unwrap();
        let now = Instant::now();
        let seq: Vec<usize> = (0..8).map(|_| s.pick(now).unwrap()).collect();
        // 期望 a 出现 3 次、b 出现 1 次，且 b 不会连续被饿死
        let a = seq.iter().filter(|i| **i == 0).count();
        let b = seq.iter().filter(|i| **i == 1).count();
        assert_eq!(a, 6);
        assert_eq!(b, 2);
        // 不能出现连续 4 个 a（那就退化成普通加权轮询了）
        let mut run = 0;
        let mut max_run = 0;
        for i in &seq {
            if *i == 0 {
                run += 1;
                max_run = max_run.max(run);
            } else {
                run = 0;
            }
        }
        assert!(max_run <= 3, "SWRR 不该出现这么长的连击：{seq:?}");
    }

    #[test]
    fn equal_weights_alternate() {
        let mut s = Scheduler::new(vec![ep("a", 1), ep("b", 1), ep("c", 1)]).unwrap();
        let now = Instant::now();
        let seq: Vec<usize> = (0..6).map(|_| s.pick(now).unwrap()).collect();
        assert_eq!(seq, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn circuit_breaker_opens_after_threshold() {
        let mut s = Scheduler::new(vec![ep("a", 1), ep("b", 1)]).unwrap();
        let now = Instant::now();
        // 端点 0 连续失败 3 次（默认阈值）
        for _ in 0..3 {
            s.record_failure(0, now, "boom");
        }
        let stats = s.stats(now);
        assert!(!stats[0].healthy());
        assert_eq!(stats[0].cooldown_secs, 30);
        // 之后只会选中 1
        for _ in 0..5 {
            assert_eq!(s.pick(now).unwrap(), 1);
        }
    }

    #[test]
    fn cooldown_expires_and_endpoint_returns() {
        let mut s = Scheduler::new(vec![ep("a", 1), ep("b", 1)]).unwrap();
        let t0 = Instant::now();
        for _ in 0..3 {
            s.record_failure(0, t0, "boom");
        }
        assert!(!s.stats(t0)[0].healthy());
        // 冷却结束后重新可用
        let later = t0 + Duration::from_secs(31);
        assert!(s.stats(later)[0].healthy());
        let picked: Vec<usize> = (0..4).map(|_| s.pick(later).unwrap()).collect();
        assert!(picked.contains(&0));
    }

    #[test]
    fn success_resets_the_failure_counter() {
        let mut s = Scheduler::new(vec![ep("a", 1)]).unwrap();
        let now = Instant::now();
        s.record_failure(0, now, "e1");
        s.record_failure(0, now, "e2");
        s.record_success(0);
        s.record_failure(0, now, "e3");
        // 只有 1 次连续失败，不该熔断
        assert!(s.stats(now)[0].healthy());
        assert_eq!(s.stats(now)[0].ok_total, 1);
        assert_eq!(s.stats(now)[0].err_total, 3);
    }

    #[test]
    fn half_open_trial_when_everything_is_down() {
        let mut s = Scheduler::new(vec![ep("a", 1), ep("b", 1)]).unwrap();
        let now = Instant::now();
        for _ in 0..3 {
            s.record_failure(0, now, "boom");
            s.record_failure(1, now, "boom");
        }
        assert!(!s.has_healthy(now));
        // 全熔断时仍要放行一次试探，而不是直接报错
        let picked = s.pick(now).expect("应当半开试探");
        assert!(picked < 2);
        // 试探后被放行的那个变成可用状态
        assert!(s.stats(now)[picked].healthy());
    }

    #[test]
    fn disabled_endpoints_are_never_picked() {
        let mut a = ep("a", 1);
        a.enabled = false;
        let mut s = Scheduler::new(vec![a, ep("b", 1)]).unwrap();
        let now = Instant::now();
        for _ in 0..6 {
            assert_eq!(s.pick(now).unwrap(), 1);
        }
        // 全都禁用 → None
        let mut s2 = Scheduler::new(vec![{
            let mut x = ep("x", 1);
            x.enabled = false;
            x
        }])
        .unwrap();
        assert!(s2.pick(now).is_none());
    }

    #[test]
    fn scheduler_requires_valid_endpoints() {
        assert!(Scheduler::new(vec![]).is_err());
        let bad = Endpoint::new("x", "not-a-url", "m");
        assert!(Scheduler::new(vec![bad]).is_err());
    }

    #[test]
    fn summary_counts_healthy_and_totals() {
        let mut s = Scheduler::new(vec![ep("a", 1), ep("b", 1)]).unwrap();
        let now = Instant::now();
        s.record_success(0);
        s.record_failure(1, now, "x");
        let sum = s.summary(now);
        assert_eq!(sum.endpoints, 2);
        assert_eq!(sum.healthy, 2);
        assert_eq!(sum.total_ok, 1);
        assert_eq!(sum.total_err, 1);
    }

    #[test]
    fn stat_rendering_is_informative() {
        let mut s = Scheduler::new(vec![ep("主端点", 1)]).unwrap();
        let now = Instant::now();
        for _ in 0..3 {
            s.record_failure(0, now, "timeout");
        }
        let line = s.stats(now)[0].render();
        assert!(line.contains("主端点"));
        assert!(line.contains("熔断中"));
        assert!(line.contains("timeout"));
    }

    #[test]
    fn reset_clears_state() {
        let mut s = Scheduler::new(vec![ep("a", 1)]).unwrap();
        let now = Instant::now();
        for _ in 0..5 {
            s.record_failure(0, now, "x");
        }
        s.reset(0);
        let st = &s.stats(now)[0];
        assert!(st.healthy());
        assert_eq!(st.err_total, 0);
    }

    #[test]
    fn secs_helper() {
        assert_eq!(secs(3), Duration::from_secs(3));
    }
}
