//! 前端资源：内嵌进二进制。
//!
//! 用 `include_str!` 而不是"从磁盘读 web/ 目录"，是为了让 `styx web`
//! 在**任何目录**都能跑起来，而且没有 CDN、没有构建步骤、断网可用——
//! 一个角色扮演内核的界面，不该因为 npm 装不上就起不来。
//!
//! 唯一的例外是表情包图片：它们是**用户可替换的资源**，
//! 编进二进制就意味着每次换图都要重新编译，所以留在磁盘上由
//! [`crate::WebServer`] 按白名单提供。

/// 单页应用骨架。
pub const INDEX_HTML: &str = include_str!("../web/index.html");
/// 样式。
pub const APP_CSS: &str = include_str!("../web/app.css");
/// 前端逻辑。
pub const APP_JS: &str = include_str!("../web/app.js");

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些锚点是前后端之间的契约：改名不该悄无声息地让界面空掉。
    #[test]
    fn html_exposes_the_anchors_the_script_needs() {
        for anchor in [
            "id=\"stage\"",
            "id=\"inspector\"",
            "id=\"sticker-panel\"",
            "id=\"sticker-grid\"",
            "id=\"input\"",
            "id=\"btn-send\"",
            "id=\"btn-stickers\"",
            "id=\"btn-reset\"",
            "id=\"conn-dot\"",
            "id=\"card-name\"",
            "/app.css",
            "/app.js",
        ] {
            assert!(INDEX_HTML.contains(anchor), "index.html 缺少 {anchor}");
        }
    }

    #[test]
    fn script_targets_the_documented_endpoints() {
        for endpoint in [
            "/api/bootstrap",
            "/api/say",
            "/api/sticker",
            "/api/events",
            "/api/reset",
        ] {
            assert!(APP_JS.contains(endpoint), "app.js 没有调用 {endpoint}");
        }
        // 舞台要认得这四种"表演"，否则回复会渲染成空白
        for kind in ["user_input", "speech", "action", "thought", "sticker"] {
            assert!(APP_JS.contains(kind), "app.js 不认识事件类型 {kind}");
        }
    }

    #[test]
    fn stylesheet_carries_the_theme_and_layout_rules() {
        for needle in [
            "--bg",
            ".msg",
            ".line.speech",
            ".sticker-grid",
            "prefers-color-scheme",
            ".inspector",
        ] {
            assert!(APP_CSS.contains(needle), "app.css 缺少 {needle}");
        }
    }

    #[test]
    fn assets_are_non_trivial() {
        assert!(INDEX_HTML.len() > 1000);
        assert!(APP_CSS.len() > 2000);
        assert!(APP_JS.len() > 4000);
    }
}
