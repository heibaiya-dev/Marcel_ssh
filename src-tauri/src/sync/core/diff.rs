//! 本机改动捕获：基线（上次同步时本机自己序列化出的文档）→ 当前文档，产出要上报的 op。
//!
//! **只比基线，不比镜像**。基线是本机代码自己上次序列化的结果：旧版本不认识的新字段在基线和
//! 当前文档里都不存在，所以永远不会被它「删掉」——这是版本兼容的根基。如果改成和镜像比，
//! 旧版本一启动就会把新版本写进镜像的字段当成「本机没有」而发 Remove，新旧版本互相拆台。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::flatten::{flatten_with, learn_id_maps};
use super::hlc::HlcClock;
use super::mirror::{Op, OpKind};
use super::path::{IdMapRegistry, Path, Platform, Seg};
use super::scope::ScopeRules;

/// 一次捕获的上下文。
pub struct DiffCtx<'a> {
    pub rules: &'a ScopeRules,
    pub platform: Platform,
    /// 本源的 id 映射登记表。会被更新（学到当前文档里新出现的 id 映射位置），调用方要把它落盘。
    pub idmaps: &'a mut IdMapRegistry,
    pub clock: &'a mut HlcClock,
    /// 当前墙钟（Unix 毫秒），测试可注入；生产用 `hlc::wall_clock_ms()`。
    pub wall_ms: u64,
    /// 版本刚升级后的第一次捕获：**不发 `Remove`**（`Tombstone` 照发）。新版本删掉 / 迁移了某个
    /// 结构字段时，不该让还在用旧字段的旧版本设备跟着删。
    pub suppress_removes: bool,
}

/// 基线 → 当前 的差异。基线为 `Value::Null`（或空对象）时就是「全量导出」：每个叶子一个 `Set`。
///
/// - 当前有、基线没有或值不同 → `Set`；
/// - 基线有、当前没有：它所在的某个 id 映射元素在当前文档里整个消失了 → 对**最外层**消失的元素发
///   一个 `Tombstone`（其名下叶子不再逐个发 `Remove`）；否则发 `Remove`；
/// - 同一批改动用同一个时间戳；没有任何改动时不推进时钟，返回空。
pub fn diff(source: &str, baseline: &Value, current: &Value, ctx: &mut DiffCtx<'_>) -> Vec<Op> {
    // 基线用「基线自己的登记表」展开，当前文档用学完之后的登记表展开。
    // 区别只在一种情形：某个数组位置在这两次捕获之间第一次变成 id 映射（比如 `tags: []` →
    // `tags: [{id,..}]`）。基线当时它是个原子叶子 `[]`（镜像里确实有这么个叶子），所以这里要让它
    // 作为叶子出现在基线里、不出现在当前里，从而发出对那个旧叶子的 Remove，免得它在镜像里留成幽灵。
    learn_id_maps(baseline, ctx.idmaps);
    let baseline_reg = ctx.idmaps.clone();
    learn_id_maps(current, ctx.idmaps);

    let base = flatten_with(baseline, &baseline_reg, Some(ctx.rules), ctx.platform);
    let cur = flatten_with(current, ctx.idmaps, Some(ctx.rules), ctx.platform);

    let mut sets: Vec<(Path, Value)> = Vec::new();
    for (p, v) in &cur {
        if base.get(p) != Some(v) {
            sets.push((p.clone(), v.clone()));
        }
    }

    // 当前文档里还存在的元素（任一叶子的任一 Id 前缀）
    let mut cur_elems: BTreeSet<&[Seg]> = BTreeSet::new();
    for p in cur.keys() {
        for (i, seg) in p.iter().enumerate() {
            if seg.is_id() {
                cur_elems.insert(&p[..=i]);
            }
        }
    }

    let mut removes: Vec<Path> = Vec::new();
    let mut tombstones: BTreeSet<Path> = BTreeSet::new();
    for p in base.keys() {
        if cur.contains_key(p) {
            continue;
        }
        let vanished = p
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_id())
            .map(|(i, _)| &p[..=i])
            .find(|pre| !cur_elems.contains(pre)); // 从外向内，第一个消失的就是最外层
        match vanished {
            Some(e) => {
                tombstones.insert(e.to_vec());
            }
            None => {
                if !ctx.suppress_removes {
                    removes.push(p.clone());
                }
            }
        }
    }

    if sets.is_empty() && removes.is_empty() && tombstones.is_empty() {
        return Vec::new();
    }

    let hlc = ctx.clock.now(ctx.wall_ms);
    let mk = |path: Path, kind: OpKind| Op {
        source: source.to_string(),
        path,
        kind,
        hlc: hlc.clone(),
    };
    let mut ops = Vec::with_capacity(sets.len() + removes.len() + tombstones.len());
    ops.extend(sets.into_iter().map(|(p, v)| mk(p, OpKind::Set(v))));
    ops.extend(removes.into_iter().map(|p| mk(p, OpKind::Remove)));
    ops.extend(tombstones.into_iter().map(|p| mk(p, OpKind::Tombstone)));
    ops
}

/// 把整份文档导出成 `Set` op，时间戳取自本机时钟（开启同步时用）。
/// 就是「空基线的 [`diff`]」，单独给个入口是免得调用方去记这个约定。
pub fn export_all(source: &str, doc: &Value, ctx: &mut DiffCtx<'_>) -> Vec<Op> {
    diff(source, &Value::Null, doc, ctx)
}

/// 以**指定时间戳**把整份文档导出成 `Set` op（加入同步时用 `Hlc::zero_for(本机)`，见规格 4.6）。
/// 与 [`export_all`] 的区别只是 hlc 不取自时钟，也不推进时钟。
pub fn export_all_at(
    source: &str,
    doc: &Value,
    rules: &ScopeRules,
    platform: Platform,
    idmaps: &mut IdMapRegistry,
    hlc: &super::hlc::Hlc,
) -> Vec<Op> {
    learn_id_maps(doc, idmaps);
    let leaves: BTreeMap<Path, Value> = flatten_with(doc, idmaps, Some(rules), platform);
    leaves
        .into_iter()
        .map(|(path, v)| Op {
            source: source.to_string(),
            path,
            kind: OpKind::Set(v),
            hlc: hlc.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::core::hlc::Hlc;
    use crate::sync::core::scope::{ItemFilter, Scope};
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
            ("conns/*/keyPath", Scope::Local),
            ("notify", Scope::Platform),
        ],
        item_filters: &[ItemFilter {
            list: "skills",
            keep_only: |el| match el.get("id").and_then(Value::as_str) {
                Some(i) if i.starts_with("builtin-") => Some(&["enabled"]),
                _ => None,
            },
        }],
    };

    struct Env {
        idmaps: IdMapRegistry,
        clock: HlcClock,
        wall: u64,
    }

    impl Env {
        fn new() -> Self {
            Env {
                idmaps: IdMapRegistry::new(),
                clock: HlcClock::new("A"),
                wall: 1000,
            }
        }
        fn diff(&mut self, base: &Value, cur: &Value) -> Vec<Op> {
            self.diff_opts(base, cur, false)
        }
        fn diff_opts(&mut self, base: &Value, cur: &Value, suppress: bool) -> Vec<Op> {
            self.wall += 10;
            let mut ctx = DiffCtx {
                rules: &RULES,
                platform: Platform::Desktop,
                idmaps: &mut self.idmaps,
                clock: &mut self.clock,
                wall_ms: self.wall,
                suppress_removes: suppress,
            };
            diff("src", base, cur, &mut ctx)
        }
    }

    fn find<'a>(ops: &'a [Op], path: &[Seg]) -> Option<&'a Op> {
        ops.iter().find(|o| o.path == path)
    }

    #[test]
    fn no_change_no_ops_and_clock_not_ticked() {
        let mut e = Env::new();
        let doc = json!({"a": 1, "l": [{"id": "x", "n": 1}]});
        let before = e.clock.last().clone();
        assert!(e.diff(&doc, &doc).is_empty());
        assert_eq!(e.clock.last(), &before);
    }

    #[test]
    fn add_change_and_remove_fields() {
        let mut e = Env::new();
        let base = json!({"a": 1, "b": {"c": 2, "d": 3}, "gone": "x"});
        let cur = json!({"a": 1, "b": {"c": 20, "n": 4}, "new": true});
        let ops = e.diff(&base, &cur);
        assert!(
            matches!(&find(&ops, &[k("b"), k("c")]).unwrap().kind, OpKind::Set(v) if v == &json!(20))
        );
        assert!(
            matches!(&find(&ops, &[k("b"), k("n")]).unwrap().kind, OpKind::Set(v) if v == &json!(4))
        );
        assert!(
            matches!(&find(&ops, &[k("new")]).unwrap().kind, OpKind::Set(v) if v == &json!(true))
        );
        assert!(matches!(
            find(&ops, &[k("b"), k("d")]).unwrap().kind,
            OpKind::Remove
        ));
        assert!(matches!(
            find(&ops, &[k("gone")]).unwrap().kind,
            OpKind::Remove
        ));
        assert!(find(&ops, &[k("a")]).is_none(), "没变的不报");
        assert_eq!(ops.len(), 5);
    }

    #[test]
    fn all_ops_of_one_batch_share_one_timestamp_and_source() {
        let mut e = Env::new();
        let ops = e.diff(&json!({"a": 1, "b": 2}), &json!({"a": 2, "c": 3}));
        assert_eq!(ops.len(), 3);
        assert!(ops.iter().all(|o| o.hlc == ops[0].hlc));
        assert!(ops.iter().all(|o| o.source == "src"));
        assert_eq!(ops[0].hlc.device, "A");
        // 下一批的时间戳严格更大
        let ops2 = e.diff(&json!({"a": 2}), &json!({"a": 3}));
        assert!(ops2[0].hlc > ops[0].hlc);
    }

    #[test]
    fn deleting_an_element_emits_a_single_tombstone() {
        let mut e = Env::new();
        let base = json!({"conns": [
            {"id": "a", "host": "h1", "port": 22, "tags": [{"id": "t1", "label": "x"}]},
            {"id": "b", "host": "h2"},
        ]});
        let cur = json!({"conns": [{"id": "b", "host": "h2"}]});
        let ops = e.diff(&base, &cur);
        // a 整个消失：一个墓碑 + 顺序叶子的更新；不逐个发 Remove
        let tombs: Vec<_> = ops.iter().filter(|o| o.kind == OpKind::Tombstone).collect();
        assert_eq!(tombs.len(), 1);
        assert_eq!(tombs[0].path, vec![k("conns"), id("a")]);
        assert!(!ops.iter().any(|o| o.kind == OpKind::Remove), "{ops:#?}");
        let order = find(&ops, &[k("conns"), Seg::ORDER]).unwrap();
        assert!(matches!(&order.kind, OpKind::Set(v) if v == &json!(["b"])));
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn nested_element_deletion_uses_the_outermost_vanished_element() {
        let mut e = Env::new();
        let base = json!({"conns": [
            {"id": "a", "tags": [{"id": "t1", "label": "x"}, {"id": "t2", "label": "y"}]},
        ]});
        // 只删内层 t1
        let cur = json!({"conns": [{"id": "a", "tags": [{"id": "t2", "label": "y"}]}]});
        let ops = e.diff(&base, &cur);
        let tombs: Vec<_> = ops.iter().filter(|o| o.kind == OpKind::Tombstone).collect();
        assert_eq!(tombs.len(), 1);
        assert_eq!(
            tombs[0].path,
            vec![k("conns"), id("a"), k("tags"), id("t1")]
        );
        // 外层、内层一起删：只发外层
        let cur2 = json!({"conns": []});
        let ops2 = e.diff(&base, &cur2);
        let tombs2: Vec<_> = ops2
            .iter()
            .filter(|o| o.kind == OpKind::Tombstone)
            .collect();
        assert_eq!(tombs2.len(), 1);
        assert_eq!(tombs2[0].path, vec![k("conns"), id("a")]);
        // 列表清空后顺序叶子消失 → 一个 Remove(Order)；元素里的叶子不单独 Remove
        let removes: Vec<_> = ops2.iter().filter(|o| o.kind == OpKind::Remove).collect();
        assert_eq!(removes.len(), 1);
        assert_eq!(removes[0].path, vec![k("conns"), Seg::ORDER]);
    }

    #[test]
    fn removing_a_field_of_a_surviving_element_is_a_plain_remove() {
        let mut e = Env::new();
        let base = json!({"conns": [{"id": "a", "host": "h", "note": "n"}]});
        let cur = json!({"conns": [{"id": "a", "host": "h"}]});
        let ops = e.diff(&base, &cur);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].path, vec![k("conns"), id("a"), k("note")]);
        assert_eq!(ops[0].kind, OpKind::Remove);
    }

    #[test]
    fn adding_an_element_sets_every_leaf_and_order() {
        let mut e = Env::new();
        let base = json!({"conns": [{"id": "a", "host": "h"}]});
        let cur =
            json!({"conns": [{"id": "a", "host": "h"}, {"id": "b", "host": "h2", "port": 1}]});
        let ops = e.diff(&base, &cur);
        assert!(find(&ops, &[k("conns"), id("b"), k("id")]).is_some());
        assert!(find(&ops, &[k("conns"), id("b"), k("host")]).is_some());
        assert!(find(&ops, &[k("conns"), id("b"), k("port")]).is_some());
        assert!(
            matches!(&find(&ops, &[k("conns"), Seg::ORDER]).unwrap().kind,
            OpKind::Set(v) if v == &json!(["a", "b"]))
        );
        assert_eq!(ops.len(), 4);
    }

    #[test]
    fn reordering_only_emits_the_order_leaf() {
        let mut e = Env::new();
        let base = json!({"l": [{"id": "a"}, {"id": "b"}, {"id": "c"}]});
        let cur = json!({"l": [{"id": "c"}, {"id": "a"}, {"id": "b"}]});
        let ops = e.diff(&base, &cur);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].path, vec![k("l"), Seg::ORDER]);
    }

    #[test]
    fn local_scope_leaves_are_never_reported() {
        let mut e = Env::new();
        let base =
            json!({"window": {"w": 1}, "conns": [{"id": "a", "host": "h", "keyPath": "/k1"}]});
        let cur = json!({"window": {"w": 999, "h": 5}, "conns": [{"id": "a", "host": "h", "keyPath": "/other"}]});
        assert!(e.diff(&base, &cur).is_empty());
        // 删掉 Local 字段同样不报
        let cur2 = json!({"conns": [{"id": "a", "host": "h"}]});
        assert!(e.diff(&base, &cur2).is_empty());
    }

    #[test]
    fn platform_scope_leaves_get_the_prefix() {
        let mut e = Env::new();
        let ops = e.diff(
            &json!({}),
            &json!({"notify": {"sound": true}, "theme": "x"}),
        );
        assert!(find(&ops, &[k("@desktop"), k("notify"), k("sound")]).is_some());
        assert!(find(&ops, &[k("theme")]).is_some());
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn item_filter_limits_reported_fields_of_builtin_elements() {
        let mut e = Env::new();
        let base = json!({"skills": [{"id": "builtin-a", "name": "A", "enabled": true}]});
        // 内置 skill 的内容变了（升级带来的）不该上报；enabled 变了才报
        let cur = json!({"skills": [{"id": "builtin-a", "name": "A v2", "enabled": false}]});
        let ops = e.diff(&base, &cur);
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0].path,
            vec![k("skills"), id("builtin-a"), k("enabled")]
        );
    }

    #[test]
    fn suppress_removes_drops_removes_but_keeps_tombstones() {
        let mut e = Env::new();
        let base = json!({
            "a": 1, "old": "x",
            "conns": [{"id": "a", "host": "h", "legacy": 1}, {"id": "b", "host": "h2"}],
        });
        let cur = json!({"a": 2, "conns": [{"id": "a", "host": "h"}]});
        let ops = e.diff_opts(&base, &cur, true);
        assert!(!ops.iter().any(|o| o.kind == OpKind::Remove), "{ops:#?}");
        // 墓碑照发
        assert!(ops
            .iter()
            .any(|o| o.kind == OpKind::Tombstone && o.path == vec![k("conns"), id("b")]));
        // Set 照发
        assert!(find(&ops, &[k("a")]).is_some());
        // 对比：不抑制时有 Remove
        let mut e2 = Env::new();
        let ops2 = e2.diff_opts(&base, &cur, false);
        assert!(ops2.iter().any(|o| o.kind == OpKind::Remove));
    }

    #[test]
    fn fields_unknown_to_this_version_are_never_reported_as_removed() {
        // 版本兼容的根基：基线与当前都没有旧版本不认识的字段 → 无论镜像里有什么，都不会发 Remove
        let mut e = Env::new();
        let base = json!({"known": 1});
        let cur = json!({"known": 2});
        let ops = e.diff(&base, &cur);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0].kind, OpKind::Set(_)));
    }

    #[test]
    fn export_all_is_every_leaf_as_set() {
        let mut e = Env::new();
        let doc = json!({"a": 1, "l": [{"id": "x", "n": 1}], "window": {"w": 1}});
        let mut ctx = DiffCtx {
            rules: &RULES,
            platform: Platform::Desktop,
            idmaps: &mut e.idmaps,
            clock: &mut e.clock,
            wall_ms: 5000,
            suppress_removes: false,
        };
        let ops = export_all("src", &doc, &mut ctx);
        assert!(ops.iter().all(|o| matches!(o.kind, OpKind::Set(_))));
        assert_eq!(ops.len(), 4); // a, l/x/id, l/x/n, l/Order；window 是 Local
    }

    #[test]
    fn export_all_at_uses_given_timestamp_without_ticking_the_clock() {
        let mut reg = IdMapRegistry::new();
        let zero = Hlc::zero_for("ME");
        let ops = export_all_at(
            "src",
            &json!({"a": 1, "l": [{"id": "x"}]}),
            &RULES,
            Platform::Desktop,
            &mut reg,
            &zero,
        );
        assert_eq!(ops.len(), 3);
        assert!(ops.iter().all(|o| o.hlc == zero));
        assert!(reg.contains_array(&[k("l")]));
    }

    #[test]
    fn array_becoming_id_mapped_removes_the_stale_atomic_leaf() {
        // `tags: []` 当时是原子叶子；后来有了 id 元素 → 基线里的旧叶子要被 Remove，免得成幽灵
        let mut e = Env::new();
        let base = json!({"item": {"tags": []}});
        let cur = json!({"item": {"tags": [{"id": "t1"}]}});
        let ops = e.diff(&base, &cur);
        let stale = find(&ops, &[k("item"), k("tags")]).expect("应有对旧原子叶子的 Remove");
        assert_eq!(stale.kind, OpKind::Remove);
        assert!(find(&ops, &[k("item"), k("tags"), id("t1"), k("id")]).is_some());
        // 之后同样的位置再变化：登记表已含它，不会再重复这件事
        let ops2 = e.diff(&cur, &json!({"item": {"tags": []}}));
        assert!(
            ops2.iter().all(|o| o.path != vec![k("item"), k("tags")]),
            "{ops2:#?}"
        );
    }

    #[test]
    fn diff_updates_the_registry_it_was_given() {
        let mut e = Env::new();
        e.diff(&json!({}), &json!({"l": [{"id": "x"}]}));
        assert!(e.idmaps.contains_array(&[k("l")]));
    }
}
