//! JSON 文档 → 扁平叶子（`BTreeMap<Path, Value>`）。
//!
//! 规则（别改，镜像与落地都建立在它上面）：
//! - 对象：逐 key 递归；**空对象不产生叶子**（缺失 = 不存在，反序列化时走 serde 默认）。
//! - 数组：登记过的 id 映射位置，或「非空且每个元素都是带字符串 `id` 的对象」→ 按 id 展开，
//!   元素自身的 `id` 字段也作为普通叶子保留，另产生一个顺序叶子 `[.., Order] = ["id1","id2",..]`；
//!   这类数组**没有任何元素可同步时（空数组）不产生叶子**（与「空对象不产生叶子」同理，
//!   也让「清空了列表」与「字段干脆没写出来」在两台设备上看起来一样）。
//!   其余数组整体是一个原子叶子（包括空数组：未登记前 `[]` 就是个值）。
//! - 其它（字符串 / 数字 / 布尔 / null）是叶子。根文档为 `null` 视为空文档。
//!
//! id 映射数组里，没有字符串 `id` 的元素、重复 id 的后来者**不参与同步**（落地时保持本机原样）。

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::path::{escape_key, IdMapRegistry, Path, Platform, Seg};
use super::scope::{ItemKeep, Scope, ScopeRules};

/// 元素的 id：必须是带字符串 `id` 字段的对象。
pub(super) fn element_id(v: &Value) -> Option<&str> {
    v.as_object()?.get("id")?.as_str()
}

/// 「非空且每个元素都是带字符串 id 的对象」。
fn qualifies_as_id_map(arr: &[Value]) -> bool {
    !arr.is_empty() && arr.iter().all(|e| element_id(e).is_some())
}

/// 学习文档里所有该按 id 映射的数组位置，登记到 `idmaps`。
///
/// 登记表是「只增不减」的，且一个位置一旦登记，该位置的**所有**数组（哪怕是空的、
/// 元素缺 id 的）都按 id 映射。因此学习要做到不动点：先登记的位置可能让原本不被递归的
/// 子树变得可递归，里面又冒出新的位置。扫到登记表不再增长为止（至多层数那么多遍）。
pub fn learn_id_maps(doc: &Value, idmaps: &mut IdMapRegistry) {
    loop {
        let before = idmaps.len();
        learn_pass(doc, &mut Vec::new(), idmaps);
        if idmaps.len() == before {
            return;
        }
    }
}

fn learn_pass(v: &Value, path: &mut Path, idmaps: &mut IdMapRegistry) {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                path.push(Seg::Key(escape_key(k)));
                learn_pass(c, path, idmaps);
                path.pop();
            }
        }
        Value::Array(a) if idmaps.contains_array(path) || qualifies_as_id_map(a) => {
            idmaps.register_array(path);
            for el in a {
                if let Some(id) = element_id(el) {
                    path.push(Seg::id(id));
                    learn_pass(el, path, idmaps);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

/// 不带作用域规则的扁平化（规格里的 `flatten`）：先学习 id 映射位置，再展开。
pub fn flatten(doc: &Value, idmaps: &mut IdMapRegistry) -> BTreeMap<Path, Value> {
    learn_id_maps(doc, idmaps);
    flatten_with(doc, idmaps, None, Platform::Desktop)
}

/// 带作用域规则的扁平化：Local 叶子被过滤，Platform 叶子在路径最前面加 `platform` 的前缀，
/// 元素级过滤（内置 skill 只留 `enabled` 等）生效。上报（`diff`）用它。
pub fn flatten_scoped(
    doc: &Value,
    rules: &ScopeRules,
    platform: Platform,
    idmaps: &mut IdMapRegistry,
) -> BTreeMap<Path, Value> {
    learn_id_maps(doc, idmaps);
    flatten_with(doc, idmaps, Some(rules), platform)
}

/// 用**给定登记表**（不再学习）展开。`diff` 要用「基线当时的登记表」展开基线，所以单独暴露。
///
/// `rules = None` = 不过滤任何作用域：新字段守护快照（`sync::scope_guard`）靠它把 Local 叶子也
/// 展开出来，否则快照里只剩 Shared，作用域改错了也看不出来。
pub(crate) fn flatten_with(
    doc: &Value,
    idmaps: &IdMapRegistry,
    rules: Option<&ScopeRules>,
    platform: Platform,
) -> BTreeMap<Path, Value> {
    let mut w = Walker {
        idmaps,
        rules,
        platform,
        out: BTreeMap::new(),
        emitted: 0,
    };
    if !doc.is_null() {
        w.walk(doc, &mut Vec::new());
    }
    w.out
}

struct Walker<'a> {
    idmaps: &'a IdMapRegistry,
    rules: Option<&'a ScopeRules>,
    platform: Platform,
    out: BTreeMap<Path, Value>,
    /// 实际写入 `out` 的叶子数。判断「某个元素有没有任何叶子被同步」用，
    /// 因为被作用域整条过滤掉的元素不应出现在顺序叶子里。
    emitted: usize,
}

impl Walker<'_> {
    fn walk(&mut self, v: &Value, path: &mut Path) {
        match v {
            Value::Object(m) => {
                for (k, c) in m {
                    path.push(Seg::Key(escape_key(k)));
                    self.walk(c, path);
                    path.pop();
                }
            }
            Value::Array(a) if self.idmaps.contains_array(path) => self.walk_id_map(a, path),
            _ => self.emit(path, v.clone()),
        }
    }

    fn walk_id_map(&mut self, arr: &[Value], path: &mut Path) {
        let mut ids: Vec<Value> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for el in arr {
            let Some(id) = element_id(el) else { continue };
            if !seen.insert(id) {
                continue; // 重复 id：先到先得
            }
            let keep = match self.rules {
                Some(r) => r.item_keep(path, el),
                None => ItemKeep::All,
            };
            if matches!(keep, ItemKeep::Only(f) if f.is_empty()) {
                continue; // 整个元素是本机的
            }
            let before = self.emitted;
            path.push(Seg::id(id));
            match keep {
                ItemKeep::All => self.walk(el, path),
                ItemKeep::Only(fields) => {
                    if let Value::Object(m) = el {
                        for f in fields {
                            if let Some(c) = m.get(*f) {
                                path.push(Seg::Key(escape_key(f)));
                                self.walk(c, path);
                                path.pop();
                            }
                        }
                    }
                }
            }
            path.pop();
            if self.emitted > before {
                ids.push(Value::String(id.to_string()));
            }
        }
        if !ids.is_empty() {
            path.push(Seg::ORDER);
            self.emit(path, Value::Array(ids));
            path.pop();
        }
    }

    fn emit(&mut self, path: &Path, v: Value) {
        let key = match self.rules.map(|r| r.scope_of(path)) {
            None | Some(Scope::Shared) => path.clone(),
            Some(Scope::Local) => return,
            Some(Scope::Platform) => {
                let mut p = Vec::with_capacity(path.len() + 1);
                p.push(self.platform.prefix_seg());
                p.extend(path.iter().cloned());
                p
            }
        };
        self.out.insert(key, v);
        self.emitted += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::core::scope::ItemFilter;
    use serde_json::json;

    fn k(s: &str) -> Seg {
        Seg::key(s)
    }
    fn id(s: &str) -> Seg {
        Seg::id(s)
    }
    fn flat(doc: &Value) -> BTreeMap<Path, Value> {
        flatten(doc, &mut IdMapRegistry::new())
    }

    #[test]
    fn nested_objects_become_leaves() {
        let f = flat(&json!({"a": 1, "b": {"c": "x", "d": {"e": true, "f": null}}}));
        assert_eq!(f.len(), 4);
        assert_eq!(f[&vec![k("a")]], json!(1));
        assert_eq!(f[&vec![k("b"), k("c")]], json!("x"));
        assert_eq!(f[&vec![k("b"), k("d"), k("e")]], json!(true));
        assert_eq!(f[&vec![k("b"), k("d"), k("f")]], json!(null));
    }

    #[test]
    fn empty_object_produces_no_leaf() {
        let f = flat(&json!({"a": {}, "b": {"c": {}}, "d": 1}));
        assert_eq!(f.len(), 1);
        assert!(f.contains_key(&vec![k("d")]));
    }

    #[test]
    fn root_null_is_empty_document() {
        assert!(flat(&Value::Null).is_empty());
        assert!(flat(&json!({})).is_empty());
    }

    #[test]
    fn id_array_is_expanded_with_order_leaf() {
        let doc = json!({"conns": [
            {"id": "a", "host": "h1", "port": 22},
            {"id": "b", "host": "h2", "tags": ["x", "y"]},
        ]});
        let mut reg = IdMapRegistry::new();
        let f = flatten(&doc, &mut reg);
        assert!(reg.contains_array(&[k("conns")]));
        assert_eq!(f[&vec![k("conns"), id("a"), k("id")]], json!("a"));
        assert_eq!(f[&vec![k("conns"), id("a"), k("host")]], json!("h1"));
        assert_eq!(f[&vec![k("conns"), id("a"), k("port")]], json!(22));
        // 元素里的标量数组是原子叶子
        assert_eq!(f[&vec![k("conns"), id("b"), k("tags")]], json!(["x", "y"]));
        assert_eq!(f[&vec![k("conns"), Seg::ORDER]], json!(["a", "b"]));
        // a: id/host/port；b: id/host/tags；再加一个顺序叶子
        assert_eq!(f.len(), 7, "{f:#?}");
    }

    #[test]
    fn array_of_objects_without_ids_is_atomic() {
        let doc = json!({"rules": [{"name": "a"}, {"name": "b"}]});
        let mut reg = IdMapRegistry::new();
        let f = flatten(&doc, &mut reg);
        assert!(reg.is_empty());
        assert_eq!(f.len(), 1);
        assert_eq!(f[&vec![k("rules")]], json!([{"name": "a"}, {"name": "b"}]));
    }

    #[test]
    fn mixed_array_where_one_element_lacks_id_is_atomic() {
        let doc = json!({"xs": [{"id": "a"}, {"name": "no-id"}]});
        let mut reg = IdMapRegistry::new();
        let f = flatten(&doc, &mut reg);
        assert!(reg.is_empty());
        assert_eq!(f.len(), 1);
        // 非字符串 id 同样不算
        let f2 = flat(&json!({"xs": [{"id": 1}, {"id": 2}]}));
        assert_eq!(f2.len(), 1);
    }

    #[test]
    fn scalar_arrays_are_atomic_including_empty() {
        let f = flat(&json!({"a": [1, 2, 3], "b": [], "c": ["x"]}));
        assert_eq!(f[&vec![k("a")]], json!([1, 2, 3]));
        assert_eq!(f[&vec![k("b")]], json!([]));
        assert_eq!(f[&vec![k("c")]], json!(["x"]));
    }

    #[test]
    fn empty_array_before_and_after_registration() {
        let mut reg = IdMapRegistry::new();
        // 登记前：[] 是个原子值
        let before = flatten(&json!({"xs": []}), &mut reg);
        assert_eq!(before[&vec![k("xs")]], json!([]));
        // 同一位置出现过 id 元素 → 登记
        flatten(&json!({"xs": [{"id": "a"}]}), &mut reg);
        assert!(reg.contains_array(&[k("xs")]));
        // 登记后：同一位置的空数组不再产生任何叶子
        let after = flatten(&json!({"xs": []}), &mut reg);
        assert!(after.is_empty(), "{after:?}");
        // 登记后元素缺 id 的数组也按 id 映射：缺 id 的元素不参与同步
        let after2 = flatten(&json!({"xs": [{"id": "a"}, {"name": "n"}]}), &mut reg);
        assert_eq!(after2.len(), 2); // xs/a/id + Order
        assert_eq!(after2[&vec![k("xs"), Seg::ORDER]], json!(["a"]));
    }

    #[test]
    fn registration_is_sticky_across_documents() {
        let mut reg = IdMapRegistry::new();
        flatten(&json!({"xs": [{"id": "a"}]}), &mut reg);
        // 现在一个只有原子元素的数组出现在同一位置：仍按 id 映射（元素缺 id → 被忽略）
        let f = flatten(&json!({"xs": [1, 2]}), &mut reg);
        assert!(f.is_empty());
    }

    #[test]
    fn nested_id_maps_register_each_level() {
        let doc = json!({"conns": [
            {"id": "a", "tags": [{"id": "t1", "label": "L"}, {"id": "t2"}]},
            {"id": "b", "tags": []},
        ]});
        let mut reg = IdMapRegistry::new();
        let f = flatten(&doc, &mut reg);
        assert!(reg.contains_array(&[k("conns")]));
        assert!(reg.contains_array(&[k("conns"), id("zz"), k("tags")]));
        assert_eq!(
            f[&vec![k("conns"), id("a"), k("tags"), id("t1"), k("label")]],
            json!("L")
        );
        assert_eq!(
            f[&vec![k("conns"), id("a"), k("tags"), Seg::ORDER]],
            json!(["t1", "t2"])
        );
        // b 的 tags 是空数组：同一位置已登记 → 没有叶子（而不是原子 []）
        assert!(!f.contains_key(&vec![k("conns"), id("b"), k("tags")]));
        assert!(!f.contains_key(&vec![k("conns"), id("b"), k("tags"), Seg::ORDER]));
    }

    #[test]
    fn learning_reaches_fixpoint_regardless_of_visit_order() {
        // b（先被访问）的 tags 元素缺 id，a（后访问）的 tags 才满足条件；
        // 登记后 b 的元素也按 id 映射，其内部更深的数组必须也被学到。
        let doc = json!({"conns": [
            {"id": "a", "tags": [{"id": "t1"}]},
            {"id": "b", "tags": [{"id": "t2", "deep": [{"id": "d1"}]}, {"nope": 1}]},
        ]});
        let mut reg = IdMapRegistry::new();
        flatten(&doc, &mut reg);
        assert!(reg.contains_array(&[k("conns"), id("x"), k("tags"), id("y"), k("deep")]));
        // 第二次展开结果与第一次完全相同（稳定）
        let a = flatten(&doc, &mut reg);
        let b = flatten(&doc, &mut IdMapRegistry::new());
        assert_eq!(a, b);
    }

    #[test]
    fn duplicate_ids_first_wins_and_non_id_elements_skipped() {
        let mut reg = IdMapRegistry::new();
        flatten(&json!({"xs": [{"id": "a"}]}), &mut reg);
        let f = flatten(
            &json!({"xs": [{"id": "a", "v": 1}, {"id": "a", "v": 2}, "str", {"v": 3}]}),
            &mut reg,
        );
        assert_eq!(f[&vec![k("xs"), id("a"), k("v")]], json!(1));
        assert_eq!(f[&vec![k("xs"), Seg::ORDER]], json!(["a"]));
    }

    #[test]
    fn root_level_array_is_supported() {
        let doc = json!([{"id": "a", "n": 1}, {"id": "b", "n": 2}]);
        let mut reg = IdMapRegistry::new();
        let f = flatten(&doc, &mut reg);
        assert!(reg.contains_array(&[]));
        assert_eq!(f[&vec![id("a"), k("n")]], json!(1));
        assert_eq!(f[&vec![Seg::ORDER]], json!(["a", "b"]));
    }

    #[test]
    fn root_scalar_is_a_root_leaf() {
        let f = flat(&json!(5));
        assert_eq!(f[&Vec::<Seg>::new()], json!(5));
    }

    #[test]
    fn at_prefixed_keys_are_escaped_not_mistaken_for_platform_tags() {
        let f = flat(&json!({"@desktop": 1, "@@x": 2}));
        assert!(f.contains_key(&vec![k("@@desktop")]));
        assert!(f.contains_key(&vec![k("@@@x")]));
    }

    #[test]
    fn flatten_is_deterministic_regardless_of_key_order() {
        let a = flat(&json!({"x": 1, "y": {"b": 2, "a": 3}}));
        let b = flat(&json!({"y": {"a": 3, "b": 2}, "x": 1}));
        assert_eq!(a, b);
    }

    // ---------------- 作用域 ----------------

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
                Some(i) if i.starts_with("tmp-") => Some(&[]),
                _ => None,
            },
        }],
    };

    fn flat_scoped(doc: &Value, p: Platform) -> BTreeMap<Path, Value> {
        flatten_scoped(doc, &RULES, p, &mut IdMapRegistry::new())
    }

    #[test]
    fn local_leaves_are_filtered() {
        let f = flat_scoped(
            &json!({"theme": "dark", "window": {"w": 1, "h": 2}}),
            Platform::Desktop,
        );
        assert_eq!(f.len(), 1);
        assert!(f.contains_key(&vec![k("theme")]));
    }

    #[test]
    fn local_field_inside_id_element_is_filtered() {
        let f = flat_scoped(
            &json!({"conns": [{"id": "a", "host": "h", "keyPath": "/home/x/id"}]}),
            Platform::Desktop,
        );
        assert!(f.contains_key(&vec![k("conns"), id("a"), k("host")]));
        assert!(!f.contains_key(&vec![k("conns"), id("a"), k("keyPath")]));
    }

    #[test]
    fn platform_leaves_get_prefix_of_current_platform() {
        let doc = json!({"notify": {"sound": true}, "theme": "x"});
        let d = flat_scoped(&doc, Platform::Desktop);
        assert_eq!(
            d[&vec![k("@desktop"), k("notify"), k("sound")]],
            json!(true)
        );
        assert!(d.contains_key(&vec![k("theme")]));
        let a = flat_scoped(&doc, Platform::Android);
        assert_eq!(
            a[&vec![k("@android"), k("notify"), k("sound")]],
            json!(true)
        );
    }

    #[test]
    fn item_filter_keeps_only_listed_fields_and_hides_fully_local_elements() {
        let doc = json!({"skills": [
            {"id": "builtin-a", "name": "A", "content": "long", "enabled": true},
            {"id": "tmp-1", "name": "temp"},
            {"id": "user-1", "name": "U", "enabled": false},
        ]});
        let f = flat_scoped(&doc, Platform::Desktop);
        // 内置：只有 enabled
        assert_eq!(
            f[&vec![k("skills"), id("builtin-a"), k("enabled")]],
            json!(true)
        );
        assert!(!f.contains_key(&vec![k("skills"), id("builtin-a"), k("name")]));
        assert!(!f.contains_key(&vec![k("skills"), id("builtin-a"), k("id")]));
        // 整条本机：不出现，也不在顺序里
        assert!(!f.keys().any(|p| p.contains(&id("tmp-1"))));
        assert_eq!(
            f[&vec![k("skills"), Seg::ORDER]],
            json!(["builtin-a", "user-1"])
        );
        // 用户 skill 全同步
        assert_eq!(f[&vec![k("skills"), id("user-1"), k("name")]], json!("U"));
    }

    #[test]
    fn element_whose_every_leaf_is_local_is_not_in_order() {
        static R: ScopeRules = ScopeRules {
            patterns: &[("xs/*", Scope::Local)],
            item_filters: &[],
        };
        let f = flatten_scoped(
            &json!({"xs": [{"id": "a"}, {"id": "b"}]}),
            &R,
            Platform::Desktop,
            &mut IdMapRegistry::new(),
        );
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn flatten_with_uses_given_registry_without_learning() {
        // 空登记表：id 数组被当原子叶子
        let doc = json!({"xs": [{"id": "a"}]});
        let f = flatten_with(&doc, &IdMapRegistry::new(), None, Platform::Desktop);
        assert_eq!(f.len(), 1);
        assert!(f.contains_key(&vec![k("xs")]));
    }
}
