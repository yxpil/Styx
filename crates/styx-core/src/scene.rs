//! 场景：角色**当下所处的那一格世界**。
//!
//! 场景是内核里唯一"由外部驱动"的输入（用户改它、或模型通过 `[场]` 标签改它），
//! 角色卡和动态状态都是由内核维护的。把场景单独拎出来，是为了让同一个角色
//! 能直接搬进不同剧情线（换场景不换人设）。

use serde::{Deserialize, Serialize};

/// 一个场景快照。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scene {
    /// 世界观 / 时代背景。
    #[serde(default)]
    pub world: String,
    /// 地点。
    #[serde(default)]
    pub location: String,
    /// 时间（可以是"深夜两点"这种模糊描述）。
    #[serde(default)]
    pub time: String,
    /// 天气 / 环境氛围。
    #[serde(default)]
    pub weather: String,
    /// 在场角色名。
    #[serde(default)]
    pub present: Vec<String>,
    /// 当前局面：正在发生什么。
    #[serde(default)]
    pub situation: String,
    /// 基调：悬疑 / 治愈 / 冲突 等。
    #[serde(default)]
    pub tone: String,
    /// 用户在此场景中扮演的角色名（用于称呼与关系解析）。
    #[serde(default)]
    pub user_role: String,
}

impl Scene {
    /// 一个最简场景。
    pub fn new(location: impl Into<String>) -> Self {
        Scene {
            location: location.into(),
            ..Default::default()
        }
    }

    /// 渲染为提示词里的一段紧凑描述，跳过所有空字段。
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.world.is_empty() {
            parts.push(format!("世界：{}", self.world));
        }
        if !self.location.is_empty() {
            parts.push(format!("地点：{}", self.location));
        }
        if !self.time.is_empty() {
            parts.push(format!("时间：{}", self.time));
        }
        if !self.weather.is_empty() {
            parts.push(format!("环境：{}", self.weather));
        }
        if !self.tone.is_empty() {
            parts.push(format!("基调：{}", self.tone));
        }
        if !self.present.is_empty() {
            parts.push(format!("在场：{}", self.present.join("、")));
        }
        if !self.user_role.is_empty() {
            parts.push(format!("用户身份：{}", self.user_role));
        }
        if !self.situation.is_empty() {
            parts.push(format!("局面：{}", self.situation));
        }
        if parts.is_empty() {
            return "（未设定场景）".to_string();
        }
        parts.join("\n")
    }

    /// 是否为空场景（全部字段都空）。
    pub fn is_empty(&self) -> bool {
        self.world.is_empty()
            && self.location.is_empty()
            && self.time.is_empty()
            && self.weather.is_empty()
            && self.present.is_empty()
            && self.situation.is_empty()
            && self.tone.is_empty()
            && self.user_role.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_skips_empty_fields() {
        let mut s = Scene::new("拾光旧书店");
        s.time = "深夜".into();
        s.present = vec!["林夏".into(), "陈默".into()];
        let r = s.render();
        assert!(r.contains("拾光旧书店"));
        assert!(r.contains("深夜"));
        assert!(r.contains("林夏、陈默"));
        assert!(!r.contains("世界："));
        assert!(!s.is_empty());
    }

    #[test]
    fn empty_scene_renders_placeholder() {
        assert_eq!(Scene::default().render(), "（未设定场景）");
        assert!(Scene::default().is_empty());
    }
}
