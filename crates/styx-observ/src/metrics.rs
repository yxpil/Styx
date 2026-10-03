//! 指标：计数器 / 仪表 / 直方图，导出 Prometheus 文本格式。
//!
//! 不引 `prometheus` crate：要导出的就是几个数字和一张分桶表，
//! 而文本格式本身短到可以照着写（见 [`Metrics::render`] 的注释）。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 默认分桶：从 5 毫秒到 2 分钟。
///
/// 上界刻意留到 120 秒——这个产品的"一次请求"里包含一次完整的模型回合，
/// 用 Prometheus 那套面向 HTTP 的默认分桶（最大 10 秒）会让所有样本
/// 全挤进 `+Inf`，直方图就白做了。
pub const DEFAULT_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// 一张直方图。
#[derive(Debug, Clone)]
struct Histogram {
    buckets: Vec<f64>,
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    fn new(buckets: Vec<f64>) -> Self {
        let counts = vec![0; buckets.len()];
        Histogram {
            buckets,
            counts,
            sum: 0.0,
            count: 0,
        }
    }

    fn observe(&mut self, v: f64) {
        self.sum += v;
        self.count += 1;
        // 累加式分桶：每个桶存的是"小于等于该上界"的累计数，
        // 这正是 Prometheus 的 `_bucket{le=...}` 语义。
        for (i, upper) in self.buckets.iter().enumerate() {
            if v <= *upper {
                self.counts[i] += 1;
            }
        }
    }
}

#[derive(Default)]
struct Inner {
    counters: BTreeMap<String, u64>,
    gauges: BTreeMap<String, f64>,
    histograms: BTreeMap<String, Histogram>,
    help: BTreeMap<String, String>,
}

/// 指标登记处。克隆它只是复制一个 `Arc`，可以随手分发给各个线程。
#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Mutex<Inner>>,
    started: Instant,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics")
            .field("series", &self.series_count())
            .finish()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// 新建。
    pub fn new() -> Self {
        Metrics {
            inner: Arc::new(Mutex::new(Inner::default())),
            started: Instant::now(),
        }
    }

    /// 记一句说明，会出现在导出结果的 `# HELP` 行里。
    pub fn describe(&self, name: &str, help: &str) {
        if !is_valid_name(name) {
            return;
        }
        if let Ok(mut i) = self.inner.lock() {
            i.help.insert(name.to_string(), help.to_string());
        }
    }

    /// 计数器 +n。
    pub fn inc(&self, name: &str, by: u64) {
        if !is_valid_name(name) {
            return;
        }
        if let Ok(mut i) = self.inner.lock() {
            *i.counters.entry(name.to_string()).or_insert(0) += by;
        }
    }

    /// 仪表直接赋值。
    pub fn set(&self, name: &str, value: f64) {
        if !is_valid_name(name) {
            return;
        }
        if let Ok(mut i) = self.inner.lock() {
            i.gauges.insert(name.to_string(), value);
        }
    }

    /// 直方图观察一个值（单位是秒）。
    pub fn observe(&self, name: &str, seconds: f64) {
        self.observe_with_buckets(name, seconds, &DEFAULT_BUCKETS);
    }

    /// 直方图观察一个值，指定分桶。
    pub fn observe_with_buckets(&self, name: &str, seconds: f64, buckets: &[f64]) {
        if !is_valid_name(name) {
            return;
        }
        let Ok(mut i) = self.inner.lock() else {
            return;
        };
        let h = i
            .histograms
            .entry(name.to_string())
            .or_insert_with(|| Histogram::new(buckets.to_vec()));
        h.observe(seconds);
    }

    /// 读一个计数器。
    pub fn counter(&self, name: &str) -> u64 {
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.counters.get(name).copied())
            .unwrap_or(0)
    }

    /// 读一个仪表。
    pub fn gauge(&self, name: &str) -> f64 {
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.gauges.get(name).copied())
            .unwrap_or(0.0)
    }

    /// 按名字取值：仪表优先，其次计数器，再次直方图的样本数。
    ///
    /// 告警规则不该关心"这个数是怎么攒出来的"——一次阈值比较对三种
    /// 类型是同一件事，把类型差异推给写规则的人，只会让规则变得难写。
    pub fn value(&self, name: &str) -> Option<f64> {
        let i = self.inner.lock().ok()?;
        if let Some(v) = i.gauges.get(name) {
            return Some(*v);
        }
        if let Some(v) = i.counters.get(name) {
            return Some(*v as f64);
        }
        i.histograms.get(name).map(|h| h.count as f64)
    }

    /// 读一个直方图的样本数。
    pub fn histogram_count(&self, name: &str) -> u64 {
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.histograms.get(name).map(|h| h.count))
            .unwrap_or(0)
    }

    /// 当前登记的序列数。
    pub fn series_count(&self) -> usize {
        self.inner
            .lock()
            .map(|i| i.counters.len() + i.gauges.len() + i.histograms.len())
            .unwrap_or(0)
    }

    /// 启动至今的秒数。
    pub fn uptime_seconds(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// 导出 Prometheus 文本格式。
    ///
    /// 格式本身很短：每种指标先 `# TYPE` 声明类型（可选 `# HELP`），
    /// 然后一行一个样本。直方图要额外吐 `_bucket{le=…}`、`_sum`、`_count`，
    /// 且 `le` 桶必须是累计值——这些细节就是这里全部的复杂度。
    pub fn render(&self) -> String {
        let Ok(i) = self.inner.lock() else {
            return String::new();
        };
        let mut out = String::new();

        for (name, value) in &i.counters {
            header(
                &mut out,
                name,
                "counter",
                i.help.get(name).map(String::as_str),
            );
            out.push_str(&format!("{name} {value}\n"));
        }
        for (name, value) in &i.gauges {
            header(
                &mut out,
                name,
                "gauge",
                i.help.get(name).map(String::as_str),
            );
            out.push_str(&format!("{name} {}\n", fmt_float(*value)));
        }
        for (name, h) in &i.histograms {
            header(
                &mut out,
                name,
                "histogram",
                i.help.get(name).map(String::as_str),
            );
            for (idx, upper) in h.buckets.iter().enumerate() {
                out.push_str(&format!(
                    "{}_bucket{{le=\"{}\"}} {}\n",
                    name,
                    fmt_float(*upper),
                    h.counts[idx]
                ));
            }
            // `+Inf` 桶等于总样本数，是 Prometheus 的硬性约定。
            out.push_str(&format!("{}_bucket{{le=\"+Inf\"}} {}\n", name, h.count));
            out.push_str(&format!("{}_sum {}\n", name, fmt_float(h.sum)));
            out.push_str(&format!("{}_count {}\n", name, h.count));
        }

        // 自带的 uptime：连"进程活了多久"都要靠外部计时器猜，是件很别扭的事。
        header(
            &mut out,
            "styx_uptime_seconds",
            "gauge",
            Some("进程已运行时长"),
        );
        out.push_str(&format!(
            "styx_uptime_seconds {}\n",
            fmt_float(self.started.elapsed().as_secs_f64())
        ));
        out
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: Option<&str>) {
    if let Some(h) = help {
        out.push_str(&format!("# HELP {name} {h}\n"));
    }
    out.push_str(&format!("# TYPE {name} {kind}\n"));
}

/// Prometheus 的指标名规则：`[a-zA-Z_:][a-zA-Z0-9_:]*`。
///
/// 不做替换而是**直接丢弃**非法名：一个悄悄被改名的指标，比一个
/// 从头就没出现的指标更难查——面板上少一条线，至少能立刻发现。
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// 浮点格式化：整数不带小数点，其余尽量短。
fn fmt_float(v: f64) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_for(text: &str, name: &str) -> String {
        text.lines()
            .find(|l| l.starts_with(&format!("{name} ")))
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn counters_accumulate_and_gauges_overwrite() {
        let m = Metrics::new();
        m.inc("styx_turns_total", 1);
        m.inc("styx_turns_total", 2);
        assert_eq!(m.counter("styx_turns_total"), 3);

        m.set("styx_in_flight", 5.0);
        m.set("styx_in_flight", 2.0);
        assert_eq!(m.gauge("styx_in_flight"), 2.0);
    }

    #[test]
    fn render_emits_type_and_help_lines() {
        let m = Metrics::new();
        m.describe("styx_turns_total", "已完成的回合数");
        m.inc("styx_turns_total", 7);
        let out = m.render();
        assert!(
            out.contains("# HELP styx_turns_total 已完成的回合数"),
            "{out}"
        );
        assert!(out.contains("# TYPE styx_turns_total counter"), "{out}");
        assert_eq!(line_for(&out, "styx_turns_total"), "styx_turns_total 7");
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_end_with_inf() {
        let m = Metrics::new();
        for v in [0.01, 0.3, 0.7, 2.0] {
            m.observe("styx_turn_seconds", v);
        }
        let out = m.render();
        // 累计语义：le=0.5 收到 0.01 和 0.3；le=10 收到全部四个
        assert!(
            out.contains("styx_turn_seconds_bucket{le=\"0.5\"} 2"),
            "{out}"
        );
        assert!(
            out.contains("styx_turn_seconds_bucket{le=\"10\"} 4"),
            "{out}"
        );
        assert!(
            out.contains("styx_turn_seconds_bucket{le=\"+Inf\"} 4"),
            "{out}"
        );
        assert_eq!(
            line_for(&out, "styx_turn_seconds_count"),
            "styx_turn_seconds_count 4"
        );
        assert_eq!(
            line_for(&out, "styx_turn_seconds_sum"),
            "styx_turn_seconds_sum 3.01"
        );
    }

    #[test]
    fn invalid_metric_names_are_dropped_not_renamed() {
        let m = Metrics::new();
        m.inc("has space", 1);
        m.inc("1starts_with_digit", 1);
        m.inc("valid_name", 1);
        assert_eq!(m.series_count(), 1, "非法名应当被丢弃");
        assert_eq!(m.counter("valid_name"), 1);
        assert!(!m.render().contains("has space"));
    }

    #[test]
    fn uptime_is_reported() {
        let m = Metrics::new();
        let out = m.render();
        assert!(out.contains("# TYPE styx_uptime_seconds gauge"), "{out}");
        assert!(line_for(&out, "styx_uptime_seconds").starts_with("styx_uptime_seconds "));
    }

    #[test]
    fn clones_share_one_registry() {
        let a = Metrics::new();
        let b = a.clone();
        a.inc("shared_total", 4);
        assert_eq!(b.counter("shared_total"), 4);
    }
}
