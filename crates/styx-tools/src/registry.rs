//! 工具注册表与 [`ToolPort`] 实现。
//!
//! 内核只认识 [`styx_core::ports::ToolPort`]：能列工具、能调工具。
//! 工具从哪来（内置、MCP、BIT Remote）对内核是透明的。
//!
//! 这与 [BIT](https://github.com/yxpil/bit) / [TentacleTool](https://github.com/yxpil/TentacleTool)
//! 的工具模型是同构的，因此两边可以互相挂载。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use styx_core::error::{Result, StyxError};
use styx_core::ports::{ToolPort, ToolSpec};

/// 一个工具。
pub trait Tool: Send + Sync {
    /// 工具声明。
    fn spec(&self) -> ToolSpec;

    /// 调用。
    fn invoke(&self, args: Value) -> Result<Value>;
}

/// 工具注册表。
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        ToolRegistry::default()
    }

    /// 注册一个工具；同名会覆盖。
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> &mut Self {
        self.tools.insert(tool.spec().name.clone(), Arc::new(tool));
        self
    }

    /// 注册一个已装箱的工具。
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.spec().name.clone(), tool);
        self
    }

    /// 是否已注册。
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 注册的工具数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 按名字取一个工具。
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// 把所有工具打包成一个 [`ToolPort`]。
    pub fn into_port(self) -> Arc<dyn ToolPort> {
        Arc::new(self)
    }
}

impl ToolPort for ToolRegistry {
    fn name(&self) -> &str {
        "tools"
    }

    fn list(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    fn invoke(&self, tool: &str, args: Value) -> Result<Value> {
        let t = self
            .tools
            .get(tool)
            .ok_or_else(|| StyxError::UnknownTool(tool.to_string()))?;
        t.invoke(args)
    }

    fn status(&self) -> String {
        if self.tools.is_empty() {
            return "tools（未注册任何工具）".into();
        }
        let names: Vec<&str> = self.tools.keys().map(|s| s.as_str()).collect();
        format!("tools（{} 个）：{}", names.len(), names.join(", "))
    }
}

/// 把一组 [`ToolPort`] 串成一个：先问第一个，找不到再问下一个。
///
/// 这让"内置工具 + MCP 服务器 + BIT"能作为一个整体注入内核。
pub struct ChainedTools {
    ports: Vec<Arc<dyn ToolPort>>,
}

impl std::fmt::Debug for ChainedTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainedTools")
            .field("ports", &self.ports.iter().map(|p| p.name()).collect::<Vec<_>>())
            .finish()
    }
}

impl ChainedTools {
    /// 串联多个工具端口。
    pub fn new(ports: Vec<Arc<dyn ToolPort>>) -> Self {
        ChainedTools { ports }
    }
}

impl ToolPort for ChainedTools {
    fn name(&self) -> &str {
        "tools(chained)"
    }

    fn list(&self) -> Vec<ToolSpec> {
        // 用 BTreeMap 去重：同名保持「靠前的端口胜出」，输出顺序按名字排序，
        // 与单个 `ToolRegistry`（本身就是 BTreeMap）的顺序一致。
        // 否则同一批工具在链上、链下的排列会不一样，客户端看着就像在乱跳。
        let mut merged: BTreeMap<String, ToolSpec> = BTreeMap::new();
        for p in &self.ports {
            for spec in p.list() {
                merged.entry(spec.name.clone()).or_insert(spec);
            }
        }
        merged.into_values().collect()
    }

    fn invoke(&self, tool: &str, args: Value) -> Result<Value> {
        let mut last: Option<StyxError> = None;
        for p in &self.ports {
            if !p.list().iter().any(|s| s.name == tool) {
                continue;
            }
            match p.invoke(tool, args.clone()) {
                Ok(v) => return Ok(v),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| StyxError::UnknownTool(tool.to_string())))
    }

    fn status(&self) -> String {
        let parts: Vec<String> = self
            .ports
            .iter()
            .map(|p| format!("{}={}", p.name(), p.list().len()))
            .collect();
        format!("tools(chained) [{}]", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("echo", "原样返回入参")
                .with_schema(json!({"type":"object","properties":{"text":{"type":"string"}}}))
        }
        fn invoke(&self, args: Value) -> Result<Value> {
            Ok(json!({ "echoed": args }))
        }
    }

    struct Boom;

    impl Tool for Boom {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("boom", "总是失败")
        }
        fn invoke(&self, _args: Value) -> Result<Value> {
            Err(StyxError::Tool("boom".into(), "炸了".into()))
        }
    }

    #[test]
    fn registry_lists_and_invokes() {
        let mut r = ToolRegistry::new();
        r.register(Echo).register(Boom);
        assert_eq!(r.len(), 2);
        assert!(r.contains("echo"));

        let specs = r.list();
        assert_eq!(specs.len(), 2);
        assert!(specs.iter().any(|s| s.name == "echo" && s.description == "原样返回入参"));

        let out = r.invoke("echo", json!({"text":"hi"})).unwrap();
        assert_eq!(out["echoed"]["text"], "hi");

        assert!(r.status().contains("echo"));
    }

    #[test]
    fn unknown_tool_is_an_error() {
        let r = ToolRegistry::new();
        let err = r.invoke("nope", json!({})).unwrap_err();
        assert!(matches!(err, StyxError::UnknownTool(_)));
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn tool_failure_propagates() {
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
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn empty_registry_status_is_explicit() {
        let r = ToolRegistry::new();
        assert!(r.is_empty());
        assert!(r.status().contains("未注册"));
    }

    #[test]
    fn chained_tools_dedupes_and_falls_through() {
        let mut a = ToolRegistry::new();
        a.register(Echo);
        let mut b = ToolRegistry::new();
        b.register(Echo).register(Boom);

        let chain = ChainedTools::new(vec![a.into_port(), b.into_port()]);
        // 同名只出现一次
        let names: Vec<String> = chain.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["boom".to_string(), "echo".to_string()]);
        // echo 由第一个端口处理
        assert_eq!(
            chain.invoke("echo", json!({"text":"x"})).unwrap()["echoed"]["text"],
            "x"
        );
        // boom 只有第二个端口有
        assert!(chain.invoke("boom", json!({})).is_err());
        assert!(chain.invoke("missing", json!({})).is_err());
        assert!(chain.status().contains("chained"));
    }

    #[test]
    fn chained_tools_falls_back_when_the_first_port_fails() {
        struct Flaky;
        impl Tool for Flaky {
            fn spec(&self) -> ToolSpec {
                ToolSpec::new("flaky", "先失败")
            }
            fn invoke(&self, _a: Value) -> Result<Value> {
                Err(StyxError::Tool("flaky".into(), "down".into()))
            }
        }
        struct Good;
        impl Tool for Good {
            fn spec(&self) -> ToolSpec {
                ToolSpec::new("flaky", "后成功")
            }
            fn invoke(&self, _a: Value) -> Result<Value> {
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
}
