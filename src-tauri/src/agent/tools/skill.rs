use async_trait::async_trait;
use serde_json::json;

use crate::agent::risk::Disposition;
use crate::agent::templates::TemplateManager;
use crate::agent::tools::{AgentTool, ToolContext, ToolOutput};
use crate::error::AppError;
use crate::skills::store::{Skill, SkillResource};

/// skill 注册成工具时的名字前缀。
///
/// 单独提成常量是因为「这次注册里有没有 skill」只能按前缀判断 —— skills 是
/// 运行时动态注册的（`skill_<名称>_<id>`），不像内置工具那样有声明条目。
pub const SKILL_TOOL_PREFIX: &str = "skill_";

/// 工具名总长上限。OpenAI 兼容接口要求 `^[a-zA-Z0-9_-]{1,64}$`，超限会让**整个请求**
/// 被拒（所有任务都跑不了，而不只是这个 skill 不可用）；GitHub 导入的 skill 名常有
/// 几十个字符。取 64 整，名字部分按剩余长度截断。
const MAX_TOOL_NAME_LEN: usize = 64;

/// 把请求里的资源路径规整成清单里的写法：反斜杠转 `/`、去掉开头的 `./`。
/// SKILL.md 正文里常写 `./references/a.md`，模型照抄过来不该因此「未找到」。
/// 仍是白名单等值查找 —— 规整只去掉 `./`，不解析 `..`，穿越依旧找不到。
fn normalize_requested_path(raw: &str) -> String {
    let mut p = raw.trim().replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    p
}

pub struct SkillTool {
    name: String,
    display_name: String,
    description: String,
    prompt: String,
    /// 附属 `.md` 资源（相对 skill 根的路径）。agent 跑在远端、读不到本机
    /// 配置目录，只能经 `path` 参数按需回传，所以资源随工具进内存。
    resources: Vec<SkillResource>,
}

impl SkillTool {
    pub fn new(skill: &Skill) -> Self {
        // 非 ASCII 字母数字替换为 _，折叠连续 _ 并去除首尾 _。
        // 纯中文名会得到空串（或撞名），此时追加 id 前 8 位保证工具名
        // 稳定且互不冲突（内置 skill id 固定，工具名跨启动一致）。
        let mut safe_name = String::new();
        let mut prev_underscore = true; // 抑制开头的 _
        for c in skill.name.chars() {
            if c.is_ascii_alphanumeric() || c == '-' {
                safe_name.push(c);
                prev_underscore = false;
            } else if !prev_underscore {
                safe_name.push('_');
                prev_underscore = true;
            }
        }
        let safe_name = safe_name.trim_matches('_');
        let id_tag: String = skill
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
        // 名字部分的预算 = 总长 - 前缀 - `_` - id 标签。safe_name 此时只含 ASCII，
        // 按字节截断安全；截完再去一次尾部的 `_`/`-`，避免出现 `__`。
        let budget = MAX_TOOL_NAME_LEN.saturating_sub(SKILL_TOOL_PREFIX.len() + 1 + id_tag.len());
        let safe_name = if safe_name.len() > budget {
            safe_name[..budget].trim_end_matches(['_', '-'])
        } else {
            safe_name
        };
        let name = if safe_name.is_empty() {
            format!("{}{}", SKILL_TOOL_PREFIX, id_tag)
        } else {
            format!("{}{}_{}", SKILL_TOOL_PREFIX, safe_name, id_tag)
        };
        let mut prompt = skill.prompt.clone();
        if crate::skills::builtin::is_builtin_skill_id(&skill.id) {
            prompt.push_str(&builtin_skill_additions(&skill.id, cfg!(mobile)));
        }
        Self {
            name,
            display_name: skill.name.clone(),
            description: if skill.description.trim().is_empty() {
                skill.name.clone()
            } else {
                format!("{}: {}", skill.name, skill.description)
            },
            prompt,
            resources: skill.resources.clone(),
        }
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// 资源清单文本（工具无参调用与未知路径报错共用）。
    fn resource_list(&self) -> String {
        self.resources
            .iter()
            .map(|r| format!("- {}", r.path))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 生成工具返回值。与 `execute` 分离，便于脱离 `ToolContext` 单测。
    fn render(&self, params: &serde_json::Value) -> ToolOutput {
        let requested = params
            .get("path")
            .and_then(|v| v.as_str())
            .map(normalize_requested_path)
            .filter(|s| !s.is_empty());

        // 带 path：按需返回某个附属资源。白名单等值查找 —— 不存在路径穿越。
        if let Some(path) = requested.as_deref() {
            let Some(res) = self.resources.iter().find(|r| r.path == path) else {
                return ToolOutput::fail(
                    format!("SKILL {} · 未知资源", self.display_name),
                    format!(
                        "未找到资源文件 {}。该 skill 的可用资源文件：\n{}",
                        path,
                        self.resource_list()
                    ),
                );
            };
            return ToolOutput::ok(
                format!("SKILL {} · {}", self.display_name, res.path),
                format!(
                    "## Skill: {} — 资源文件 {}\n\n{}",
                    self.display_name, res.path, res.content
                ),
            );
        }

        // 无 path：返回主指令；有资源时附上清单，让模型知道还能按需读取什么。
        let mut output = format!(
            "## Skill: {}\n\nYou selected the skill \"{}\". Follow these instructions:\n\n{}",
            self.display_name, self.display_name, self.prompt
        );
        if !self.resources.is_empty() {
            output.push_str(&format!(
                "\n\n---\n\n该 skill 附带 {} 个资源文件。正文若引用了它们，请用 `path` 参数按需读取，不要凭记忆编造其内容：\n{}",
                self.resources.len(),
                self.resource_list()
            ));
        }
        ToolOutput::ok(format!("SKILL {}", self.display_name), output)
    }
}

/// 内置 skill 调用时追加在正文后的内容：平台说明；手机端调用 visualize 时再加
/// 一段手机专属规则。
///
/// 文本在 `templates/skill/平台说明.hbs`（桌面/移动走 `is_mobile` 分支）与
/// `templates/skill/可视化_移动端.hbs`（只在手机端追加，无分支）。`mobile` 作为
/// 参数而不是在这里读 `cfg!(mobile)`，是为了桌面构建的测试也能把手机分支跑到
/// （`#[cfg]` 两份副本做不到）。前面的 `\n\n` 分隔符由这里补，模板文件只放正文。
/// 存储层（skills.json）始终保存纯净内容，注入只发生在工具调用时。
///
/// `builtin_skills/visualize.md` 是两端共用的创作契约，手机专属约束一律走模板，
/// 不要写进那份正文（`visualize_skill_keeps_authoring_contract_and_single_example`
/// 钉着它的内容）。
fn builtin_skill_additions(skill_id: &str, mobile: bool) -> String {
    let mut out = format!(
        "\n\n{}",
        TemplateManager.render_fragment("平台说明", &json!({ "is_mobile": mobile }))
    );
    if mobile && skill_id == crate::skills::builtin::VISUALIZE_SKILL_ID {
        out.push_str(&format!(
            "\n\n{}",
            TemplateManager.render_fragment("可视化_移动端", &json!({}))
        ));
    }
    out
}

#[async_trait]
impl AgentTool for SkillTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要读取的附属资源文件的相对路径（如 references/triggers.md）。省略则返回 skill 主指令与可用资源文件清单。"
                }
            }
        })
    }

    fn disposition(&self) -> Disposition {
        Disposition::Allow
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, AppError> {
        Ok(self.render(&params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_skill_gets_platform_section() {
        let mut skill = Skill::new("可视化", "创作", "正文内容");
        skill.id = crate::skills::builtin::VISUALIZE_SKILL_ID.into();
        let tool = SkillTool::new(&skill);
        assert!(
            tool.prompt.contains("用户当前平台"),
            "内置 skill 必须追加平台段"
        );
        assert!(
            tool.prompt.contains("桌面版") || tool.prompt.contains("移动版"),
            "平台段必须包含具体平台名"
        );
        assert!(tool.prompt.contains("正文内容"), "原 prompt 必须保留");
    }

    #[test]
    fn visualize_mobile_rules_attach_only_to_visualize_on_mobile() {
        use crate::skills::builtin::VISUALIZE_SKILL_ID;
        let marker = "手机端可视化规则";

        let on_mobile = builtin_skill_additions(VISUALIZE_SKILL_ID, true);
        assert!(on_mobile.contains("移动版"), "平台说明仍须保留");
        assert!(
            on_mobile.contains(marker),
            "手机端调用 visualize 必须带手机规则"
        );
        assert!(
            on_mobile.find("用户当前平台") < on_mobile.find(marker),
            "平台说明在前、手机规则在后"
        );

        assert!(
            !builtin_skill_additions(VISUALIZE_SKILL_ID, false).contains(marker),
            "桌面端不应拿到手机规则"
        );
        assert!(
            !builtin_skill_additions("builtin.some-other", true).contains(marker),
            "其它内置 skill 不应拿到手机规则"
        );
    }

    #[test]
    fn user_skill_has_no_platform_section() {
        // 普通 uuid id（用户自建 skill 的形态）
        let skill = Skill::new("我的技能", "desc", "正文内容");
        assert!(!crate::skills::builtin::is_builtin_skill_id(&skill.id));
        let tool = SkillTool::new(&skill);
        assert!(!tool.prompt.contains("用户当前平台"));
        assert!(tool.prompt.contains("正文内容"));
    }

    fn skill_with_resources() -> Skill {
        let mut skill = Skill::new("demo", "desc", "正文内容");
        skill.resources = vec![
            SkillResource {
                path: "references/a.md".into(),
                content: "AAA".into(),
            },
            SkillResource {
                path: "references/b.md".into(),
                content: "BBB".into(),
            },
        ];
        skill
    }

    #[test]
    fn no_path_returns_prompt_plus_resource_index() {
        let tool = SkillTool::new(&skill_with_resources());
        let out = tool.render(&json!({}));
        assert!(out.success);
        assert!(out.output.contains("正文内容"));
        assert!(out.output.contains("references/a.md"));
        assert!(out.output.contains("references/b.md"));
        assert!(!out.output.contains("AAA"), "无参调用不应内联资源正文");
    }

    #[test]
    fn path_returns_that_resource_content() {
        let tool = SkillTool::new(&skill_with_resources());
        let out = tool.render(&json!({ "path": "references/b.md" }));
        assert!(out.success);
        assert!(out.output.contains("BBB"));
        assert!(!out.output.contains("AAA"));
    }

    #[test]
    fn unknown_path_fails_and_lists_available() {
        let tool = SkillTool::new(&skill_with_resources());
        let out = tool.render(&json!({ "path": "references/nope.md" }));
        assert!(!out.success);
        assert!(out.output.contains("references/a.md"));
    }

    #[test]
    fn path_traversal_is_not_resolvable() {
        let tool = SkillTool::new(&skill_with_resources());
        // 白名单等值查找：任何穿越尝试都只会落到「未知资源」分支
        let out = tool.render(&json!({ "path": "../../etc/passwd" }));
        assert!(!out.success);
    }

    #[test]
    fn dot_slash_prefix_is_tolerated_but_traversal_still_fails() {
        let tool = SkillTool::new(&skill_with_resources());
        let out = tool.render(&json!({ "path": "./references/a.md" }));
        assert!(out.success);
        assert!(out.output.contains("AAA"));
        let out = tool.render(&json!({ "path": "references\\b.md" }));
        assert!(out.success);
        assert!(!tool.render(&json!({ "path": "./../x.md" })).success);
    }

    #[test]
    fn long_skill_name_keeps_tool_name_within_api_limit() {
        let long = "a-very-long-imported-skill-name-".repeat(4);
        let tool = SkillTool::new(&Skill::new(long, "d", "正文"));
        assert!(
            tool.name().len() <= MAX_TOOL_NAME_LEN,
            "got {}",
            tool.name()
        );
        assert!(tool.name().starts_with(SKILL_TOOL_PREFIX));
        assert!(!tool.name().contains("__"));
        // 同名长 skill 仍靠 id 标签区分
        let a = SkillTool::new(&Skill::new("x".repeat(100), "d", "p"));
        let b = SkillTool::new(&Skill::new("x".repeat(100), "d", "p"));
        assert_ne!(a.name(), b.name());
    }

    #[test]
    fn empty_description_has_no_dangling_colon() {
        let tool = SkillTool::new(&Skill::new("plain", "  ", "正文"));
        assert_eq!(tool.description(), "plain");
    }

    #[test]
    fn skill_without_resources_has_no_index_section() {
        let tool = SkillTool::new(&Skill::new("plain", "d", "正文"));
        let out = tool.render(&json!({}));
        assert!(out.output.contains("正文"));
        assert!(!out.output.contains("资源文件"));
    }
}
