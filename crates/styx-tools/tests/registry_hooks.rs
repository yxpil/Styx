//! 集成测试：工具注册表 / 链式端口（钩子/插件机制）。
//!
//! 从 crate 外部验证 ToolRegistry 与 ChainedTools 的钩子语义：
//! 注册→列出→调用、未注册工具被拒绝、失败传播、链式回退、以及"一个工具
//! 失败不影响其它工具"的失败隔离。

use serde_json::{json, Value};
use styx_core::error::StyxError;
use styx_core::ports::{ToolPort, ToolSpec};
use styx_tools::registry::{ChainedTools, Tool, ToolRegistry};

struct Echo;
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("echo", "原样返回入参")
            .with_schema(json!({"type":"object","properties":{"text":{"type":"string"}}}))
    }
    fn invoke(&self, args: Value) -> Result<Value, StyxError> {
        Ok(json!({ "echoed": args }))
    }
}

struct Boom;
impl Tool for Boom {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("boom", "总是失败")
    }
    fn invoke(&self, _args: Value) -> Result<Value, StyxError> {
        Err(StyxError::Tool("boom".into(), "炸了".into()))
    }
}

struct Add;
impl Tool for Add {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("add", "两数相加")
    }
    fn invoke(&self, args: Value) -> Result<Value, StyxError> {
        let a = args["a"].as_i64().unwrap_or(0);
        let b = args["b"].as_i64().unwrap_or(0);
        Ok(json!({ "sum": a + b }))
    }
}

// ── 注册 → 列出 → 调用 ──

#[test]
fn register_list_and_invoke_roundtrip() {
    let mut r = ToolRegistry::new();
    assert!(r.is_empty());
    r.register(Echo).register(Add);
    assert_eq!(r.len(), 2);
    assert!(r.contains("echo"));
    assert!(r.contains("add"));

    let specs = r.list();
    assert!(specs.iter().any(|s| s.name == "add"));

    let out = r.invoke("add", json!({"a":2,"b":3})).unwrap();
    assert_eq!(out["sum"], 5);
}

#[test]
fn unknown_tool_is_rejected() {
    let r = ToolRegistry::new();
    let err = r.invoke("nope", json!({})).unwrap_err();
    assert!(matches!(err, StyxError::UnknownTool(_)));
}

#[test]
fn tool_failure_propagates_without_panicking() {
    let mut r = ToolRegistry::new();
    r.register(Boom);
    let err = r.invoke("boom", json!({})).unwrap_err();
    assert!(err.to_string().contains("炸了"));
}

#[test]
fn registration_overwrites_same_name() {
    let mut r = ToolRegistry::new();
    r.register(Echo);
    r.register(Echo);
    assert_eq!(r.len(), 1, "same-name register must overwrite, not duplicate");
}

// ── 失败隔离：一个工具失败，其它工具仍可用 ──

#[test]
fn failure_isolation_between_tools() {
    let mut r = ToolRegistry::new();
    r.register(Echo).register(Boom).register(Add);

    // boom 失败……
    assert!(r.invoke("boom", json!({})).is_err());
    // ……但 echo / add 依然正常工作
    assert_eq!(r.invoke("echo", json!({"text":"ok"})).unwrap()["echoed"]["text"], "ok");
    assert_eq!(r.invoke("add", json!({"a":1,"b":1})).unwrap()["sum"], 2);
}

// ── 链式端口：去重 + 第一个端口胜出 + 失败回退 ──

#[test]
fn chained_dedupes_and_first_port_wins() {
    let mut a = ToolRegistry::new();
    a.register(Echo);
    let mut b = ToolRegistry::new();
    b.register(Echo).register(Boom);

    let chain = ChainedTools::new(vec![a.into_port(), b.into_port()]);
    let names: Vec<String> = chain.list().into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["boom".to_string(), "echo".to_string()]);
    // echo 由第一个端口处理
    assert_eq!(chain.invoke("echo", json!({"text":"x"})).unwrap()["echoed"]["text"], "x");
}

#[test]
fn chained_falls_back_when_first_port_errors() {
    struct Flaky;
    impl Tool for Flaky {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("flaky", "先失败")
        }
        fn invoke(&self, _a: Value) -> Result<Value, StyxError> {
            Err(StyxError::Tool("flaky".into(), "down".into()))
        }
    }
    struct Good;
    impl Tool for Good {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("flaky", "后成功")
        }
        fn invoke(&self, _a: Value) -> Result<Value, StyxError> {
            Ok(json!({"ok": true}))
        }
    }
    let mut a = ToolRegistry::new();
    a.register(Flaky);
    let mut b = ToolRegistry::new();
    b.register(Good);
    let chain = ChainedTools::new(vec![a.into_port(), b.into_port()]);
    assert_eq!(chain.invoke("flaky", json!({})).unwrap()["ok"], true);
}

#[test]
fn chained_unknown_tool_errors() {
    let a = ToolRegistry::new();
    let chain = ChainedTools::new(vec![a.into_port()]);
    let err = chain.invoke("missing", json!({})).unwrap_err();
    assert!(matches!(err, StyxError::UnknownTool(_)));
}
