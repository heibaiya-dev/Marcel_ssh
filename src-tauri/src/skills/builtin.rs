use chrono::Utc;

use crate::skills::parser::process_md;
use crate::skills::store::{Skill, SkillStore};

/// 内置 skill 的 id 前缀。前后端都以此前缀识别内置 skill。
///
/// 设计约定（不可破坏）：
/// - 内置 skill 使用**固定确定性 id**，保证应用升级前后仍是同一条记录；
/// - 内置 skill **不可编辑、不可删除**，只能启用/禁用——内容始终以当前
///   二进制内嵌的版本为准，应用升级后启动时无条件覆盖，确保用户总能
///   吃到最新内容（enabled 状态保留）；
/// - `skill_update` / `skill_delete` command 都必须遵守上述规则
///   （见 `commands/skill.rs`）。
pub const BUILTIN_SKILL_PREFIX: &str = "builtin.";

pub fn is_builtin_skill_id(id: &str) -> bool {
    id.starts_with(BUILTIN_SKILL_PREFIX)
}

/// 交互可视化 skill 的 id。它与 `render_html` 工具同开同关，且手机端调用时会
/// 追加手机专属规则（见 `agent/tools/skill.rs`），所以多处按 id 认它。
pub const VISUALIZE_SKILL_ID: &str = "builtin.visualize";

struct BuiltinSkillDef {
    id: &'static str,
    /// SKILL.md 内容（含 YAML frontmatter），编译期嵌入二进制。
    content: &'static str,
}

const BUILTIN_SKILLS: &[BuiltinSkillDef] = &[BuiltinSkillDef {
    id: VISUALIZE_SKILL_ID,
    content: include_str!("../../builtin_skills/visualize.md"),
}];

/// 已下线的内置 skill id（界面操作指导类：Agent 模式 / 模型配置 / 扩展功能）。
///
/// 内置 skill 对用户**不可删除**，旧版本启动时写进 `skills.json` 的记录不会
/// 自己消失，所以下线一条内置 skill 必须把它的 id 登记在这里，启动时由
/// [`retire_builtin_skills`] 清掉；否则老用户的列表里会留着一条删不掉、
/// 内容再也不更新的残骸，还会继续注册成 `skill_*` 工具。
const RETIRED_BUILTIN_SKILL_IDS: &[&str] = &[
    "builtin.agent-modes",
    "builtin.model-setup",
    "builtin.extensions",
];

/// 启动时调用：把已下线的内置 skill 从 store 里摘掉，返回被摘掉的条数
/// （调用方据此决定是否持久化）。
pub fn retire_builtin_skills(store: &mut SkillStore) -> usize {
    let before = store.skills.len();
    store
        .skills
        .retain(|s| !RETIRED_BUILTIN_SKILL_IDS.contains(&s.id.as_str()));
    before - store.skills.len()
}

/// 启动时调用：把内置 skill 注入/更新到 store。
///
/// - 本地不存在 → 注入（enabled = true）；
/// - 本地存在但内容与二进制内嵌版本不同（旧版本内容、或旧版本 App 允许
///   编辑时被用户改过）→ 覆盖为内嵌版本，**保留 enabled 与 created_at**；
/// - 内容一致 → 不动（不产生变更）。
///
/// 返回发生注入/更新的 skill 列表，调用方据此决定是否持久化。
pub fn ensure_builtin_skills(store: &mut SkillStore) -> Vec<Skill> {
    let mut changed = Vec::new();
    for def in BUILTIN_SKILLS {
        let parsed = match process_md(def.content, def.id) {
            Ok(p) => p,
            Err(e) => {
                // 内容编译期嵌入，解析失败属于打包错误；有单测保证，这里仅防御。
                log::error!("内置 skill {} 解析失败: {}", def.id, e);
                continue;
            }
        };
        let now = Utc::now().to_rfc3339();
        match store.skills.iter_mut().find(|s| s.id == def.id) {
            Some(existing) => {
                if existing.name != parsed.name
                    || existing.description != parsed.description
                    || existing.prompt != parsed.prompt
                {
                    existing.name = parsed.name;
                    existing.description = parsed.description;
                    existing.prompt = parsed.prompt;
                    existing.updated_at = now;
                    changed.push(existing.clone());
                }
            }
            None => {
                let skill = Skill {
                    id: def.id.to_string(),
                    name: parsed.name,
                    description: parsed.description,
                    prompt: parsed.prompt,
                    // 内置 skill 由单个 md 内嵌，不带附属资源
                    resources: Vec::new(),
                    enabled: true,
                    // 内置 skill 置顶由前端展示层保证，position 不参与排序
                    position: 0.0,
                    created_at: now.clone(),
                    updated_at: now,
                };
                store.skills.push(skill.clone());
                changed.push(skill);
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_defs_parse_and_ids_are_valid() {
        let mut ids = std::collections::HashSet::new();
        for def in BUILTIN_SKILLS {
            assert!(
                is_builtin_skill_id(def.id),
                "内置 skill id 必须以 {} 开头: {}",
                BUILTIN_SKILL_PREFIX,
                def.id
            );
            assert!(ids.insert(def.id), "内置 skill id 重复: {}", def.id);
            let parsed = process_md(def.content, def.id)
                .unwrap_or_else(|e| panic!("内置 skill {} 解析失败: {}", def.id, e));
            assert!(!parsed.name.trim().is_empty(), "{} name 为空", def.id);
            assert!(
                !parsed.description.trim().is_empty(),
                "{} description 为空（LLM 靠它决定何时调用）",
                def.id
            );
            assert!(!parsed.prompt.trim().is_empty(), "{} prompt 为空", def.id);
        }
    }

    /// 作者契约：方向性规范 + 小规模实例 + **恰好一个**完整示例。示例一多，
    /// 模型就会把所有内容塞进同一个形状，「一切服务于你想表达的内容」名存实
    /// 亡。required 钉住契约与唯一示例（`ivp-root`，反比例函数），forbidden
    /// 钉住旧模板不得回归；示例根 id 与 `<svg` 各恰好一次，钉住「只有一个完整
    /// 示例」——将来要加第二个示例，必须先确认那不是在造模板，再改这里的计数。
    #[test]
    fn visualize_skill_keeps_authoring_contract_and_single_example() {
        let def = BUILTIN_SKILLS
            .iter()
            .find(|def| def.id == "builtin.visualize")
            .expect("builtin.visualize must be registered");
        let parsed = process_md(def.content, def.id).expect("visualize skill must parse");
        for required in [
            "一切服务于你想表达的内容",
            "--viz-series-1",
            ".viz-controls",
            "chart.js@4.4.1",
            "chart.update()",
            "ResizeObserver",
            "function springTo(state, target, render)",
            "id=\"ivp-root\"",
            "动效是硬性验收项",
            "可中断性最重要",
            "采用高召回策略",
            "不要先询问“要不要做图”",
            "项目没有 Mermaid 展示能力",
            "不提工具、fragment、文件或实现机制",
            // fragment 的转义口径：真实换行、不预转义。反过来教模型自己写 `\n` / `\"`，实测
            // 出过整页被二次转义的坏图（render_html 对此另有检测兜底，两处互为补充）。
            "不要预先转义",
        ] {
            assert!(
                parsed.prompt.contains(required),
                "visualize skill 缺少关键契约/示例: {required}"
            );
        }
        for forbidden in [
            "sim-cooling",
            "cmp-langs",
            "必须从这里改",
            "marcel-queue-demo",
            // 旧措辞「换行写 \n、引号写 \"」：诱导模型把 fragment 预先转义，不得回归。
            "换行写",
        ] {
            assert!(
                !parsed.prompt.contains(forbidden),
                "visualize skill 不应再提供旧模板/旧示例: {forbidden}"
            );
        }
        for once in ["id=\"ivp-root\"", "<svg"] {
            assert_eq!(
                parsed.prompt.matches(once).count(),
                1,
                "visualize skill 只允许一个完整示例（锚点 {once} 应恰好一次）；多出来的都会变成模板"
            );
        }
    }

    #[test]
    fn retired_ids_are_not_registered_and_are_removed_from_store() {
        for id in RETIRED_BUILTIN_SKILL_IDS {
            assert!(
                is_builtin_skill_id(id) && BUILTIN_SKILLS.iter().all(|d| d.id != *id),
                "下线的内置 skill 不能同时还在注册表里: {id}"
            );
        }
        let mut store = SkillStore::new();
        ensure_builtin_skills(&mut store);
        for id in RETIRED_BUILTIN_SKILL_IDS {
            let mut stale = Skill::new("旧内置", "desc", "prompt");
            stale.id = (*id).to_string();
            store.add(stale);
        }
        let user = Skill::new("我的 skill", "desc", "prompt");
        let user_id = user.id.clone();
        store.add(user);

        assert_eq!(
            retire_builtin_skills(&mut store),
            RETIRED_BUILTIN_SKILL_IDS.len()
        );
        assert!(
            store.get(VISUALIZE_SKILL_ID).is_some(),
            "在册内置 skill 不能被误删"
        );
        assert!(store.get(&user_id).is_some(), "用户 skill 不能被误删");
        assert_eq!(retire_builtin_skills(&mut store), 0, "第二次不应再有变更");
    }

    #[test]
    fn inject_into_empty_store() {
        let mut store = SkillStore::new();
        let changed = ensure_builtin_skills(&mut store);
        assert_eq!(changed.len(), BUILTIN_SKILLS.len());
        assert_eq!(store.skills.len(), BUILTIN_SKILLS.len());
        assert!(store.skills.iter().all(|s| s.enabled));
    }

    #[test]
    fn second_run_is_noop() {
        let mut store = SkillStore::new();
        ensure_builtin_skills(&mut store);
        let changed = ensure_builtin_skills(&mut store);
        assert!(changed.is_empty(), "内容一致时不应产生变更");
    }

    #[test]
    fn disabled_state_is_preserved() {
        let mut store = SkillStore::new();
        ensure_builtin_skills(&mut store);
        let id = BUILTIN_SKILLS[0].id;
        assert!(store.toggle(id));
        let changed = ensure_builtin_skills(&mut store);
        assert!(changed.is_empty());
        assert!(!store.get(id).unwrap().enabled, "禁用状态必须保留");
    }

    #[test]
    fn stale_content_is_overwritten_keeping_enabled_and_created_at() {
        let mut store = SkillStore::new();
        ensure_builtin_skills(&mut store);
        let id = BUILTIN_SKILLS[0].id;
        store.toggle(id); // 用户禁用
        let created_at = store.get(id).unwrap().created_at.clone();
        // 模拟旧版本内容 / 旧版本 App 中被编辑过
        {
            let s = store.skills.iter_mut().find(|s| s.id == id).unwrap();
            s.prompt = "旧版内容".into();
            s.name = "旧版名称".into();
        }
        let changed = ensure_builtin_skills(&mut store);
        assert_eq!(changed.len(), 1);
        let s = store.get(id).unwrap();
        assert_ne!(s.prompt, "旧版内容", "内容必须被覆盖为内嵌版本");
        assert!(!s.enabled, "覆盖内容时保留 enabled");
        assert_eq!(s.created_at, created_at, "覆盖内容时保留 created_at");
    }

    #[test]
    fn user_skills_are_untouched() {
        let mut store = SkillStore::new();
        store.add(Skill::new("我的 skill", "desc", "prompt"));
        let before = store.skills[0].clone();
        ensure_builtin_skills(&mut store);
        let after = store.get(&before.id).unwrap();
        assert_eq!(after.prompt, before.prompt);
        assert_eq!(after.updated_at, before.updated_at);
    }
}
