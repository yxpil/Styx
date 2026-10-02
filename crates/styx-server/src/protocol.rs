//! 协议：一行一个 JSON 对象，请求与响应都是一行。
//!
//! 为什么用 JSON-lines 而不是二进制帧：
//!
//! - **可以用 `nc` 调试**。角色的行为调试起来像聊天一样直观，
//!   这在做"角色扮演"这种高度依赖肉眼观察的工作时非常值钱；
//! - **一行一个响应**天然流式，前端可以边收边渲染；
//! - 与 Nebula（加密帧 + JSON 载荷）、MightBe（行分隔文本）的"载荷是 JSON /
//!   行是定界符"思路一致，生态里不增加新的心智负担。
//!
//! 需要加密时，把它套在 TLS 或 Nebula 那样的加密隧道里即可——
//! 协议层不该替用户决定威胁模型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 客户端请求。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// 连通性探测。
    Ping,
    /// 握手：绑定（或创建）一个会话。
    Hello {
        #[serde(default)]
        session: Option<String>,
    },
    /// 说一句，推进一个回合。
    Say { text: String },
    /// 查看当前状态。
    State,
    /// 查看当前场景。
    Scene,
    /// 改场景。
    SetScene { scene: Value },
    /// 查看事件流。
    Events {
        #[serde(default)]
        limit: Option<usize>,
    },
    /// 列出工具。
    Tools,
    /// 调用工具。
    Call {
        tool: String,
        #[serde(default)]
        args: Value,
    },
    /// 查看后端状态。
    Status,
    /// 手动让角色记住一件事。
    Remember {
        text: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        importance: Option<f32>,
    },
    /// 只做召回，不生成。
    Recall {
        query: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// 只做联想，不生成。
    Associate {
        seed: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// 重置会话。
    Reset {
        #[serde(default)]
        scene: Option<Value>,
    },
    /// 断开。
    Quit,
}

impl Request {
    /// 操作名（用于日志与错误提示）。
    pub fn op(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::Hello { .. } => "hello",
            Request::Say { .. } => "say",
            Request::State => "state",
            Request::Scene => "scene",
            Request::SetScene { .. } => "set_scene",
            Request::Events { .. } => "events",
            Request::Tools => "tools",
            Request::Call { .. } => "call",
            Request::Status => "status",
            Request::Remember { .. } => "remember",
            Request::Recall { .. } => "recall",
            Request::Associate { .. } => "associate",
            Request::Reset { .. } => "reset",
            Request::Quit => "quit",
        }
    }

    /// 从一行 JSON 解析。
    pub fn parse(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim())
            .map_err(|e| format!("请求不是合法 JSON 或缺少 op 字段：{e}"))
    }
}

/// 服务端响应：始终是 `{"ok": bool, ...}`。
///
/// 附加数据用 `flatten` 摊平到顶层（而不是塞进 `data` 子对象），
/// 这样客户端可以直接读 `resp.turn` 而不必 `resp.data.turn`。
/// 注意 `data` 必须是 **Map** 而不是任意 `Value`——`serde(flatten)` 只接受
/// 结构体或映射，塞一个 `Null` 进去会在序列化时报错。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: serde_json::Map<String, Value>,
}

impl Response {
    /// 成功响应。
    pub fn ok(op: &str, data: Value) -> Self {
        let data = match data {
            Value::Object(m) => m,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("value".into(), other);
                m
            }
        };
        Response {
            ok: true,
            op: Some(op.to_string()),
            error: None,
            data,
        }
    }

    /// 失败响应。
    pub fn err(op: &str, error: impl Into<String>) -> Self {
        Response {
            ok: false,
            op: Some(op.to_string()),
            error: Some(error.into()),
            data: serde_json::Map::new(),
        }
    }

    /// 序列化成一行（不含换行符）。
    pub fn to_line(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|e| format!(r#"{{"ok":false,"error":"序列化响应失败：{e}"}}"#))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_each_op() {
        let cases: Vec<(&str, &str)> = vec![
            (r#"{"op":"ping"}"#, "ping"),
            (r#"{"op":"hello","session":"a"}"#, "hello"),
            (r#"{"op":"say","text":"你好"}"#, "say"),
            (r#"{"op":"state"}"#, "state"),
            (r#"{"op":"scene"}"#, "scene"),
            (r#"{"op":"tools"}"#, "tools"),
            (r#"{"op":"status"}"#, "status"),
            (r#"{"op":"quit"}"#, "quit"),
        ];
        for (line, want) in cases {
            let r = Request::parse(line).unwrap();
            assert_eq!(r.op(), want, "{line}");
        }
    }

    #[test]
    fn parses_ops_with_arguments() {
        let r = Request::parse(r#"{"op":"call","tool":"dice","args":{"sides":20}}"#).unwrap();
        match r {
            Request::Call { tool, args } => {
                assert_eq!(tool, "dice");
                assert_eq!(args["sides"], 20);
            }
            other => panic!("{other:?}"),
        }

        let r = Request::parse(r#"{"op":"events"}"#).unwrap();
        match r {
            Request::Events { limit } => assert!(limit.is_none()),
            other => panic!("{other:?}"),
        }

        let r = Request::parse(r#"{"op":"remember","text":"x"}"#).unwrap();
        match r {
            Request::Remember { tags, importance, .. } => {
                assert!(tags.is_empty());
                assert!(importance.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_requests() {
        assert!(Request::parse("not json").is_err());
        assert!(Request::parse(r#"{"text":"没有 op"}"#).is_err());
        assert!(Request::parse(r#"{"op":"unknown_op"}"#).is_err());
        let e = Request::parse(r#"{"op":"say"}"#).unwrap_err();
        assert!(e.contains("缺少 op 字段") || e.contains("text"), "{e}");
    }

    #[test]
    fn response_round_trip_is_one_line() {
        let r = Response::ok("say", json!({"turn": 3, "reply": {"speech": ["不卖。"]}}));
        let line = r.to_line();
        assert!(!line.contains('\n'));
        let back: Response = serde_json::from_str(&line).unwrap();
        assert!(back.ok);
        assert_eq!(back.op.as_deref(), Some("say"));
        assert_eq!(back.data["turn"], 3);
        assert_eq!(back.data["reply"]["speech"][0], "不卖。");
        // 成功响应里不该出现 error 键
        assert!(!line.contains("\"error\""));
    }

    #[test]
    fn error_response_shape() {
        let line = Response::err("say", "模型不可用").to_line();
        assert!(line.contains(r#""ok":false"#));
        assert!(line.contains("模型不可用"));
        let back: Response = serde_json::from_str(&line).unwrap();
        assert!(!back.ok);
    }

    #[test]
    fn response_flattens_data_into_the_top_level() {
        let line = Response::ok("state", json!({"mood": "平静"})).to_line();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["mood"], "平静");
        assert_eq!(v["op"], "state");
    }
}
