use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::persist::JsonPersistable;

fn default_true() -> bool {
    true
}

/// 附属资源文件（目前只收 `.md`）。
///
/// `path` 是相对 skill 根目录的路径（如 `references/triggers.md`），`content`
/// 是文件正文。交付给 agent 时经 skill 工具的 `path` 参数按需返回 —— agent
/// 跑在远端服务器，读不到本机配置目录里的这些文件，只能靠工具回传。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillResource {
    pub path: String,
    pub content: String,
}

/// A user-defined AI behavior instruction.
///
/// Delivery is progressive disclosure: an enabled skill becomes a `skill_<name>`
/// tool, and `prompt` is returned as that tool's result only when the model calls
/// it. Nothing here is appended to the system prompt — the prompt only announces
/// that skill tools exist (`templates/agent/技能.hbs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub prompt: String,
    /// 附属资源文件（`SKILL.md` 之外的 `.md`，按需被 agent 读取）。
    ///
    /// 旧数据（`skills.json` 里没有这个字段）反序列化成空 vec —— 兼容 =
    /// 保持原样，不因缺字段而清空或报错。空时序列化会跳过，避免每次保存
    /// 都给所有旧 skill 白写一个空数组。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<SkillResource>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 手动排序位置（用户拖拽排序）。仅对用户 skill 有意义；内置 skill
    /// 在 UI 中始终置顶，不参与排序。缺省 0.0：旧数据无此字段时全部并列，
    /// 前端稳定排序保持原有相对顺序（兼容 = 保持原样）。
    #[serde(default)]
    pub position: f64,
    pub created_at: String,
    pub updated_at: String,
}

impl Skill {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            description: description.into(),
            prompt: prompt.into(),
            resources: Vec::new(),
            enabled: true,
            position: 0.0,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// 按相对路径精确查找附属资源。
    ///
    /// 只做等值匹配、不做路径归一化 —— 资源路径在导入时已校验为合法的
    /// 相对路径，这里找不到就是找不到，天然不存在路径穿越。
    pub fn resource(&self, path: &str) -> Option<&SkillResource> {
        self.resources.iter().find(|r| r.path == path)
    }
}

/// Persistent store for user skills.
/// Serialised to skills.json in the app config directory.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SkillStore {
    pub skills: Vec<Skill>,
}

impl SkillStore {
    pub fn new() -> Self {
        Self { skills: Vec::new() }
    }

    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    pub fn get(&self, id: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.id == id)
    }

    pub fn add(&mut self, skill: Skill) {
        self.skills.push(skill);
    }

    pub fn update(
        &mut self,
        id: &str,
        name: Option<String>,
        description: Option<String>,
        prompt: Option<String>,
        resources: Option<Vec<SkillResource>>,
    ) -> bool {
        let now = Utc::now().to_rfc3339();
        if let Some(skill) = self.skills.iter_mut().find(|s| s.id == id) {
            // None = 不动；Some(空) = 清空全部资源
            if let Some(r) = resources {
                skill.resources = r;
            }
            if let Some(n) = name {
                skill.name = n;
            }
            if let Some(d) = description {
                skill.description = d;
            }
            if let Some(p) = prompt {
                skill.prompt = p;
            }
            skill.updated_at = now;
            return true;
        }
        false
    }

    pub fn toggle(&mut self, id: &str) -> bool {
        if let Some(skill) = self.skills.iter_mut().find(|s| s.id == id) {
            skill.enabled = !skill.enabled;
            skill.updated_at = Utc::now().to_rfc3339();
            return true;
        }
        false
    }

    pub fn delete(&mut self, id: &str) -> bool {
        let len_before = self.skills.len();
        self.skills.retain(|s| s.id != id);
        self.skills.len() != len_before
    }

    /// 下一个可用的手动排序位置（追加到列表末尾）。
    pub fn next_position(&self) -> f64 {
        self.skills
            .iter()
            .map(|s| s.position)
            .fold(0.0_f64, f64::max)
            + 1.0
    }

    /// 按给定 id 顺序重排用户 skill 的 position（index * 1000，留出后续
    /// 手动插入的间隙）。忽略不存在的 id；内置 skill 由调用方拒绝。
    ///
    /// 返回 position 发生变化的 skill 克隆，调用方据此持久化 + 推送同步。
    /// 不改动 updated_at：顺序是轻量视图元数据，不应制造内容变更的假象。
    pub fn apply_user_order(&mut self, ordered_ids: &[String]) -> Vec<Skill> {
        let mut changed = Vec::new();
        for (index, id) in ordered_ids.iter().enumerate() {
            let pos = (index + 1) as f64 * 1000.0;
            if let Some(skill) = self.skills.iter_mut().find(|s| &s.id == id) {
                if (skill.position - pos).abs() > f64::EPSILON {
                    skill.position = pos;
                    changed.push(skill.clone());
                }
            }
        }
        changed
    }
}

impl JsonPersistable for SkillStore {
    const SOURCE: crate::data::DataSourceId = crate::data::DataSourceId::Skills;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_data_without_position_defaults_to_zero() {
        let json = r#"{
            "id": "a", "name": "n", "prompt": "p",
            "enabled": true, "createdAt": "t", "updatedAt": "t"
        }"#;
        let skill: Skill = serde_json::from_str(json).unwrap();
        assert_eq!(skill.position, 0.0);
    }

    #[test]
    fn old_data_without_resources_defaults_to_empty() {
        // 兼容红线：旧 skills.json 没有 resources 字段 → 空 vec，保持原样
        let json = r#"{
            "id": "a", "name": "n", "prompt": "p",
            "enabled": true, "createdAt": "t", "updatedAt": "t"
        }"#;
        let skill: Skill = serde_json::from_str(json).unwrap();
        assert!(skill.resources.is_empty());
        assert!(skill.resource("references/a.md").is_none());
    }

    #[test]
    fn empty_resources_are_skipped_on_serialize() {
        // 不给没有资源的旧 skill 白写一个空数组
        let skill = Skill::new("a", "", "p");
        let json = serde_json::to_string(&skill).unwrap();
        assert!(!json.contains("resources"));
    }

    #[test]
    fn resource_lookup_is_exact_match() {
        let mut skill = Skill::new("a", "", "p");
        skill.resources = vec![SkillResource {
            path: "references/a.md".into(),
            content: "x".into(),
        }];
        assert!(skill.resource("references/a.md").is_some());
        assert!(skill.resource("./references/a.md").is_none());
        assert!(skill.resource("../references/a.md").is_none());
    }

    #[test]
    fn update_resources_none_keeps_some_replaces() {
        let mut store = SkillStore::new();
        let mut s = Skill::new("a", "", "p");
        s.resources = vec![SkillResource {
            path: "a.md".into(),
            content: "x".into(),
        }];
        let id = s.id.clone();
        store.add(s);
        // None：保持原样
        assert!(store.update(&id, Some("b".into()), None, None, None));
        assert_eq!(store.get(&id).unwrap().resources.len(), 1);
        // Some：整体替换
        let new = vec![SkillResource {
            path: "c.md".into(),
            content: "y".into(),
        }];
        assert!(store.update(&id, None, None, None, Some(new)));
        assert_eq!(store.get(&id).unwrap().resources[0].path, "c.md");
        // Some(空)：清空
        assert!(store.update(&id, None, None, None, Some(vec![])));
        assert!(store.get(&id).unwrap().resources.is_empty());
    }

    #[test]
    fn next_position_appends_after_max() {
        let mut store = SkillStore::new();
        assert_eq!(store.next_position(), 1.0);
        let mut a = Skill::new("a", "", "");
        a.position = 5.0;
        store.add(a);
        assert_eq!(store.next_position(), 6.0);
    }

    #[test]
    fn apply_user_order_assigns_positions_and_reports_changes() {
        let mut store = SkillStore::new();
        store.add(Skill::new("a", "", ""));
        store.add(Skill::new("b", "", ""));
        store.add(Skill::new("c", "", ""));
        let ids: Vec<String> = store.skills.iter().map(|s| s.id.clone()).collect();
        // 反转顺序：c, b, a
        let mut reversed = ids.clone();
        reversed.reverse();
        let changed = store.apply_user_order(&reversed);
        assert_eq!(changed.len(), 3);
        assert_eq!(store.get(&reversed[0]).unwrap().position, 1000.0);
        assert_eq!(store.get(&reversed[1]).unwrap().position, 2000.0);
        assert_eq!(store.get(&reversed[2]).unwrap().position, 3000.0);
        // 再跑一次相同顺序：无变化
        assert!(store.apply_user_order(&reversed).is_empty());
    }

    #[test]
    fn apply_user_order_ignores_unknown_ids_and_keeps_others() {
        let mut store = SkillStore::new();
        store.add(Skill::new("a", "", ""));
        let id_a = store.skills[0].id.clone();
        let changed = store.apply_user_order(&["missing".to_string(), id_a.clone()]);
        assert_eq!(changed.len(), 1);
        // "missing" 被忽略但占用了 index 0，a 拿到 index 1 的位置
        assert_eq!(store.get(&id_a).unwrap().position, 2000.0);
    }

    #[test]
    fn apply_user_order_partial_list_positions_by_given_index() {
        let mut store = SkillStore::new();
        store.add(Skill::new("a", "", ""));
        store.add(Skill::new("b", "", ""));
        let id_b = store.skills[1].id.clone();
        // 仅列出 b：b 拿到 index 0 的位置
        let changed = store.apply_user_order(&[id_b.clone()]);
        assert_eq!(changed.len(), 1);
        assert_eq!(store.get(&id_b).unwrap().position, 1000.0);
    }
}
