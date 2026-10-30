//! 作用域规则：哪些叶子跨设备同步、哪些只留在本机、哪些按平台分开同步。
//!
//! 默认全部 `Shared`——新加的配置字段默认就会同步（上层有「新字段守护」测试逼着开发者
//! 表态）。例外在每个同步源的 [`ScopeRules`] 里声明：
//! - `Local`：只留本机（窗口尺寸、本机路径、本机授权……），既不上报也不接收；
//! - `Platform`：桌面与手机各一份（通知设置这种两端形态不同的东西），路径前面加 `@desktop` / `@android`；
//! - [`ItemFilter`]：对 id 映射列表里的**单个元素**挑字段——内置 skill 只同步 `enabled`，
//!   `mcp-temp-` 开头的临时连接整条不同步。
//!
//! 规则既用在上报（`flatten_scoped` / `diff`），也用在落地（`apply_to_doc`）：
//! 落地时同样要拒绝写入本机认为是 Local 的路径，这样即便另一台设备的版本对作用域的看法不同，
//! 也碰不到本机专属字段。

use serde_json::Value;

use super::path::Seg;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    Shared,
    Local,
    Platform,
}

/// 对某个元素 [`ItemFilter`] 的裁决。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKeep {
    /// 元素的全部字段参与同步（没有过滤器命中，或过滤器说「全留」）。
    All,
    /// 只有这些顶层字段参与同步；空名单 = 整个元素都是本机的，连同它在顺序里的位置都不同步。
    Only(&'static [&'static str]),
}

pub struct ItemFilter {
    /// 列表所在路径的模式（`/` 分隔，`*` 匹配任意一段），如 `"skills"`、`"servers"`。
    pub list: &'static str,
    /// 看元素内容决定留哪些字段：`None` = 全留，`Some(名单)` = 只留名单里的顶层字段。
    pub keep_only: fn(&Value) -> Option<&'static [&'static str]>,
}

pub struct ScopeRules {
    /// `(路径模式, 作用域)`。模式用 `/` 分隔，`*` 匹配任意一段（含 id 段），前缀匹配
    /// （`connections/*/keyPath` 覆盖它的整个子树）。多条命中取**最长**的，等长取先写的。
    pub patterns: &'static [(&'static str, Scope)],
    pub item_filters: &'static [ItemFilter],
}

impl ScopeRules {
    /// 一切共享、没有任何过滤（测试与「全量同步」的源用）。
    pub const SHARED_ALL: ScopeRules = ScopeRules {
        patterns: &[],
        item_filters: &[],
    };

    /// 路径（**不含**平台前缀的文档内路径）的作用域。
    pub fn scope_of(&self, path: &[Seg]) -> Scope {
        let mut best: Option<(usize, Scope)> = None;
        for (pat, scope) in self.patterns {
            if let Some(n) = prefix_match_len(pat, path) {
                if best.map_or(true, |(bn, _)| n > bn) {
                    best = Some((n, *scope));
                }
            }
        }
        best.map_or(Scope::Shared, |(_, s)| s)
    }

    /// 位于 `list_path` 的 id 映射列表里的某个元素，哪些字段参与同步。
    pub fn item_keep(&self, list_path: &[Seg], element: &Value) -> ItemKeep {
        for f in self.item_filters {
            if exact_match(f.list, list_path) {
                if let Some(fields) = (f.keep_only)(element) {
                    return ItemKeep::Only(fields);
                }
            }
        }
        ItemKeep::All
    }
}

fn seg_matches(pat_seg: &str, seg: &Seg) -> bool {
    match seg {
        Seg::Key(k) => pat_seg == "*" || pat_seg == k,
        Seg::Id { .. } => pat_seg == "*",
        // 顺序叶子不被任何通配吞掉：`skills/*` 说的是「每个元素」，不是顺序叶子
        Seg::Order { .. } => false,
    }
}

/// 模式是路径的前缀时返回模式的段数。
fn prefix_match_len(pat: &str, path: &[Seg]) -> Option<usize> {
    if pat.is_empty() {
        return None;
    }
    let mut n = 0;
    for (i, ps) in pat.split('/').enumerate() {
        let seg = path.get(i)?;
        if !seg_matches(ps, seg) {
            return None;
        }
        n = i + 1;
    }
    Some(n)
}

/// 模式与路径逐段完全匹配（长度必须相等）。
fn exact_match(pat: &str, path: &[Seg]) -> bool {
    if pat.is_empty() {
        // 空模式 = 文档根（根本身就是 id 映射数组的源）
        return path.is_empty();
    }
    // prefix_match_len 返回的是模式的段数，等于路径长度即逐段完全匹配
    prefix_match_len(pat, path) == Some(path.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn k(s: &str) -> Seg {
        Seg::key(s)
    }
    fn id(s: &str) -> Seg {
        Seg::id(s)
    }

    static RULES: ScopeRules = ScopeRules {
        patterns: &[
            ("window", Scope::Local),
            ("window/shared", Scope::Shared),
            ("connections/*/keyPath", Scope::Local),
            ("notify", Scope::Platform),
            ("items/*", Scope::Shared),
        ],
        item_filters: &[ItemFilter {
            list: "skills",
            keep_only: |el| {
                if el
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|i| i.starts_with("builtin-"))
                {
                    Some(&["enabled"])
                } else if el
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|i| i.starts_with("tmp-"))
                {
                    Some(&[])
                } else {
                    None
                }
            },
        }],
    };

    #[test]
    fn default_is_shared() {
        assert_eq!(RULES.scope_of(&[k("theme")]), Scope::Shared);
        assert_eq!(RULES.scope_of(&[]), Scope::Shared);
        assert_eq!(
            ScopeRules::SHARED_ALL.scope_of(&[k("anything")]),
            Scope::Shared
        );
    }

    #[test]
    fn prefix_match_covers_subtree() {
        assert_eq!(RULES.scope_of(&[k("window")]), Scope::Local);
        assert_eq!(RULES.scope_of(&[k("window"), k("w")]), Scope::Local);
        assert_eq!(
            RULES.scope_of(&[k("window"), k("a"), id("x"), k("b")]),
            Scope::Local
        );
        // 模式比路径长则不命中
        assert_eq!(RULES.scope_of(&[k("connections"), id("c1")]), Scope::Shared);
    }

    #[test]
    fn longest_pattern_wins() {
        assert_eq!(RULES.scope_of(&[k("window"), k("shared")]), Scope::Shared);
        assert_eq!(
            RULES.scope_of(&[k("window"), k("shared"), k("x")]),
            Scope::Shared
        );
        assert_eq!(RULES.scope_of(&[k("window"), k("other")]), Scope::Local);
    }

    #[test]
    fn wildcard_matches_key_and_id_but_not_order() {
        assert_eq!(
            RULES.scope_of(&[k("connections"), id("c1"), k("keyPath")]),
            Scope::Local
        );
        assert_eq!(
            RULES.scope_of(&[k("connections"), k("anykey"), k("keyPath")]),
            Scope::Local
        );
        assert_eq!(
            RULES.scope_of(&[k("connections"), id("c1"), k("host")]),
            Scope::Shared
        );
        // 顺序叶子不被 `*` 吞掉
        assert_eq!(
            RULES.scope_of(&[k("connections"), Seg::ORDER, k("keyPath")]),
            Scope::Shared
        );
    }

    #[test]
    fn platform_scope() {
        assert_eq!(RULES.scope_of(&[k("notify"), k("sound")]), Scope::Platform);
    }

    #[test]
    fn item_filter_picks_fields_by_element() {
        let list = [k("skills")];
        assert_eq!(
            RULES.item_keep(&list, &json!({"id": "builtin-a", "name": "x"})),
            ItemKeep::Only(&["enabled"])
        );
        assert_eq!(
            RULES.item_keep(&list, &json!({"id": "tmp-1"})),
            ItemKeep::Only(&[])
        );
        assert_eq!(
            RULES.item_keep(&list, &json!({"id": "user-1"})),
            ItemKeep::All
        );
        // 列表路径不匹配
        assert_eq!(
            RULES.item_keep(&[k("other")], &json!({"id": "builtin-a"})),
            ItemKeep::All
        );
        // 过滤器只认精确的列表位置，不认它的子位置
        assert_eq!(
            RULES.item_keep(
                &[k("skills"), id("s"), k("sub")],
                &json!({"id": "builtin-a"})
            ),
            ItemKeep::All
        );
    }

    #[test]
    fn empty_pattern_never_matches() {
        static R: ScopeRules = ScopeRules {
            patterns: &[("", Scope::Local)],
            item_filters: &[],
        };
        assert_eq!(R.scope_of(&[k("a")]), Scope::Shared);
    }
}
