use serde::{Deserialize, Serialize};
use tauri::State;

use crate::agent::risk::{
    CustomRiskRule, Disposition, RiskRuleOverride, RuleConfig, RuleKind, RuleScope, SecurityPolicy,
    CUSTOM_REASON, CUSTOM_REASON_VARS, RULES,
};
use crate::agent::task::AgentMode;
use crate::agent::tool_dispatcher::{
    command_requires_confirmation, decide_command, needs_human_confirmation, CommandDecision,
};
use crate::config::settings::AgentModeSettings;
use crate::error::AppError;
use crate::AppState;

/// 命令命中的一条风险规则，给设置页「命令测试」展示。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRuleHit {
    pub rule_id: String,
    pub title: String,
    pub level: Disposition,
}

/// Result of evaluating a command against the current AGENT-mode policy.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandCheckResult {
    pub allowed: bool,
    pub requires_confirmation: bool,
    pub disposition: Disposition,
    pub reason: String,
    /// 决定档位的那条规则（`None` = 档位来自命令名单，或没有任何规则命中）。
    pub rule_id: Option<String>,
    /// 命中的全部风险规则（含没有决定档位、但也命中了的那些）。
    pub hits: Vec<CommandRuleHit>,
}

/// 设置页「测试命令」带来的**草稿**：用户还没点保存的规则也要能测出结果。
/// 字段缺省 = 用已保存的设置。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CheckCommandDraft {
    pub risk_rule_overrides: Option<std::collections::HashMap<String, RiskRuleOverride>>,
    pub custom_risk_rules: Option<Vec<CustomRiskRule>>,
    pub custom_protected_paths: Option<Vec<String>>,
    pub agent_mode_settings: Option<AgentModeSettings>,
}

/// 内置的命令审批系统提示词。设置页用它做默认值展示与「恢复默认」，避免前端
/// 再抄一份——两份逐字节相同的副本会各自漂移（见 `templates/approval/审批.hbs`）。
#[tauri::command]
pub async fn agent_default_approval_prompt() -> String {
    crate::agent::templates::TemplateManager.render_approval_base()
}

/// 内置的 Jev 判据（`templates/approval/审批Jev.hbs`）。与上面那条同理：设置页把它预填在
/// 「Jev 判据」输入框里、用它做「恢复默认」，前端不自带副本。
#[tauri::command]
pub async fn agent_default_jev_prompt() -> String {
    crate::agent::templates::TemplateManager.render_approval_jev()
}

/// 设置页「测试命令」用的预演：这条命令在当前配置下会被怎么处置。
///
/// 它调用 [`crate::agent::tool_dispatcher::decide_command`] —— 和真正执行时
/// **同一份判定**，所以这里显示的结论就是实际会发生的事。这里以前自己抄了一份
/// 名单逻辑、而且完全不看风险评估，测出来的结论和跑起来的行为可以不一样。
#[tauri::command]
pub async fn agent_check_command(
    state: State<'_, AppState>,
    command: String,
    mode: AgentMode,
    draft: Option<CheckCommandDraft>,
) -> Result<CommandCheckResult, AppError> {
    let settings = state.settings.read().await;
    let draft = draft.unwrap_or_default();
    let mut policy = SecurityPolicy::from_app_settings(&settings);
    if let Some(paths) = &draft.custom_protected_paths {
        policy.custom_protected_paths = paths.clone();
    }
    if draft.risk_rule_overrides.is_some() || draft.custom_risk_rules.is_some() {
        policy = policy.with_rules(RuleConfig::new(
            draft
                .risk_rule_overrides
                .clone()
                .unwrap_or_else(|| settings.risk_rule_overrides.clone()),
            draft
                .custom_risk_rules
                .clone()
                .unwrap_or_else(|| settings.custom_risk_rules.clone()),
        ));
    }
    let agent_settings = draft
        .agent_mode_settings
        .as_ref()
        .unwrap_or(&settings.agent_mode_settings);
    let decision = decide_command(command.trim(), &mode, agent_settings, Some(&policy));
    // 档位本身与模式无关，但「这个模式下到底会不会弹窗」还取决于模式与
    // `auto_mode_force_approval`（Auto 的强制审批例外口）。设置页的结论必须与
    // dispatcher 静态判定一致，否则测出来和跑起来又是两回事。
    let requires_confirmation = needs_human_confirmation(
        &mode,
        Some(&decision),
        decision.disposition,
        false,
        agent_settings,
        false,
    );

    let hits = decision
        .hits
        .iter()
        .map(|h| CommandRuleHit {
            rule_id: h.rule_id.clone(),
            title: policy
                .rules
                .title_of(&h.rule_id)
                .unwrap_or_else(|| h.rule_id.clone()),
            level: h.level,
        })
        .collect();

    Ok(CommandCheckResult {
        allowed: decision.disposition != Disposition::Deny,
        requires_confirmation,
        disposition: decision.disposition,
        reason: decision.reason,
        rule_id: decision.rule_id,
        hits,
    })
}

// ──────────────────────────── 审批总览 ────────────────────────────

/// 总览表一个格子的结论：这一档的命令在这个模式下会怎样。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OverviewVerdict {
    /// 弹审批问你。
    Ask,
    /// 直接执行。
    Run,
    /// 不执行（直接拒绝，不征求意见）。
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OverviewCell {
    pub verdict: OverviewVerdict,
    /// 白名单模式下，**在名单里的**命令在这一格的结论；`None` = 与 `verdict` 一样。
    /// 只有「没命中任何规则」那一行的 Agent / 开了审批的 Plan 会出现差别。
    pub listed: Option<OverviewVerdict>,
}

/// 总览表的一行：某一档位的命令在 Agent / Plan / Auto 下各会怎样。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OverviewRow {
    /// `Allow` 在这里表示「没命中任何规则」那一行。
    pub level: Disposition,
    pub agent: OverviewCell,
    pub plan: OverviewCell,
    pub auto: OverviewCell,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalOverview {
    pub rows: Vec<OverviewRow>,
}

/// 探针命令名：只用来让白名单匹配（在 / 不在名单里），不会被风险评估，也不会被执行。
const PROBE_LISTED: &str = "marcel-probe-listed";
const PROBE_UNLISTED: &str = "marcel-probe-unlisted";

fn overview_verdict(
    level: Disposition,
    cmd: &str,
    mode: &AgentMode,
    settings: &AgentModeSettings,
) -> OverviewVerdict {
    if level == Disposition::Deny {
        return OverviewVerdict::Deny;
    }
    // 与 `decide_command` 一样的两步：先算命令这一层要不要确认，再交给真实执行时
    // 用的 `needs_human_confirmation` 按模式定夺。
    let requires_confirmation = command_requires_confirmation(level, cmd, mode, settings);
    let disposition = match level {
        Disposition::ForceApproval => Disposition::ForceApproval,
        _ if requires_confirmation => Disposition::Approval,
        _ => Disposition::Allow,
    };
    let decision = CommandDecision {
        disposition,
        reason: String::new(),
        requires_confirmation,
        rule_id: None,
        rule_title: None,
        hits: Vec::new(),
    };
    if needs_human_confirmation(mode, Some(&decision), disposition, false, settings, false) {
        OverviewVerdict::Ask
    } else {
        OverviewVerdict::Run
    }
}

fn overview_cell(
    level: Disposition,
    mode: &AgentMode,
    settings: &AgentModeSettings,
) -> OverviewCell {
    let verdict = overview_verdict(level, PROBE_UNLISTED, mode, settings);
    // 只有「没命中」一行会受白名单影响：名单内的命令可能与名单外的结论不同。
    let listed = if level == Disposition::Allow {
        let mut with_probe = settings.clone();
        with_probe.allow_commands = vec![PROBE_LISTED.to_string()];
        let listed = overview_verdict(level, PROBE_LISTED, mode, &with_probe);
        (listed != verdict).then_some(listed)
    } else {
        None
    };
    OverviewCell { verdict, listed }
}

/// 总览表：**命令类工具**在当前审批设置下，每一档 × 每个模式会怎样。
///
/// 每个格子都走真实执行用的判定（[`command_requires_confirmation`] +
/// [`needs_human_confirmation`]），不是前端抄的一张表，所以规则、开关一改它自动跟着变。
/// 与风险规则无关——规则只决定一条命令落在哪一行，不影响行里的结论，所以入参只要
/// 审批设置。非命令工具（上传下载、MCP 等）另有一套（工具自带档位 + 「每条都确认」），
/// 不在这张表里。
pub(crate) fn approval_overview(settings: &AgentModeSettings) -> ApprovalOverview {
    let row = |level: Disposition| OverviewRow {
        level,
        agent: overview_cell(level, &AgentMode::Agent, settings),
        plan: overview_cell(level, &AgentMode::Plan, settings),
        auto: overview_cell(level, &AgentMode::Auto, settings),
    };
    ApprovalOverview {
        rows: vec![
            row(Disposition::Deny),
            row(Disposition::ForceApproval),
            row(Disposition::Approval),
            row(Disposition::Allow),
        ],
    }
}

/// 设置页顶部「什么时候问你」总览表。`draft` 是设置页里还没保存的审批设置，缺省用已保存的。
#[tauri::command]
pub async fn agent_approval_overview(
    state: State<'_, AppState>,
    draft: Option<AgentModeSettings>,
) -> Result<ApprovalOverview, AppError> {
    let settings = state.settings.read().await;
    let agent_settings = draft.as_ref().unwrap_or(&settings.agent_mode_settings);
    Ok(approval_overview(agent_settings))
}

// ──────────────────────────── 规则目录 ────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskListInfo {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub defaults: Vec<&'static str>,
}

/// 说明模板里的一个占位：`{name}` 在命中时换成实际值，`hint` 讲它是什么。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasonVar {
    pub name: &'static str,
    pub hint: &'static str,
}

fn reason_vars(vars: &[(&'static str, &'static str)]) -> Vec<ReasonVar> {
    vars.iter()
        .map(|(name, hint)| ReasonVar { name, hint })
        .collect()
}

/// 自定义规则的默认说明（模板 + 占位）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomRuleReason {
    pub reason: &'static str,
    pub reason_vars: Vec<ReasonVar>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskRuleInfo {
    pub id: &'static str,
    pub kind: RuleKind,
    pub scope: RuleScope,
    pub title: &'static str,
    pub description: &'static str,
    pub examples: Vec<&'static str>,
    pub default_level: Disposition,
    pub lists: Vec<RiskListInfo>,
    /// 命中时回给模型的默认说明（模板）。设置页把它预填进「拒绝说明」让用户在它的基础上改。
    pub reason: &'static str,
    pub reason_vars: Vec<ReasonVar>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskRuleCatalog {
    pub rules: Vec<RiskRuleInfo>,
    pub custom_rule: CustomRuleReason,
}

/// 「需要审批的工具」里可以点名的内置工具名（取自工具声明表，新增工具自动出现）。
/// 显示名与分组是呈现问题，由前端按名字配；这里只负责「有哪些」。
#[tauri::command]
pub async fn agent_list_approvable_tools() -> Vec<String> {
    crate::agent::tools::approvable_tool_names()
}

/// 全部内置命令风险规则（说明、默认档位、可编辑名单的默认值）。
///
/// 只给静态目录；用户的改动在 `settings.riskRuleOverrides` / `customRiskRules` 里，
/// 由设置页自己合并。规则文案只此一份（前端不抄），所以新增规则不用改前端。
#[tauri::command]
pub async fn agent_list_risk_rules() -> RiskRuleCatalog {
    RiskRuleCatalog {
        rules: RULES
            .iter()
            .map(|r| RiskRuleInfo {
                id: r.id,
                kind: r.kind,
                scope: r.scope,
                title: r.title,
                description: r.description,
                examples: r.examples.to_vec(),
                default_level: r.default_level,
                lists: r
                    .lists
                    .iter()
                    .map(|l| RiskListInfo {
                        key: l.key,
                        label: l.label,
                        hint: l.hint,
                        defaults: l.defaults.to_vec(),
                    })
                    .collect(),
                reason: r.reason,
                reason_vars: reason_vars(r.reason_vars),
            })
            .collect(),
        custom_rule: CustomRuleReason {
            reason: CUSTOM_REASON,
            reason_vars: reason_vars(CUSTOM_REASON_VARS),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::CommandListMode;

    /// 一行一条真实命令：总览表的每个格子必须等于「这条命令真跑起来会发生的事」
    /// （`decide_command` + `needs_human_confirmation`，dispatch 读的就是它们）。
    /// 规则或开关怎么改，只要表和真实判定对不上，这里就变红。
    fn actual(cmd: &str, mode: &AgentMode, s: &AgentModeSettings) -> OverviewVerdict {
        let d = decide_command(cmd, mode, s, None);
        if d.disposition == Disposition::Deny {
            return OverviewVerdict::Deny;
        }
        if needs_human_confirmation(mode, Some(&d), d.disposition, false, s, false) {
            OverviewVerdict::Ask
        } else {
            OverviewVerdict::Run
        }
    }

    fn every_settings_combo() -> Vec<AgentModeSettings> {
        let mut out = Vec::new();
        for allowlist in [false, true] {
            for confirm_each in [false, true] {
                for plan in [false, true] {
                    for auto_force in [false, true] {
                        out.push(AgentModeSettings {
                            list_mode: if allowlist {
                                CommandListMode::Allowlist
                            } else {
                                CommandListMode::Denylist
                            },
                            allow_commands: vec!["ls".to_string()],
                            confirm_each_command: confirm_each,
                            plan_mode_requires_approval: plan,
                            auto_mode_force_approval: auto_force,
                            ..AgentModeSettings::default()
                        });
                    }
                }
            }
        }
        out
    }

    fn cell_of(row: &OverviewRow, mode: &AgentMode) -> OverviewCell {
        match mode {
            AgentMode::Agent => row.agent,
            AgentMode::Plan => row.plan,
            AgentMode::Auto => row.auto,
        }
    }

    #[test]
    fn overview_cells_equal_what_real_commands_do() {
        // (档位, 该档位的真实命令)；「没命中」另外在下面分名单内 / 名单外验。
        let by_level = [
            (Disposition::Deny, "rm -rf /etc"),
            (Disposition::ForceApproval, "systemctl restart nginx"),
            (
                Disposition::Approval,
                "sudo -n tail -n 20 /var/log/nginx/access.log",
            ),
        ];
        for s in every_settings_combo() {
            let overview = approval_overview(&s);
            for mode in [AgentMode::Agent, AgentMode::Plan, AgentMode::Auto] {
                for (level, cmd) in by_level {
                    let row = overview.rows.iter().find(|r| r.level == level).unwrap();
                    let cell = cell_of(row, &mode);
                    assert_eq!(cell.listed, None, "{level:?} 这一行不受白名单影响");
                    assert_eq!(
                        cell.verdict,
                        actual(cmd, &mode, &s),
                        "{level:?} × {mode:?}，`{cmd}`，设置 {s:?}"
                    );
                }
                let none = overview
                    .rows
                    .iter()
                    .find(|r| r.level == Disposition::Allow)
                    .unwrap();
                let cell = cell_of(none, &mode);
                // 名单外的命令（`uname` 不在名单里，也不命中任何规则）。
                assert_eq!(
                    cell.verdict,
                    actual("uname -a", &mode, &s),
                    "没命中 × {mode:?}（名单外），设置 {s:?}"
                );
                // 名单内的命令（`ls`）：`listed` 为 `None` 就是与名单外相同。
                assert_eq!(
                    cell.listed.unwrap_or(cell.verdict),
                    actual("ls -la", &mode, &s),
                    "没命中 × {mode:?}（名单内），设置 {s:?}"
                );
            }
        }
    }

    #[test]
    fn overview_defaults_match_the_documented_behaviour() {
        use OverviewVerdict::{Ask, Deny, Run};
        // 「按规则」：没命中规则的命令直接执行（出厂是「每条都确认」，那一行 Agent 下全是问你）。
        let s = AgentModeSettings {
            confirm_each_command: false,
            ..AgentModeSettings::default()
        };
        let o = approval_overview(&s);
        let verdicts = |level: Disposition| {
            let r = o.rows.iter().find(|r| r.level == level).unwrap();
            (r.agent.verdict, r.plan.verdict, r.auto.verdict)
        };
        assert_eq!(verdicts(Disposition::Deny), (Deny, Deny, Deny));
        assert_eq!(verdicts(Disposition::ForceApproval), (Ask, Run, Run));
        // 出厂状态下「请求审批」与「强制审批」行为一样，区别只在 Auto 的开关。
        assert_eq!(verdicts(Disposition::Approval), (Ask, Run, Run));
        assert_eq!(verdicts(Disposition::Allow), (Run, Run, Run));

        // 出厂默认是「每条都确认」：没命中的命令在 Agent 下也问，Plan / Auto 照旧不问。
        let factory = approval_overview(&AgentModeSettings::default());
        let none = factory
            .rows
            .iter()
            .find(|r| r.level == Disposition::Allow)
            .unwrap();
        assert_eq!(
            (none.agent.verdict, none.plan.verdict, none.auto.verdict),
            (Ask, Run, Run)
        );
    }

    #[test]
    fn auto_force_toggle_only_changes_the_auto_force_cell() {
        use OverviewVerdict::{Ask, Run};
        let s = AgentModeSettings {
            auto_mode_force_approval: true,
            ..AgentModeSettings::default()
        };
        let o = approval_overview(&s);
        let get = |level: Disposition| {
            o.rows
                .iter()
                .find(|r| r.level == level)
                .unwrap()
                .auto
                .verdict
        };
        assert_eq!(get(Disposition::ForceApproval), Ask);
        assert_eq!(
            get(Disposition::Approval),
            Run,
            "请求审批在 Auto 下始终不问"
        );
        assert_eq!(get(Disposition::Allow), Run);
    }

    #[test]
    fn allowlist_splits_the_unmatched_row_into_listed_and_unlisted() {
        use OverviewVerdict::{Ask, Run};
        let s = AgentModeSettings {
            confirm_each_command: false,
            list_mode: CommandListMode::Allowlist,
            ..AgentModeSettings::default()
        };
        let o = approval_overview(&s);
        let none = o
            .rows
            .iter()
            .find(|r| r.level == Disposition::Allow)
            .unwrap();
        assert_eq!(none.agent.verdict, Ask, "名单外要问");
        assert_eq!(none.agent.listed, Some(Run), "名单内直接执行");
        // 规则与名单互不压制：请求审批档不因白名单而变。
        let approval = o
            .rows
            .iter()
            .find(|r| r.level == Disposition::Approval)
            .unwrap();
        assert_eq!(approval.agent.listed, None);
        assert_eq!(approval.agent.verdict, Ask);
    }
}
