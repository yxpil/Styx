//! 内置工具：不依赖任何外部服务的确定性小工具。
//!
//! 这些工具的作用不是"很强大"，而是**让角色能作用于世界**：
//! 掷一次骰子决定成败、看一眼时钟知道是几点、把一件约定写进共享记忆、
//! 或者从记忆里翻出一件旧事。它们同时也是 MCP / BIT 工具接入的**参照实现**——
//! 工具契约有多简单，看这个文件就够了。
//!
//! # 关于"随机"
//!
//! 为了不引入 `rand` 依赖（内核的兜底路径要保持零依赖），这里用了一个
//! xorshift64* 伪随机数发生器，种子来自系统时间。
//! **它不是密码学安全的**，只用于"掷骰子决定剧情走向"这类场合。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use styx_core::error::{Result, StyxError};
use styx_core::ports::{AssocPort, MemoryNote, MemoryPort, PoolPort, ToolSpec};

use crate::registry::Tool;

// ------------------------------------------------------------------ 伪随机数

/// 一个够用的伪随机数发生器（xorshift64*）。
pub struct SmallRng(u64);

impl SmallRng {
    /// 用系统时间做种。
    pub fn from_entropy() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545_F491_4F6C_DD1D);
        // 与栈地址混合，避免同一纳秒内的两次构造得到同一个种子
        let addr = &nanos as *const u64 as u64;
        SmallRng::seeded(nanos ^ addr.rotate_left(17))
    }

    /// 指定种子（测试用，保证可复现）。
    pub fn seeded(seed: u64) -> Self {
        // 定状态前必须先混合一次。
        //
        // 原本这里是 `seed | 1`，于是相邻种子会塌到同一个状态：
        // `42 | 1 == 43 | 1 == 43`——"换个种子"实际上什么都没换。
        // 用 splitmix64 的收尾函数把种子打散。
        let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        s = (s ^ (s >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        s = (s ^ (s >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        s ^= s >> 31;
        SmallRng(s | 1)
    }

    /// 下一个 u64。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `[0, n)` 内的整数。
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next_u64() % n
    }

    /// `[0, 1)` 内的浮点。
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// -------------------------------------------------------------------- 时钟

/// 取当前时间。
pub struct ClockTool;

impl Tool for ClockTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("clock", "获取当前时间（Unix 毫秒 + UTC 可读时间）")
    }

    fn invoke(&self, _args: Value) -> Result<Value> {
        let ms = now_millis();
        Ok(json!({
            "unix_ms": ms,
            "utc": format_utc(ms),
        }))
    }
}

/// 当前 Unix 毫秒。
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 把 Unix 毫秒格式化成 `YYYY-MM-DD HH:MM:SS UTC`。
pub fn format_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

/// Howard Hinnant 的 `civil_from_days`。
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

// -------------------------------------------------------------------- 骰子

/// 掷骰子。
pub struct DiceTool;

impl Tool for DiceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("dice", "掷骰子：指定面数与个数，返回每颗点数与总和")
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "sides": {"type": "integer", "minimum": 2, "default": 6},
                    "count": {"type": "integer", "minimum": 1, "maximum": 100, "default": 1}
                }
            }))
    }

    fn invoke(&self, args: Value) -> Result<Value> {
        let sides = args
            .get("sides")
            .and_then(|v| v.as_u64())
            .unwrap_or(6)
            .clamp(2, 1000);
        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .clamp(1, 100);
        let mut rng = SmallRng::from_entropy();
        let rolls: Vec<u64> = (0..count).map(|_| rng.below(sides) + 1).collect();
        let total: u64 = rolls.iter().sum();
        Ok(json!({
            "sides": sides,
            "count": count,
            "rolls": rolls,
            "total": total,
        }))
    }
}

// -------------------------------------------------------------- 随机挑选

/// 从候选里挑一个（可用于"剧情往哪走"）。
pub struct PickTool;

impl Tool for PickTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("pick", "从候选列表里随机挑一个（可带权重）")
            .with_schema(json!({
                "type": "object",
                "required": ["options"],
                "properties": {
                    "options": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                {"type": "string"},
                                {"type": "object", "properties": {
                                    "value": {"type": "string"},
                                    "weight": {"type": "number"}
                                }}
                            ]
                        }
                    }
                }
            }))
    }

    fn invoke(&self, args: Value) -> Result<Value> {
        let arr = args
            .get("options")
            .and_then(|v| v.as_array())
            .ok_or_else(|| StyxError::ToolArgs("pick 需要 options 数组".into()))?;
        if arr.is_empty() {
            return Err(StyxError::ToolArgs("options 不能为空".into()));
        }
        let mut items: Vec<(String, f64)> = Vec::new();
        for o in arr {
            match o {
                Value::String(s) => items.push((s.clone(), 1.0)),
                Value::Object(m) => {
                    let v = m
                        .get("value")
                        .and_then(|x| x.as_str())
                        .ok_or_else(|| StyxError::ToolArgs("option.value 必须是字符串".into()))?;
                    let w = m.get("weight").and_then(|x| x.as_f64()).unwrap_or(1.0);
                    items.push((v.to_string(), w.max(0.0)));
                }
                _ => return Err(StyxError::ToolArgs("option 必须是字符串或对象".into())),
            }
        }
        let total: f64 = items.iter().map(|(_, w)| *w).sum();
        if total <= 0.0 {
            return Err(StyxError::ToolArgs("权重总和必须大于 0".into()));
        }
        let mut rng = SmallRng::from_entropy();
        let mut target = rng.unit() * total;
        for (value, w) in &items {
            if target < *w {
                return Ok(json!({ "picked": value, "weight": w }));
            }
            target -= *w;
        }
        Ok(json!({ "picked": items.last().unwrap().0, "weight": items.last().unwrap().1 }))
    }
}

// ------------------------------------------------------------ 记忆相关工具

/// 从长期记忆里检索。
pub struct RecallTool {
    memory: Arc<dyn MemoryPort>,
}

impl RecallTool {
    pub fn new(memory: Arc<dyn MemoryPort>) -> Self {
        RecallTool { memory }
    }
}

impl Tool for RecallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("recall", "从长期记忆里检索与查询最相关的记忆")
            .with_schema(json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 5}
                }
            }))
            .with_origin("builtin:memory")
    }

    fn invoke(&self, args: Value) -> Result<Value> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| StyxError::ToolArgs("recall 需要 query".into()))?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
        let hits = self.memory.recall(query, limit)?;
        Ok(json!({
            "count": hits.len(),
            "memories": hits.iter().map(|h| json!({
                "id": h.id,
                "text": h.text,
                "score": h.score,
                "importance": h.importance,
                "tags": h.tags,
                "origin": h.origin,
            })).collect::<Vec<_>>()
        }))
    }
}

/// 从联想后端发散。
pub struct AssociateTool {
    assoc: Arc<dyn AssocPort>,
}

impl AssociateTool {
    pub fn new(assoc: Arc<dyn AssocPort>) -> Self {
        AssociateTool { assoc }
    }
}

impl Tool for AssociateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("associate", "由一个词发散出相关联的词，附带证据与置信度")
            .with_schema(json!({
                "type": "object",
                "required": ["seed"],
                "properties": {
                    "seed": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 30, "default": 6}
                }
            }))
            .with_origin("builtin:assoc")
    }

    fn invoke(&self, args: Value) -> Result<Value> {
        let seed = args
            .get("seed")
            .and_then(|v| v.as_str())
            .ok_or_else(|| StyxError::ToolArgs("associate 需要 seed".into()))?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(6) as usize;
        let list = self.assoc.associate(seed, limit)?;
        Ok(json!({
            "count": list.len(),
            "associations": list.iter().map(|a| json!({
                "word": a.word,
                "score": a.score,
                "confidence": a.confidence,
                "evidence": a.evidence,
            })).collect::<Vec<_>>()
        }))
    }
}

/// 往共享记忆池写一条事实。
pub struct NoteTool {
    pool: Arc<dyn PoolPort>,
}

impl NoteTool {
    pub fn new(pool: Arc<dyn PoolPort>) -> Self {
        NoteTool { pool }
    }
}

impl Tool for NoteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("note", "把一条事实写进共享记忆池，供其他 agent 读取")
            .with_schema(json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "importance": {"type": "number", "minimum": 0, "maximum": 1}
                }
            }))
            .with_origin("builtin:pool")
    }

    fn invoke(&self, args: Value) -> Result<Value> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| StyxError::ToolArgs("note 需要 text".into()))?;
        let tags: Vec<String> = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let importance = args
            .get("importance")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5) as f32;
        let note = MemoryNote::new(text)
            .with_tags(tags)
            .with_importance(importance)
            .with_source("styx:tool");
        let id = self.pool.remember(&note)?;
        Ok(json!({ "stored": true, "id": id }))
    }
}

/// 一次性把标准内置工具注册进注册表。
///
/// 只会注册**能工作**的工具：没有任何后端时，`recall` / `associate` / `note`
/// 不会被注册——让模型看见一个必然失败的工具，比看不见它更糟。
pub fn register_builtins(
    registry: &mut crate::registry::ToolRegistry,
    memory: Option<Arc<dyn MemoryPort>>,
    assoc: Option<Arc<dyn AssocPort>>,
    pool: Option<Arc<dyn PoolPort>>,
) {
    registry.register(ClockTool);
    registry.register(DiceTool);
    registry.register(PickTool);
    if let Some(m) = memory {
        registry.register(RecallTool::new(m));
    }
    if let Some(a) = assoc {
        registry.register(AssociateTool::new(a));
    }
    if let Some(p) = pool {
        registry.register(NoteTool::new(p));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolRegistry;
    use styx_core::ports::{Recalled, ToolPort};

    #[test]
    fn small_rng_is_deterministic_with_a_seed() {
        let mut a = SmallRng::seeded(42);
        let mut b = SmallRng::seeded(42);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = SmallRng::seeded(43);
        assert_ne!(SmallRng::seeded(42).next_u64(), c.next_u64());
    }

    #[test]
    fn small_rng_below_is_bounded() {
        let mut r = SmallRng::seeded(7);
        for _ in 0..200 {
            let v = r.below(6);
            assert!(v < 6);
        }
        assert_eq!(r.below(0), 0);
        for _ in 0..50 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
        }
    }

    #[test]
    fn clock_formats_epoch_and_now() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(
            format_utc(1_767_225_600_000),
            "2026-01-01 00:00:00 UTC"
        );
        let v = ClockTool.invoke(json!({})).unwrap();
        assert!(v["unix_ms"].as_i64().unwrap() > 1_700_000_000_000);
        assert!(v["utc"].as_str().unwrap().ends_with("UTC"));
    }

    #[test]
    fn dice_respects_bounds() {
        let v = DiceTool.invoke(json!({"sides": 6, "count": 20})).unwrap();
        let rolls = v["rolls"].as_array().unwrap();
        assert_eq!(rolls.len(), 20);
        for r in rolls {
            let n = r.as_u64().unwrap();
            assert!((1..=6).contains(&n), "{n}");
        }
        let sum: u64 = rolls.iter().map(|r| r.as_u64().unwrap()).sum();
        assert_eq!(v["total"].as_u64().unwrap(), sum);
    }

    #[test]
    fn dice_clamps_extreme_arguments() {
        let v = DiceTool.invoke(json!({"sides": 1, "count": 9999})).unwrap();
        assert_eq!(v["sides"], 2);
        assert_eq!(v["count"], 100);
    }

    #[test]
    fn pick_returns_one_of_the_options() {
        let v = PickTool
            .invoke(json!({"options": ["a", "b", "c"]}))
            .unwrap();
        let picked = v["picked"].as_str().unwrap();
        assert!(["a", "b", "c"].contains(&picked));
    }

    #[test]
    fn pick_honours_weights() {
        // 权重全压在一项上时，结果必须确定
        let v = PickTool
            .invoke(json!({"options": [
                {"value":"必然","weight":1.0},
                {"value":"不可能","weight":0.0}
            ]}))
            .unwrap();
        assert_eq!(v["picked"], "必然");
    }

    #[test]
    fn pick_validates_input() {
        assert!(PickTool.invoke(json!({})).is_err());
        assert!(PickTool.invoke(json!({"options": []})).is_err());
        assert!(PickTool
            .invoke(json!({"options": [{"weight": 1}]}))
            .is_err());
        assert!(PickTool
            .invoke(json!({"options": [{"value":"x","weight":0}]}))
            .is_err());
        assert!(PickTool.invoke(json!({"options": [42]})).is_err());
    }

    // ---- 记忆相关工具用假的端口 ----

    struct FakeMemory;
    impl MemoryPort for FakeMemory {
        fn name(&self) -> &str {
            "fake"
        }
        fn remember(&self, _n: &MemoryNote) -> Result<String> {
            Ok("1".into())
        }
        fn recall(&self, q: &str, limit: usize) -> Result<Vec<Recalled>> {
            Ok(vec![Recalled::new(
                "1",
                format!("关于 {q} 的记忆"),
                0.9,
                "fake",
            )]
            .into_iter()
            .take(limit)
            .collect())
        }
        fn related(&self, _id: &str, _l: usize) -> Result<Vec<Recalled>> {
            Ok(Vec::new())
        }
        fn forget(&self, _id: &str) -> Result<bool> {
            Ok(true)
        }
    }

    struct FakeAssoc;
    impl AssocPort for FakeAssoc {
        fn name(&self) -> &str {
            "fake"
        }
        fn associate(&self, seed: &str, _limit: usize) -> Result<Vec<styx_core::ports::Association>> {
            Ok(vec![styx_core::ports::Association::new(
                format!("{seed}的邻居"),
                0.7,
            )
            .with_confidence(0.8)
            .with_evidence(["某句话"])])
        }
    }

    struct FakePool;
    impl PoolPort for FakePool {
        fn name(&self) -> &str {
            "fake"
        }
        fn remember(&self, _n: &MemoryNote) -> Result<String> {
            Ok("pool-1".into())
        }
        fn recall(&self, _q: &str, _l: usize) -> Result<Vec<Recalled>> {
            Ok(Vec::new())
        }
        fn stats(&self) -> Result<styx_core::ports::PoolStats> {
            Ok(Default::default())
        }
    }

    #[test]
    fn recall_tool_returns_memories() {
        let t = RecallTool::new(Arc::new(FakeMemory));
        let v = t.invoke(json!({"query":"旧照片","limit":3})).unwrap();
        assert_eq!(v["count"], 1);
        assert!(v["memories"][0]["text"].as_str().unwrap().contains("旧照片"));
        assert!(t.invoke(json!({})).is_err());
    }

    #[test]
    fn associate_tool_returns_evidence() {
        let t = AssociateTool::new(Arc::new(FakeAssoc));
        let v = t.invoke(json!({"seed":"照片"})).unwrap();
        assert_eq!(v["associations"][0]["word"], "照片的邻居");
        assert_eq!(v["associations"][0]["evidence"][0], "某句话");
        assert!(t.invoke(json!({})).is_err());
    }

    #[test]
    fn note_tool_writes_to_pool() {
        let t = NoteTool::new(Arc::new(FakePool));
        let v = t
            .invoke(json!({"text":"陈默今晚在城南","tags":["约定"],"importance":0.9}))
            .unwrap();
        assert_eq!(v["stored"], true);
        assert_eq!(v["id"], "pool-1");
        assert!(t.invoke(json!({})).is_err());
    }

    #[test]
    fn register_builtins_only_adds_working_tools() {
        // 全都没有 → 只注册三个纯本地工具
        let mut r = ToolRegistry::new();
        register_builtins(&mut r, None, None, None);
        assert_eq!(r.len(), 3);
        assert!(r.contains("clock") && r.contains("dice") && r.contains("pick"));
        assert!(!r.contains("recall"));

        // 有后端 → 相关工具也进来
        let mut r = ToolRegistry::new();
        register_builtins(
            &mut r,
            Some(Arc::new(FakeMemory)),
            Some(Arc::new(FakeAssoc)),
            Some(Arc::new(FakePool)),
        );
        assert_eq!(r.len(), 6);
        assert!(r.contains("recall") && r.contains("associate") && r.contains("note"));

        let specs = r.list();
        let recall = specs.iter().find(|s| s.name == "recall").unwrap();
        assert_eq!(recall.origin, "builtin:memory");
        assert_eq!(recall.schema["required"][0], "query");
    }
}
