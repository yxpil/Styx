//! # styx-observ — 可观测性
//!
//! 三件套，都是"出了问题才想起要"的东西：
//!
//! - [`log`]：结构化日志。分级、带目标、可输出人类可读或 JSON Lines。
//! - [`metrics`]：计数器 / 仪表 / 直方图，导出 Prometheus 文本格式。
//! - [`alert`]：阈值 + 持续时间 + 冷却的告警规则，可挂多个投递端。
//!
//! ## 为什么不用 `tracing` + `tracing-subscriber`
//!
//! 这个项目的网络栈、TLS、HTTP 全是手写的，日志的消费方是自己的启动脚本
//! 和采集器，不需要生态互操作。换来的是**零新增依赖**：一个 `Mutex` 包着
//! 一个 `Write`，加上几十行时间格式化。
//!
//! ## 为什么告警要单独一层
//!
//! "有指标"和"会告警"是两件事。指标是**事实**，告警是**判断**——
//! 判断需要阈值、需要"持续多久才算数"、需要"别一直响"。把判断写死在
//! 采集器里，等于把业务语义外包出去；写在这里，改一条规则不必动部署。

pub mod alert;
pub mod log;
pub mod metrics;

pub use alert::{Alert, AlertRule, AlertSink, Alerter, Comparison, LogSink, Severity};
pub use log::{Format, Level, Logger};
pub use metrics::Metrics;

/// 初始化全局日志器（重复调用是安全的，只有第一次生效）。
pub fn init(level: Level, format: Format) -> &'static Logger {
    log::init(level, format)
}
