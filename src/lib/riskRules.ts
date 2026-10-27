/**
 * 命令风险规则的前端逻辑 —— 把「后端规则目录 + 用户覆盖项」合成界面要显示的状态，
 * 以及把界面上的每一种改动翻译回「只存差异」的覆盖项。
 *
 * 规则的说明、默认档位、名单默认值都在后端（`agent/risk/rules.rs`，经
 * `agentListRiskRules` 取得），这里**不抄**任何规则文案。
 */
import type {
  CommandCheckResult,
  CustomRiskRule,
  Disposition,
  RiskRuleCatalog,
  RiskRuleInfo,
  RiskRuleOverride,
} from './types';

export type RiskRuleOverrides = Record<string, RiskRuleOverride>;

/** 档位下拉里的取值（规则可选全部四档，自定义规则不含「放行」——放行等于没规则）。 */
export const RULE_LEVELS: Disposition[] = [
  'Allow',
  'Approval',
  'ForceApproval',
  'Deny',
];
export const CUSTOM_RULE_LEVELS: Disposition[] = [
  'Approval',
  'ForceApproval',
  'Deny',
];

/** 档位在规则页里的叫法：「放行」强调的是"这条规则不拦"。 */
export const RULE_LEVEL_LABELS: Record<Disposition, string> = {
  Allow: '放行',
  Approval: '请求审批',
  ForceApproval: '强制审批',
  Deny: '直接拒绝',
};

/** 各档位的小标签配色：放行绿、请求审批黄、强制审批橙、拒绝红。 */
export const RULE_LEVEL_TONE: Record<Disposition, string> = {
  Allow: 'bg-emerald-500/10 text-emerald-300 border-emerald-700/40',
  Approval: 'bg-amber-500/10 text-amber-300 border-amber-700/40',
  ForceApproval: 'bg-orange-500/10 text-orange-300 border-orange-700/40',
  Deny: 'bg-red-500/10 text-red-300 border-red-700/40',
};

/** 档位的小圆点颜色（下拉选项里用，一眼分出松紧）。 */
export const RULE_LEVEL_DOT: Record<Disposition, string> = {
  Allow: 'bg-emerald-400',
  Approval: 'bg-amber-400',
  ForceApproval: 'bg-orange-400',
  Deny: 'bg-red-400',
};

/**
 * 每一档一句话的含义（档位下拉里每个选项下面的小字），必须与后端判定一致。
 *
 * 「请求审批」与「强制审批」的区别只在 Auto：Auto 默认两档都不问，打开「Auto 模式强制审批」
 * 后只有强制审批会问。Plan 默认也不问，打开「Plan 模式也需要审批」才跟 Agent 一样。
 * 具体每个模式下怎样，以设置页顶部的「什么时候问你」表为准（那张表由后端算出）。
 */
export const LEVEL_HINT: Record<Disposition, string> = {
  Allow: '不拦',
  Approval: 'Agent 下问你；Plan 开了审批也问；Auto 不问',
  ForceApproval: 'Agent 下问你；Auto 默认不问，开了强制审批才问',
  Deny: '不执行，让模型换写法',
};

const LEVEL_RANK: Record<Disposition, number> = {
  Allow: 0,
  Approval: 1,
  ForceApproval: 2,
  Deny: 3,
};

/** 当前档位是否比默认更宽松（界面据此给一句行内提醒；变严不提醒）。 */
export function isLoosened(
  defaultLevel: Disposition,
  level: Disposition,
): boolean {
  return LEVEL_RANK[level] < LEVEL_RANK[defaultLevel];
}

/** 说明只含空白等于没写（与后端一致：trim 后为空就用默认说明）。 */
function isBlank(s: string | undefined): boolean {
  return !s || s.trim() === '';
}

/** 一条覆盖项是否真的改了东西（空对象等同于没改）。 */
export function isOverrideEmpty(o: RiskRuleOverride | undefined): boolean {
  if (!o) return true;
  return (
    o.enabled === undefined &&
    o.level === undefined &&
    isBlank(o.message) &&
    (!o.lists || Object.keys(o.lists).length === 0)
  );
}

export function isRuleModified(o: RiskRuleOverride | undefined): boolean {
  return !isOverrideEmpty(o);
}

/** 规则当前的档位：关闭 = 放行，否则覆盖项优先、默认兜底。 */
export function effectiveLevel(
  rule: RiskRuleInfo,
  o: RiskRuleOverride | undefined,
): Disposition {
  if (o?.enabled === false) return 'Allow';
  return o?.level ?? rule.defaultLevel;
}

/** 豁免当前是否生效。 */
export function isExemptionOn(o: RiskRuleOverride | undefined): boolean {
  return o?.enabled !== false;
}

/** 名单当前的条目（覆盖项整体替换默认）。 */
export function effectiveList(
  rule: RiskRuleInfo,
  listKey: string,
  o: RiskRuleOverride | undefined,
): string[] {
  const custom = o?.lists?.[listKey];
  if (custom) return custom;
  return rule.lists.find((l) => l.key === listKey)?.defaults ?? [];
}

/** 按名单 key 找当前条目（跨规则，key 全局唯一）。 */
export function effectiveListByKey(
  catalog: RiskRuleCatalog | null,
  overrides: RiskRuleOverrides,
  listKey: string,
): string[] | null {
  if (!catalog) return null;
  for (const rule of catalog.rules) {
    if (rule.lists.some((l) => l.key === listKey)) {
      return effectiveList(rule, listKey, overrides[rule.id]);
    }
  }
  return null;
}

export function isListModified(
  rule: RiskRuleInfo,
  listKey: string,
  o: RiskRuleOverride | undefined,
): boolean {
  return o?.lists?.[listKey] !== undefined;
}

/** 名单条目规整：去空白、去空串、去重（保持首次出现的顺序）。 */
export function normalizeListItems(items: string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of items) {
    const v = raw.trim();
    if (v && !seen.has(v)) {
      seen.add(v);
      out.push(v);
    }
  }
  return out;
}

function sameItems(a: string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

/** 把一条覆盖项里与默认相同的部分剔掉；整条为空则返回 undefined（调用方据此删键）。 */
function tidy(
  rule: RiskRuleInfo,
  o: RiskRuleOverride,
): RiskRuleOverride | undefined {
  const next: RiskRuleOverride = { ...o };
  if (next.level === rule.defaultLevel) delete next.level;
  if (next.enabled === true) delete next.enabled;
  // 不 trim 存进去的文本：用户正在输入时末尾的空格不能被吃掉；只有纯空白、
  // 或与默认原话一字不差才算「没改」。
  if (isBlank(next.message) || next.message === rule.reason) delete next.message;
  if (next.lists) {
    const lists: Record<string, string[]> = {};
    for (const [key, items] of Object.entries(next.lists)) {
      const def = rule.lists.find((l) => l.key === key);
      if (def && !sameItems(items, def.defaults)) lists[key] = items;
    }
    if (Object.keys(lists).length > 0) next.lists = lists;
    else delete next.lists;
  }
  return isOverrideEmpty(next) ? undefined : next;
}

function put(
  overrides: RiskRuleOverrides,
  rule: RiskRuleInfo,
  o: RiskRuleOverride,
): RiskRuleOverrides {
  const next = { ...overrides };
  const tidied = tidy(rule, o);
  if (tidied) next[rule.id] = tidied;
  else delete next[rule.id];
  return next;
}

/** 改一条规则的档位。选「放行」存成 level=Allow（效果与关闭相同，界面上更直观）。 */
export function setRuleLevel(
  overrides: RiskRuleOverrides,
  rule: RiskRuleInfo,
  level: Disposition,
): RiskRuleOverrides {
  const cur = overrides[rule.id] ?? {};
  // 选了新档位 = 明确要这条规则按这个档位生效，之前"关闭"的状态一并作废。
  const { enabled: _enabled, ...rest } = cur;
  return put(overrides, rule, { ...rest, level });
}

export function setExemptionOn(
  overrides: RiskRuleOverrides,
  rule: RiskRuleInfo,
  on: boolean,
): RiskRuleOverrides {
  const cur = overrides[rule.id] ?? {};
  return put(overrides, rule, { ...cur, enabled: on });
}

/** 整体替换一个名单；`items === null` = 恢复默认。 */
export function setRuleList(
  overrides: RiskRuleOverrides,
  rule: RiskRuleInfo,
  listKey: string,
  items: string[] | null,
): RiskRuleOverrides {
  const cur = overrides[rule.id] ?? {};
  const lists = { ...(cur.lists ?? {}) };
  if (items === null) delete lists[listKey];
  else lists[listKey] = normalizeListItems(items);
  return put(overrides, rule, { ...cur, lists });
}

/** 写（或清空）一条内置规则的「拒绝说明」；只在档位是直接拒绝时生效，后端据此判断。 */
export function setRuleMessage(
  overrides: RiskRuleOverrides,
  rule: RiskRuleInfo,
  message: string,
): RiskRuleOverrides {
  const cur = overrides[rule.id] ?? {};
  return put(overrides, rule, { ...cur, message });
}

export function resetRule(
  overrides: RiskRuleOverrides,
  ruleId: string,
): RiskRuleOverrides {
  const next = { ...overrides };
  delete next[ruleId];
  return next;
}

export function newCustomRuleId(): string {
  const rand =
    typeof crypto !== 'undefined' && 'randomUUID' in crypto
      ? crypto.randomUUID()
      : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
  return `custom-${rand}`;
}

/** 自定义规则给人看的名字：写了名称用名称，否则「命令 参数」（与后端 `display_name` 同口径）。 */
export function customRuleLabel(rule: CustomRiskRule): string {
  const name = (rule.name ?? '').trim();
  if (name) return name;
  const arg = rule.argContains.trim();
  return arg ? `${rule.command.trim()} ${arg}` : rule.command.trim();
}

/**
 * 出厂自带的自定义规则：原「禁止的命令」名单里的五个，现在是普通规则，看得到、删得掉。
 * 与后端 `default_custom_risk_rules` 一致（id 也一致，规则命中时靠 id 对应）。
 */
export function defaultCustomRiskRules(): CustomRiskRule[] {
  return ['rm', 'mkfs', 'dd', 'shutdown', 'reboot'].map((command) => ({
    id: `default-${command}`,
    name: '',
    command,
    argContains: '',
    level: 'Approval',
    enabled: true,
    message: '',
  }));
}

export function emptyCustomRule(): CustomRiskRule {
  return {
    id: newCustomRuleId(),
    name: '',
    command: '',
    argContains: '',
    level: 'ForceApproval',
    enabled: true,
    message: '',
  };
}

/** 一条自定义规则能不能存：命令名必填，且不能含空白（按基础命令名匹配）。 */
export function validateCustomRule(rule: CustomRiskRule): string | null {
  const cmd = rule.command.trim();
  if (!cmd) return '请填写命令名';
  if (/\s/.test(cmd))
    return '命令名只填基础命令（如 docker），参数写在「参数包含」里';
  return null;
}

/** 搜索：标题、说明、id、示例、名单条目、当前档位名里任一包含关键字。 */
export function ruleMatchesQuery(
  rule: RiskRuleInfo,
  o: RiskRuleOverride | undefined,
  query: string,
): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  const hay = [
    rule.title,
    rule.description,
    rule.id,
    ...rule.examples,
    RULE_LEVEL_LABELS[effectiveLevel(rule, o)],
    ...rule.lists.flatMap((l) => [l.label, ...effectiveList(rule, l.key, o)]),
  ];
  return hay.some((s) => s.toLowerCase().includes(q));
}

/** 有多少条内置规则被改过（设置页标题旁的提示用）。 */
export function modifiedRuleCount(overrides: RiskRuleOverrides): number {
  return Object.values(overrides).filter(isRuleModified).length;
}

// ───────────── 命令测试的结论 ─────────────

export type CommandOutcome = 'run' | 'ask' | 'deny';

export const OUTCOME_LABELS: Record<CommandOutcome, string> = {
  run: '直接执行',
  ask: '先问你',
  deny: '拒绝',
};

export type CheckMode = 'agent' | 'plan' | 'auto';

export const CHECK_MODES: { mode: CheckMode; label: string }[] = [
  { mode: 'agent', label: 'Agent' },
  { mode: 'plan', label: 'Plan' },
  { mode: 'auto', label: 'Auto' },
];

/** 一次检查的结论：被拒绝 > 要人确认 > 直接执行。 */
export function outcomeOf(
  r: Pick<CommandCheckResult, 'allowed' | 'requiresConfirmation'>,
): CommandOutcome {
  if (!r.allowed) return 'deny';
  return r.requiresConfirmation ? 'ask' : 'run';
}

/** 把三种模式的结论按结论合并：「先问你 Agent」「直接执行 Plan · Auto」。 */
export function groupOutcomes(
  results: { mode: CheckMode; result: CommandCheckResult }[],
): { outcome: CommandOutcome; modes: string[]; all: boolean }[] {
  const groups = new Map<CommandOutcome, string[]>();
  for (const { mode, result } of results) {
    const label = CHECK_MODES.find((m) => m.mode === mode)?.label ?? mode;
    const outcome = outcomeOf(result);
    groups.set(outcome, [...(groups.get(outcome) ?? []), label]);
  }
  return [...groups.entries()].map(([outcome, modes]) => ({
    outcome,
    modes,
    all: modes.length === results.length,
  }));
}

// ───────────── 受保护路径 ─────────────

/** 受保护路径规则（`rules.rs`）里承载目录名单的规则 id 与名单 key。 */
export const PROTECTED_PATHS_RULE = 'protected_paths';
export const PROTECTED_PATHS_LIST = 'protected_paths';

/** 新增自定义路径的本地预检（后端另有校验）：已存在就不加。 */
export function preCheckCustomPath(
  trimmed: string,
  existing: string[],
): string | null {
  if (!trimmed) return null;
  if (existing.includes(trimmed)) return `路径已存在：${trimmed}`;
  return null;
}
