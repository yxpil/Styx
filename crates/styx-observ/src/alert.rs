//! 告警：阈值 + 持续时间 + 冷却。
//!
//! 三个参数缺一不可，理由分别是：
//!
//! - **阈值**：没有它就没有"不对劲"这个判断；
//! - **持续时间**（`for_secs`）：没有它，一个瞬时抖动就会叫醒人。
//!   连接数在回收的瞬间冲一下是正常的；
//! - **冷却**（`cooldown_secs`）：没有它，一个持续恶化的状态会每分钟
//!   叫一次，直到所有人学会无视它——**会响个不停的告警等于没有告警**。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::log::{Field, Level};
use crate::metrics::Metrics;

/// 比较方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// 大于阈值算越界。
    Above,
    /// 小于阈值算越界。
    Below,
}

impl Comparison {
    /// 解析（`above` / `>` / `below` / `<`）。
    pub fn parse(s: &str) -> Option<Comparison> {
        match s.trim().to_ascii_lowercase().as_str() {
            "above" | ">" | "gt" | "over" => Some(Comparison::Above),
            "below" | "<" | "lt" | "under" => Some(Comparison::Below),
            _ => None,
        }
    }
}

/// 严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// 该看一眼了。
    Warning,
    /// 该起来了。
    Critical,
}

impl Severity {
    /// 短名。
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }

    /// 映射到日志级别。
    pub fn level(self) -> Level {
        match self {
            Severity::Warning => Level::Warn,
            Severity::Critical => Level::Error,
        }
    }

    /// 解析。
    pub fn parse(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "warning" | "warn" => Some(Severity::Warning),
            "critical" | "crit" | "page" => Some(Severity::Critical),
            _ => None,
        }
    }
}

/// 一条告警规则。
#[derive(Debug, Clone)]
pub struct AlertRule {
    /// 规则名（也是状态表的键，必须唯一）。
    pub name: String,
    /// 监控的指标名。
    pub metric: String,
    pub comparison: Comparison,
    pub threshold: f64,
    /// 条件要持续满足多久才真正报出去。
    pub for_secs: u64,
    /// 报过之后多久内不再重复。
    pub cooldown_secs: u64,
    pub severity: Severity,
    /// 一句话说明（会原样出现在告警里）。
    pub summary: String,
}

impl AlertRule {
    /// 造一条规则，其余参数走保守默认（立即触发 + 5 分钟冷却 + 警告级）。
    pub fn new(
        name: impl Into<String>,
        metric: impl Into<String>,
        comparison: Comparison,
        threshold: f64,
    ) -> Self {
        AlertRule {
            name: name.into(),
            metric: metric.into(),
            comparison,
            threshold,
            for_secs: 0,
            cooldown_secs: 300,
            severity: Severity::Warning,
            summary: String::new(),
        }
    }

    /// 要求持续满足。
    pub fn for_secs(mut self, secs: u64) -> Self {
        self.for_secs = secs;
        self
    }

    /// 设置冷却。
    pub fn cooldown_secs(mut self, secs: u64) -> Self {
        self.cooldown_secs = secs;
        self
    }

    /// 设置严重程度。
    pub fn severity(mut self, s: Severity) -> Self {
        self.severity = s;
        self
    }

    /// 设置说明。
    pub fn summary(mut self, s: impl Into<String>) -> Self {
        self.summary = s.into();
        self
    }

    /// 当前值是否越界。
    pub fn breaching(&self, value: f64) -> bool {
        match self.comparison {
            Comparison::Above => value > self.threshold,
            Comparison::Below => value < self.threshold,
        }
    }
}

/// 一条已触发的告警。
#[derive(Debug, Clone)]
pub struct Alert {
    pub rule: String,
    pub severity: Severity,
    pub metric: String,
    pub value: f64,
    pub threshold: f64,
    pub summary: String,
    /// 触发时刻（RFC3339）。
    pub at: String,
}

impl Alert {
    /// 一整句话，供日志和 webhook 共用。
    pub fn render(&self) -> String {
        format!(
            "[{}] {} —— {}={} 越过阈值 {}",
            self.severity.as_str(),
            if self.summary.is_empty() {
                &self.rule
            } else {
                &self.summary
            },
            self.metric,
            self.value,
            self.threshold
        )
    }
}

/// 告警投递端。
pub trait AlertSink: Send + Sync {
    /// 送出一条告警。
    fn deliver(&self, alert: &Alert);

    /// 自己的名字（出现在启动信息里）。
    fn describe(&self) -> String {
        "sink".to_string()
    }
}

/// 把告警写进结构化日志。
///
/// 这是最该先接上的投递端：绝大多数部署都有日志采集，而一条
/// `level=error target=alert` 的记录天然就能被现有的告警通道捞走。
pub struct LogSink;

impl AlertSink for LogSink {
    fn deliver(&self, alert: &Alert) {
        crate::log::emit_with(
            alert.severity.level(),
            "alert",
            &alert.render(),
            &[
                ("rule", Field::Text(alert.rule.clone())),
                ("metric", Field::Text(alert.metric.clone())),
                ("value", Field::Num(alert.value)),
                ("threshold", Field::Num(alert.threshold)),
            ],
        );
    }

    fn describe(&self) -> String {
        "log".to_string()
    }
}

#[derive(Default)]
struct RuleState {
    /// 从什么时候开始持续越界。
    breaching_since: Option<Instant>,
    /// 上次真正报出去的时刻。
    last_fired: Option<Instant>,
}

/// 规则引擎。
pub struct Alerter {
    rules: Vec<AlertRule>,
    state: Mutex<HashMap<String, RuleState>>,
    sinks: Vec<Box<dyn AlertSink>>,
}

impl std::fmt::Debug for Alerter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Alerter")
            .field("rules", &self.rules.len())
            .field(
                "sinks",
                &self.sinks.iter().map(|s| s.describe()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for Alerter {
    fn default() -> Self {
        Self::new()
    }
}

impl Alerter {
    /// 空引擎。
    pub fn new() -> Self {
        Alerter {
            rules: Vec::new(),
            state: Mutex::new(HashMap::new()),
            sinks: Vec::new(),
        }
    }

    /// 带规则。
    pub fn with_rules(rules: Vec<AlertRule>) -> Self {
        Alerter {
            rules,
            state: Mutex::new(HashMap::new()),
            sinks: Vec::new(),
        }
    }

    /// 挂一个投递端。
    pub fn add_sink(&mut self, sink: Box<dyn AlertSink>) {
        self.sinks.push(sink);
    }

    /// 已配置的规则。
    pub fn rules(&self) -> &[AlertRule] {
        &self.rules
    }

    /// 是否配了规则。
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 评估一轮，返回**本轮真正报出去**的告警。
    ///
    /// 返回值的意义是"这次响了"，而不是"这次越界了"——漏报和重复报
    /// 都在这个函数的返回值里体现。
    pub fn evaluate(&self, metrics: &Metrics) -> Vec<Alert> {
        let mut fired = Vec::new();
        let Ok(mut state) = self.state.lock() else {
            return fired;
        };
        let now = Instant::now();

        for rule in &self.rules {
            let Some(value) = metrics.value(&rule.metric) else {
                continue;
            };
            let st = state.entry(rule.name.clone()).or_default();

            if !rule.breaching(value) {
                // 恢复正常：把"持续越界"的计时清零，下次要从头算。
                st.breaching_since = None;
                continue;
            }

            let since = *st.breaching_since.get_or_insert(now);
            if now.duration_since(since) < Duration::from_secs(rule.for_secs) {
                continue;
            }
            if let Some(last) = st.last_fired {
                if now.duration_since(last) < Duration::from_secs(rule.cooldown_secs) {
                    continue;
                }
            }
            st.last_fired = Some(now);

            let alert = Alert {
                rule: rule.name.clone(),
                severity: rule.severity,
                metric: rule.metric.clone(),
                value,
                threshold: rule.threshold,
                summary: rule.summary.clone(),
                at: crate::log::rfc3339_millis(SystemTime::now()),
            };
            for sink in &self.sinks {
                sink.deliver(&alert);
            }
            fired.push(alert);
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct CountingSink(Arc<AtomicUsize>);

    impl AlertSink for CountingSink {
        fn deliver(&self, _alert: &Alert) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn describe(&self) -> String {
            "counting".into()
        }
    }

    fn alerter_with(rule: AlertRule) -> (Alerter, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let mut a = Alerter::with_rules(vec![rule]);
        a.add_sink(Box::new(CountingSink(Arc::clone(&count))));
        (a, count)
    }

    #[test]
    fn fires_once_when_the_threshold_is_crossed() {
        let (a, count) = alerter_with(
            AlertRule::new("busy", "styx_in_flight", Comparison::Above, 10.0).cooldown_secs(60),
        );
        let m = Metrics::new();
        m.set("styx_in_flight", 20.0);
        assert_eq!(a.evaluate(&m).len(), 1);
        // 冷却期内不再响
        assert_eq!(a.evaluate(&m).len(), 0);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fires_again_after_recovering_and_breaching() {
        let (a, _count) = alerter_with(
            AlertRule::new("busy", "styx_in_flight", Comparison::Above, 10.0).cooldown_secs(0),
        );
        let m = Metrics::new();
        m.set("styx_in_flight", 20.0);
        assert_eq!(a.evaluate(&m).len(), 1);
        m.set("styx_in_flight", 1.0);
        assert_eq!(a.evaluate(&m).len(), 0, "恢复时不响");
        m.set("styx_in_flight", 20.0);
        assert_eq!(a.evaluate(&m).len(), 1, "再次越界应当重新响");
    }

    #[test]
    fn for_secs_suppresses_transient_spikes() {
        let (a, _count) = alerter_with(
            AlertRule::new("busy", "styx_in_flight", Comparison::Above, 10.0)
                .for_secs(1)
                .cooldown_secs(0),
        );
        let m = Metrics::new();
        m.set("styx_in_flight", 20.0);
        // 刚越界：还没持续够 1 秒
        assert_eq!(a.evaluate(&m).len(), 0, "瞬时抖动不该报");
        std::thread::sleep(Duration::from_millis(1_050));
        assert_eq!(a.evaluate(&m).len(), 1, "持续够久了就该报");
    }

    #[test]
    fn below_comparison_works() {
        let (a, _count) = alerter_with(
            AlertRule::new("idle", "styx_workers", Comparison::Below, 1.0).cooldown_secs(0),
        );
        let m = Metrics::new();
        m.set("styx_workers", 0.0);
        assert_eq!(a.evaluate(&m).len(), 1);
    }

    #[test]
    fn missing_metrics_are_silently_skipped() {
        let (a, _count) = alerter_with(
            AlertRule::new("nope", "not_a_metric", Comparison::Above, 0.0).cooldown_secs(0),
        );
        let m = Metrics::new();
        assert!(a.evaluate(&m).is_empty(), "指标不存在时不该误报");
    }

    #[test]
    fn counters_can_be_alerted_on_too() {
        let (a, _count) = alerter_with(
            AlertRule::new("rejections", "styx_rejected_total", Comparison::Above, 3.0)
                .cooldown_secs(0),
        );
        let m = Metrics::new();
        m.inc("styx_rejected_total", 4);
        assert_eq!(a.evaluate(&m).len(), 1);
    }

    #[test]
    fn alert_render_is_one_readable_line() {
        let a = Alert {
            rule: "busy".into(),
            severity: Severity::Critical,
            metric: "styx_in_flight".into(),
            value: 99.0,
            threshold: 64.0,
            summary: "连接数打满".into(),
            at: "2026-10-03T00:00:00.000Z".into(),
        };
        assert_eq!(
            a.render(),
            "[critical] 连接数打满 —— styx_in_flight=99 越过阈值 64"
        );
    }
}
