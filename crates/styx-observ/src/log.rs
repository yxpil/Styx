//! 结构化日志。
//!
//! 两种输出：`Pretty`（人看）和 `Json`（机器看）。同一个 `emit` 调用，
//! 换的是渲染，不是信息——这样"开发时看得懂"和"上线后能采集"不必二选一。

use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 日志级别。数字越小越严重，过滤时按"小于等于设定级别"放行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    /// 小写短名（写进日志的那一个）。
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }

    /// 解析（大小写不敏感，支持 `warning` 之类的别名）。
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" | "err" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

/// 输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// 人看的单行文本。
    Pretty,
    /// 一行一个 JSON 对象（JSON Lines），给采集器。
    Json,
}

impl Format {
    /// 解析（`json` / `text`）。
    pub fn parse(s: &str) -> Option<Format> {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" | "jsonl" | "lines" => Some(Format::Json),
            "pretty" | "text" | "human" => Some(Format::Pretty),
            _ => None,
        }
    }
}

/// 一个字段值。只支持这两种是有意的：日志字段要么是给人看的字符串，
/// 要么是给面板画线的数字，其余都是噪音。
#[derive(Debug, Clone)]
pub enum Field {
    Text(String),
    Num(f64),
}

impl Field {
    fn render(&self) -> String {
        match self {
            Field::Text(s) => s.clone(),
            Field::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Field::Text(s) => serde_json::Value::String(s.clone()),
            Field::Num(n) => serde_json::json!(n),
        }
    }
}

/// 日志器。
pub struct Logger {
    level: Level,
    format: Format,
    sink: Mutex<Box<dyn Write + Send>>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger")
            .field("level", &self.level)
            .field("format", &self.format)
            .finish()
    }
}

impl Logger {
    /// 写到标准错误。
    pub fn to_stderr(level: Level, format: Format) -> Self {
        Logger {
            level,
            format,
            sink: Mutex::new(Box::new(io::stderr())),
        }
    }

    /// 写到任意 sink（测试用它抓输出）。
    pub fn with_sink(level: Level, format: Format, sink: Box<dyn Write + Send>) -> Self {
        Logger {
            level,
            format,
            sink: Mutex::new(sink),
        }
    }

    /// 当前级别。
    pub fn level(&self) -> Level {
        self.level
    }

    /// 当前格式。
    pub fn format(&self) -> Format {
        self.format
    }

    /// 这个级别会不会被输出。
    pub fn enabled(&self, level: Level) -> bool {
        level <= self.level
    }

    /// 写一条。
    pub fn emit(&self, level: Level, target: &str, message: &str) {
        self.emit_with(level, target, message, &[]);
    }

    /// 写一条带字段的。
    pub fn emit_with(&self, level: Level, target: &str, message: &str, fields: &[(&str, Field)]) {
        if !self.enabled(level) {
            return;
        }
        let now = SystemTime::now();
        let line = match self.format {
            Format::Pretty => {
                let mut s = format!(
                    "{} {:<5} {:<10} {}",
                    rfc3339_millis(now),
                    level.as_str().to_ascii_uppercase(),
                    target,
                    message
                );
                for (k, v) in fields {
                    s.push_str(&format!(" {k}={}", v.render()));
                }
                s
            }
            Format::Json => {
                let mut obj = serde_json::Map::new();
                obj.insert("ts".into(), serde_json::json!(rfc3339_millis(now)));
                obj.insert("level".into(), serde_json::json!(level.as_str()));
                obj.insert("target".into(), serde_json::json!(target));
                obj.insert("msg".into(), serde_json::json!(message));
                for (k, v) in fields {
                    obj.insert((*k).to_string(), v.to_json());
                }
                serde_json::Value::Object(obj).to_string()
            }
        };

        if let Ok(mut w) = self.sink.lock() {
            let _ = writeln!(w, "{line}");
        }
    }
}

// ------------------------------------------------------------------ 全局实例

static GLOBAL: OnceLock<Logger> = OnceLock::new();

/// 安装全局日志器。重复调用只有第一次生效——日志器是进程级的，
/// 半路换一个会让"前一半日志去哪了"变成一个新谜题。
pub fn init(level: Level, format: Format) -> &'static Logger {
    GLOBAL.get_or_init(|| Logger::to_stderr(level, format))
}

/// 已安装的全局日志器（没装过则 `None`）。
pub fn global() -> Option<&'static Logger> {
    GLOBAL.get()
}

/// 向全局日志器写一条；没装过就静默丢弃。
pub fn emit(level: Level, target: &str, message: &str) {
    if let Some(l) = GLOBAL.get() {
        l.emit(level, target, message);
    }
}

/// 向全局日志器写一条带字段的；没装过就静默丢弃。
pub fn emit_with(level: Level, target: &str, message: &str, fields: &[(&str, Field)]) {
    if let Some(l) = GLOBAL.get() {
        l.emit_with(level, target, message, fields);
    }
}

/// 这个级别会不会被输出（没装日志器时为 `false`）。
pub fn enabled(level: Level) -> bool {
    GLOBAL.get().map(|l| l.enabled(level)).unwrap_or(false)
}

// ---------------------------------------------------------------------- 宏

/// 错误级日志。用法：`log_error!("server", "连接失败：{}", e)`。
#[macro_export]
macro_rules! log_error {
    ($target:expr, $($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Error, $target, &format!($($arg)*))
    };
}

/// 警告级日志。
#[macro_export]
macro_rules! log_warn {
    ($target:expr, $($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Warn, $target, &format!($($arg)*))
    };
}

/// 信息级日志。
#[macro_export]
macro_rules! log_info {
    ($target:expr, $($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Info, $target, &format!($($arg)*))
    };
}

/// 调试级日志。参数里的表达式只有在级别放行时才会求值。
#[macro_export]
macro_rules! log_debug {
    ($target:expr, $($arg:tt)*) => {
        if $crate::log::enabled($crate::log::Level::Debug) {
            $crate::log::emit($crate::log::Level::Debug, $target, &format!($($arg)*))
        }
    };
}

// ------------------------------------------------------------------ 时间格式

/// `Unix 秒 + 毫秒` 的 `SystemTime` → `2026-10-03T05:20:00.123Z`。
///
/// 手写而不是引 `chrono`：只需要 UTC、只需要这一种格式，
/// 而日期换算就是十来行整数运算。
pub fn rfc3339_millis(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, mo, da) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// 自 1970-01-01 起的天数 → `(年, 月, 日)`。
///
/// Howard Hinnant 的 `civil_from_days`：把纪元挪到 3 月 1 日，
/// 让闰日落在年末，于是"年"可以整段整段地算。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn logger(level: Level, format: Format) -> (Logger, Capture) {
        let cap = Capture::default();
        let l = Logger::with_sink(level, format, Box::new(cap.clone()));
        (l, cap)
    }

    #[test]
    fn level_parsing_accepts_aliases() {
        assert_eq!(Level::parse("WARN"), Some(Level::Warn));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse(" err "), Some(Level::Error));
        assert_eq!(Level::parse("chatty"), None);
        assert_eq!(Format::parse("jsonl"), Some(Format::Json));
        assert_eq!(Format::parse("text"), Some(Format::Pretty));
    }

    #[test]
    fn level_ordering_is_severity_first() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        // 设成 warn 时，error 和 warn 放行，info 及以上被挡
        let l = Logger::to_stderr(Level::Warn, Format::Pretty);
        assert!(l.enabled(Level::Error));
        assert!(l.enabled(Level::Warn));
        assert!(!l.enabled(Level::Info));
    }

    #[test]
    fn pretty_line_carries_level_target_and_fields() {
        let (l, cap) = logger(Level::Info, Format::Pretty);
        l.emit_with(
            Level::Info,
            "server",
            "连接建立",
            &[
                ("peer", Field::Text("127.0.0.1:5000".into())),
                ("n", Field::Num(3.0)),
            ],
        );
        let out = cap.text();
        assert!(out.contains("INFO"), "{out}");
        assert!(out.contains("server"), "{out}");
        assert!(out.contains("连接建立"), "{out}");
        assert!(out.contains("peer=127.0.0.1:5000"), "{out}");
        assert!(out.contains("n=3"), "整数不该带小数点：{out}");
    }

    #[test]
    fn json_line_is_one_parseable_object() {
        let (l, cap) = logger(Level::Debug, Format::Json);
        l.emit_with(
            Level::Warn,
            "llm",
            "端点降级",
            &[
                ("endpoint", Field::Text("a".into())),
                ("ms", Field::Num(12.5)),
            ],
        );
        let out = cap.text();
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("应当是一行 JSON");
        assert_eq!(v["level"], "warn");
        assert_eq!(v["target"], "llm");
        assert_eq!(v["msg"], "端点降级");
        assert_eq!(v["endpoint"], "a");
        assert_eq!(v["ms"], 12.5);
        assert!(v["ts"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn filtered_lines_produce_no_output() {
        let (l, cap) = logger(Level::Warn, Format::Pretty);
        l.emit(Level::Info, "server", "不该出现");
        l.emit(Level::Debug, "server", "也不该");
        assert_eq!(cap.text(), "");
        l.emit(Level::Error, "server", "该出现");
        assert!(cap.text().contains("该出现"));
    }

    #[test]
    fn timestamp_formatting_round_trips_known_instants() {
        // 1759464000 秒 = 20364 天 + 4 小时；20364 天落在 2025-10-03。
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_759_464_000_123);
        assert_eq!(rfc3339_millis(t), "2025-10-03T04:00:00.123Z");
        assert_eq!(rfc3339_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 闰年 2 月 29 日
        let leap = UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800);
        assert_eq!(rfc3339_millis(leap), "2024-02-29T00:00:00.000Z");
    }
}
