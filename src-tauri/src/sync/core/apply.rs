//! 远端改动落地：把镜像里「变了的那些叶子」写回本机 JSON 文档。
//!
//! 铁律：**只动 `affected` 涉及的路径**，其它一律保持本机原样——本机专属字段、本机不认识的结构
//! （更新版本写进文档、本版本反序列化时会丢掉的字段）、本机独有的元素，都不碰。
//!
//! 调用方的标准流程（一轮同步里对单个源）：在写者锁内
//! 先对本机做一次捕获（`diff`，本机 op 并入镜像；防抖窗口里的本机改动不能被吞）→
//! `merge_remote_ops` 并入远端 op / 快照 → [`affected_vs_doc`]（**本机文档现状 vs 镜像可见集**）→
//! `apply_to_doc` → 反序列化 / 校验 → 提交 → 新基线 = 提交后的序列化。
//!
//! 为什么受影响路径要拿"文档现状"而不是"合并前后的可见集"来比：本机的 op 并不总能赢（对方时钟更快、
//! 对同一元素的并发编辑 / 删除），也可能改变别的叶子的可见性（用同一个 id 重建元素时旧字段随之回来）；
//! 只比合并前后，这些"本机文档已经偏离镜像"的地方就永远没有人去对齐，两台设备就此分叉。
//! 拿文档现状比对则是幂等的"把文档对齐到镜像"，不依赖任何中间状态。代价是它**要求落地前已经捕获过**：
//! 没捕获的本机改动会被当成"文档偏离镜像"而被镜像的旧值盖回去。
//!
//! 落地时同样遵守作用域：
//! - 平台前缀：只认本平台的（剥掉前缀写入），别的平台的跳过；
//! - 本机认为是 `Local` 的路径一律不写（即便对方的版本认为它是共享的）；
//! - 元素级过滤（内置 skill 只同步 `enabled`）：不在名单里的字段不写；本机没有的这类元素不凭空创建，
//!   也不因墓碑被删（内置的东西本机总会自己补回来）。
//!
//! 取舍：删除叶子后留下的空对象不清理——`{}` 与缺失在扁平化里等价，是否等同于「不存在」由各源的
//! `from_sync_json` 按 serde 默认处理；贸然清理会让「必填结构体字段缺失」的源反序列化失败。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;

use serde_json::{json, Map, Value};

use super::flatten::{element_id, flatten_scoped};
use super::mirror::{affected, Mirror};
use super::path::{
    split_platform_prefix, unescape_key, IdMapRegistry, Path, Platform, PrefixKind, Seg,
};
use super::scope::{ItemKeep, Scope, ScopeRules};

/// 把 `affected` 里的叶子按镜像当前状态写进 `local`，返回新文档（`local` 不变）。
pub fn apply_to_doc(
    local: &Value,
    mirror: &Mirror,
    affected: &[Path],
    rules: &ScopeRules,
    platform: Platform,
) -> Value {
    let visible = mirror.visible();
    let mut doc = local.clone();
    let mut aff: Vec<&Path> = affected.iter().collect();
    aff.sort();
    aff.dedup();

    // 需要按顺序叶子重排的数组（镜像空间的列表路径）
    let mut touched: BTreeSet<Path> = BTreeSet::new();

    // 第一阶段：删。先删后写，这样「叶子 → 子树」这类换型的字段先被清掉再被重建。
    for p in &aff {
        if visible.contains_key(p.as_slice()) {
            continue; // 可见的留给写阶段
        }
        let Some(off) = resolve(p, rules, platform) else {
            continue;
        };
        if p.last().is_some_and(Seg::is_order) {
            // 顺序叶子消失（被删，或输给了更晚的删除）只是「没有顺序信息了」，不是删数据；
            // 但本机数组可能还留着旧顺序，要退回到"按最早叶子 hlc 排"的统一兜底顺序，
            // 否则各设备会各自停在不同的顺序上。
            touched.insert(p[..p.len() - 1].to_vec());
            continue;
        }
        let dead = p
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_id())
            .map(|(i, _)| i)
            .find(|&i| !has_visible_under(&visible, &p[..=i]));
        match dead {
            Some(i) => {
                if let Seg::Id { id } = &p[i] {
                    remove_element(&mut doc, &p[off..i], id, rules);
                }
            }
            None => {
                let d = &p[off..];
                if write_allowed(&doc, d, rules) {
                    remove_leaf(&mut doc, d);
                }
            }
        }
    }

    // 第二阶段：写（路径已排序，浅的在前）
    for p in &aff {
        let Some(value) = visible.get(p.as_slice()) else {
            continue;
        };
        let Some(off) = resolve(p, rules, platform) else {
            continue;
        };
        if p.last().is_some_and(Seg::is_order) {
            touched.insert(p[..p.len() - 1].to_vec());
            continue;
        }
        let d = &p[off..];
        if !write_allowed(&doc, d, rules) {
            continue;
        }
        for i in write_leaf(&mut doc, d, value.clone()) {
            touched.insert(p[..off + i].to_vec());
        }
    }

    // 第三阶段：重排
    for m in &touched {
        let (kind, d) = split_platform_prefix(m, platform);
        if kind == PrefixKind::Other {
            continue;
        }
        reorder(&mut doc, d, m, &visible, mirror);
    }
    doc
}

/// 受影响路径：把「本机文档现在的样子」与「镜像可见集」逐叶子对比，只保留对本机有意义的路径
/// （本平台 / 共享、且本机规则不认为它是 Local 的；别的平台的前缀路径被丢掉）。
///
/// 这是落地的**标准入口**（也用于加入同步、读到本机从未落地过的源、配置损坏后从镜像恢复）。
/// 比对结果里：值不同的 / 文档里没有的 → 写入；文档里有、镜像里已死或已删的（云端删了的元素、
/// 被 Remove 的字段）→ 删除。已经对齐的文档返回空，`apply_to_doc` 也就什么都不改。
///
/// 前提：**本机改动已捕获进镜像**。否则文档里"镜像还不知道"的叶子会被当成多余的而删掉 / 被旧值盖回。
/// 加入同步时先把本机数据以 `export_all_at(.., Hlc::zero_for(本机))` 并入镜像，本机独有的东西
/// 在镜像里是活的，就不会被删。
///
/// 副作用说明：顺序叶子在文档与镜像之间允许有出入（镜像里没提到的活元素会被追加在末尾），
/// 所以有这种出入的源每轮都会得到一个 `Order` 路径、触发一次幂等的重排；调用方比较
/// `apply_to_doc` 的结果与原文档相等就可以跳过后面的提交。
pub fn affected_vs_doc(
    doc: &Value,
    mirror: &Mirror,
    rules: &ScopeRules,
    platform: Platform,
    idmaps: &mut IdMapRegistry,
) -> Vec<Path> {
    let before = flatten_scoped(doc, rules, platform, idmaps);
    affected(&before, &mirror.visible())
        .into_iter()
        .filter(|p| resolve(p, rules, platform).is_some())
        .collect()
}

/// 镜像路径对本机是否适用；适用时返回文档内路径的起点（0 = 无前缀，1 = 剥掉平台前缀）。
fn resolve(path: &[Seg], rules: &ScopeRules, platform: Platform) -> Option<usize> {
    let (kind, rest) = split_platform_prefix(path, platform);
    match (kind, rules.scope_of(rest)) {
        (PrefixKind::None, Scope::Shared) => Some(0),
        (PrefixKind::Own, Scope::Platform) => Some(1),
        _ => None,
    }
}

/// `prefix` 下面是否有可见叶子（= 以它为路径的元素活着）。
fn has_visible_under(visible: &BTreeMap<Path, Value>, prefix: &[Seg]) -> bool {
    visible
        .range::<[Seg], _>((Bound::Included(prefix), Bound::Unbounded))
        .next()
        .is_some_and(|(k, _)| k.starts_with(prefix))
}

/// 沿路径向下找（不创建）。
fn get_mut<'a>(mut cur: &'a mut Value, path: &[Seg]) -> Option<&'a mut Value> {
    for seg in path {
        cur = match seg {
            Seg::Key(k) => cur.as_object_mut()?.get_mut(unescape_key(k))?,
            Seg::Id { id } => cur
                .as_array_mut()?
                .iter_mut()
                .find(|e| element_id(e) == Some(id.as_str()))?,
            Seg::Order { .. } => return None,
        };
    }
    Some(cur)
}

/// 元素级过滤的预检：这个写 / 删是否被允许。只读，不会创建任何东西。
fn write_allowed(root: &Value, path: &[Seg], rules: &ScopeRules) -> bool {
    let mut cur: Option<&Value> = Some(root);
    for (i, seg) in path.iter().enumerate() {
        match seg {
            Seg::Key(k) => {
                cur = cur
                    .and_then(Value::as_object)
                    .and_then(|o| o.get(unescape_key(k)));
            }
            Seg::Id { id } => {
                let existing = cur
                    .and_then(Value::as_array)
                    .and_then(|a| a.iter().find(|e| element_id(e) == Some(id.as_str())));
                let seed;
                let elem = match existing {
                    Some(e) => e,
                    None => {
                        seed = json!({ "id": id });
                        &seed
                    }
                };
                match rules.item_keep(&path[..i], elem) {
                    ItemKeep::All => {}
                    ItemKeep::Only(fields) => {
                        // 本机没有的这类元素不凭空创建；有的只允许名单里的字段
                        if existing.is_none() {
                            return false;
                        }
                        match path.get(i + 1) {
                            Some(Seg::Key(f)) if fields.contains(&unescape_key(f)) => {}
                            _ => return false,
                        }
                    }
                }
                cur = existing;
            }
            Seg::Order { .. } => return false,
        }
    }
    true
}

/// 容器类型对齐：下一段是 `Id` 就得是数组，否则得是对象；类型不对（含 null / 标量）就换成空容器。
fn coerce_container(v: &mut Value, next: &Seg) {
    if next.is_id() {
        if !v.is_array() {
            *v = Value::Array(Vec::new());
        }
    } else if !v.is_object() {
        *v = Value::Object(Map::new());
    }
}

/// 写一个叶子，沿途缺的对象 / 数组 / 元素自动建（元素以 `{ "id": .. }` 起步）。
/// 返回新建了元素的那些位置（`path` 里的下标 i：`path[..i]` 是该元素所在的列表）。
fn write_leaf(root: &mut Value, path: &[Seg], value: Value) -> Vec<usize> {
    let mut created = Vec::new();
    let Some(first) = path.first() else {
        *root = value;
        return created;
    };
    coerce_container(root, first);
    let mut cur: &mut Value = root;
    for (i, seg) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        match seg {
            Seg::Key(k) => {
                let Some(obj) = cur.as_object_mut() else {
                    return created;
                };
                let key = unescape_key(k).to_string();
                if last {
                    obj.insert(key, value);
                    return created;
                }
                let child = obj.entry(key).or_insert(Value::Null);
                coerce_container(child, &path[i + 1]);
                cur = child;
            }
            Seg::Id { id } => {
                if last {
                    return created; // 元素本身不是叶子
                }
                let Some(arr) = cur.as_array_mut() else {
                    return created;
                };
                let pos = match arr.iter().position(|e| element_id(e) == Some(id.as_str())) {
                    Some(p) => p,
                    None => {
                        arr.push(json!({ "id": id }));
                        created.push(i);
                        arr.len() - 1
                    }
                };
                cur = &mut arr[pos];
            }
            Seg::Order { .. } => return created,
        }
    }
    created
}

/// 删一个叶子（不存在就什么也不做）。
fn remove_leaf(root: &mut Value, path: &[Seg]) {
    let Some((last, parent_path)) = path.split_last() else {
        return;
    };
    let Seg::Key(k) = last else { return };
    if let Some(Value::Object(m)) = get_mut(root, parent_path) {
        m.remove(unescape_key(k));
    }
}

/// 从列表里移除整个元素（元素级过滤说「这个元素是本机的 / 内置的」则不动）。
fn remove_element(root: &mut Value, list_path: &[Seg], id: &str, rules: &ScopeRules) {
    let Some(Value::Array(arr)) = get_mut(root, list_path) else {
        return;
    };
    let Some(first) = arr.iter().find(|e| element_id(e) == Some(id)) else {
        return;
    };
    if rules.item_keep(list_path, first) != ItemKeep::All {
        return;
    }
    arr.retain(|e| element_id(e) != Some(id));
}

/// 按镜像里的顺序叶子重排数组（顺序叶子不可见时按空名单处理，即全部走兜底排序）：
/// 顺序叶子里列出的活元素在最前（按其顺序）→ 顺序里没提到的活元素按其最早叶子 hlc 排在后面
/// → 本机有、镜像不认识的元素（没有 id、重复 id、被作用域挡在镜像之外的）保持相对位置排在最后。
fn reorder(
    doc: &mut Value,
    doc_path: &[Seg],
    mirror_path: &[Seg],
    visible: &BTreeMap<Path, Value>,
    mirror: &Mirror,
) {
    let Some(Value::Array(arr)) = get_mut(doc, doc_path) else {
        return;
    };
    let element_path = |id: &str| -> Path {
        let mut p = mirror_path.to_vec();
        p.push(Seg::id(id));
        p
    };

    let mut order_key = mirror_path.to_vec();
    order_key.push(Seg::ORDER);
    let order: Vec<&str> = visible
        .get(&order_key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut known: HashMap<String, Value> = HashMap::new();
    let mut unknown: Vec<Value> = Vec::new();
    for el in std::mem::take(arr) {
        match element_id(&el).map(str::to_owned) {
            Some(id)
                if !known.contains_key(&id) && has_visible_under(visible, &element_path(&id)) =>
            {
                known.insert(id, el);
            }
            _ => unknown.push(el),
        }
    }

    let mut result: Vec<Value> = Vec::with_capacity(known.len() + unknown.len());
    for id in order {
        if let Some(el) = known.remove(id) {
            result.push(el);
        }
    }
    let mut rest: Vec<(String, Value)> = known.into_iter().collect();
    rest.sort_by(|(a, _), (b, _)| {
        let ha = mirror.element_first_hlc(&element_path(a));
        let hb = mirror.element_first_hlc(&element_path(b));
        ha.cmp(&hb).then_with(|| a.cmp(b))
    });
    result.extend(rest.into_iter().map(|(_, v)| v));
    result.extend(unknown);
    *arr = result;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::core::hlc::Hlc;
    use crate::sync::core::mirror::{affected, Op, OpKind};
    use crate::sync::core::scope::ItemFilter;

    fn k(s: &str) -> Seg {
        Seg::key(s)
    }
    fn id(s: &str) -> Seg {
        Seg::id(s)
    }
    fn h(ms: u64, dev: &str) -> Hlc {
        Hlc {
            ms,
            counter: 0,
            device: dev.into(),
        }
    }
    fn set(path: Path, v: Value, ms: u64) -> Op {
        Op {
            source: "t".into(),
            path,
            kind: OpKind::Set(v),
            hlc: h(ms, "R"),
        }
    }
    fn rm(path: Path, ms: u64) -> Op {
        Op {
            source: "t".into(),
            path,
            kind: OpKind::Remove,
            hlc: h(ms, "R"),
        }
    }
    fn tomb(path: Path, ms: u64) -> Op {
        Op {
            source: "t".into(),
            path,
            kind: OpKind::Tombstone,
            hlc: h(ms, "R"),
        }
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
                Some(i) if i.starts_with("tmp-") => Some(&[]),
                _ => None,
            },
        }],
    };

    /// 把一批 op 并入镜像，对本机文档落地。返回新文档。
    fn apply_ops(local: &Value, mirror: &mut Mirror, ops: &[Op]) -> Value {
        apply_ops_on(local, mirror, ops, Platform::Desktop)
    }

    fn apply_ops_on(local: &Value, mirror: &mut Mirror, ops: &[Op], platform: Platform) -> Value {
        let before = mirror.visible();
        for o in ops {
            mirror.merge(o);
        }
        let aff = affected(&before, &mirror.visible());
        apply_to_doc(local, mirror, &aff, &RULES, platform)
    }

    fn ids(doc: &Value, list: &str) -> Vec<String> {
        doc[list]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn writes_scalars_and_creates_missing_parents() {
        let mut m = Mirror::new();
        let doc = apply_ops(
            &json!({}),
            &mut m,
            &[
                set(vec![k("a")], json!(1), 10),
                set(vec![k("b"), k("c"), k("d")], json!("deep"), 10),
            ],
        );
        assert_eq!(doc, json!({"a": 1, "b": {"c": {"d": "deep"}}}));
    }

    #[test]
    fn null_local_document_is_treated_as_empty() {
        let mut m = Mirror::new();
        let doc = apply_ops(&Value::Null, &mut m, &[set(vec![k("a")], json!(1), 10)]);
        assert_eq!(doc, json!({"a": 1}));
    }

    #[test]
    fn keeps_local_only_and_unknown_fields_untouched() {
        let local = json!({
            "mine": {"window": 100},
            "window": {"w": 1},
            "future_field_from_newer_version": {"x": [1, 2]},
            "a": 1,
            "conns": [{"id": "c1", "host": "old", "keyPath": "/home/me/key", "newer": true}],
        });
        let mut m = Mirror::new();
        let doc = apply_ops(
            &local,
            &mut m,
            &[
                set(vec![k("a")], json!(2), 10),
                set(vec![k("conns"), id("c1"), k("host")], json!("new"), 10),
            ],
        );
        assert_eq!(doc["a"], json!(2));
        assert_eq!(doc["conns"][0]["host"], json!("new"));
        // 其余原样
        assert_eq!(doc["mine"], local["mine"]);
        assert_eq!(doc["window"], local["window"]);
        assert_eq!(
            doc["future_field_from_newer_version"],
            local["future_field_from_newer_version"]
        );
        assert_eq!(doc["conns"][0]["keyPath"], json!("/home/me/key"));
        assert_eq!(doc["conns"][0]["newer"], json!(true));
    }

    #[test]
    fn does_not_touch_visible_leaves_that_are_not_affected() {
        let local = json!({"a": 1, "b": 2});
        let mut m = Mirror::new();
        m.merge(&set(vec![k("a")], json!(100), 10)); // 镜像里 a 是 100，但没列入 affected
        m.merge(&set(vec![k("b")], json!(200), 10));
        let doc = apply_to_doc(&local, &m, &[vec![k("b")]], &RULES, Platform::Desktop);
        assert_eq!(doc, json!({"a": 1, "b": 200}));
    }

    #[test]
    fn local_scope_paths_from_remote_are_ignored() {
        let local = json!({"window": {"w": 1}, "conns": [{"id": "c1", "keyPath": "/mine"}]});
        let mut m = Mirror::new();
        let doc = apply_ops(
            &local,
            &mut m,
            &[
                set(vec![k("window"), k("w")], json!(999), 10),
                set(
                    vec![k("conns"), id("c1"), k("keyPath")],
                    json!("/theirs"),
                    10,
                ),
            ],
        );
        assert_eq!(doc, local);
    }

    #[test]
    fn adds_new_element_with_all_fields() {
        let local = json!({"conns": [{"id": "a", "host": "h"}]});
        let mut m = Mirror::new();
        let doc = apply_ops(
            &local,
            &mut m,
            &[
                set(vec![k("conns"), id("a"), k("id")], json!("a"), 5),
                set(vec![k("conns"), id("a"), k("host")], json!("h"), 5),
                set(vec![k("conns"), id("b"), k("id")], json!("b"), 10),
                set(vec![k("conns"), id("b"), k("host")], json!("h2"), 10),
                set(vec![k("conns"), id("b"), k("tags")], json!(["x"]), 10),
                set(vec![k("conns"), Seg::ORDER], json!(["a", "b"]), 10),
            ],
        );
        assert_eq!(ids(&doc, "conns"), ["a", "b"]);
        assert_eq!(
            doc["conns"][1],
            json!({"id": "b", "host": "h2", "tags": ["x"]})
        );
    }

    #[test]
    fn removes_element_on_tombstone_but_keeps_local_only_ones() {
        let local = json!({"conns": [
            {"id": "a", "host": "h"},
            {"id": "mcp-temp-1", "host": "tmp"},
            {"id": "b", "host": "h2"},
            {"nameless": true},
        ]});
        let mut m = Mirror::new();
        // 先让镜像知道 a、b
        let first = [
            set(vec![k("conns"), id("a"), k("id")], json!("a"), 5),
            set(vec![k("conns"), id("a"), k("host")], json!("h"), 5),
            set(vec![k("conns"), id("b"), k("id")], json!("b"), 5),
            set(vec![k("conns"), id("b"), k("host")], json!("h2"), 5),
        ];
        let doc = apply_ops(&local, &mut m, &first);
        assert_eq!(doc, local, "对已有内容的落地不应改变文档");
        // 远端删除 a
        let doc = apply_ops(&doc, &mut m, &[tomb(vec![k("conns"), id("a")], 20)]);
        let remaining: Vec<_> = doc["conns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.to_string())
            .collect();
        assert_eq!(remaining.len(), 3);
        assert!(!remaining.iter().any(|s| s.contains("\"a\"")));
        assert!(remaining.iter().any(|s| s.contains("mcp-temp-1")));
        assert!(remaining.iter().any(|s| s.contains("nameless")));
    }

    #[test]
    fn removes_leaf_but_not_siblings_and_leaves_empty_parent_object() {
        let local = json!({"b": {"c": 1}, "keep": 2});
        let mut m = Mirror::new();
        m.merge(&set(vec![k("b"), k("c")], json!(1), 5));
        let doc = apply_ops(&local, &mut m, &[rm(vec![k("b"), k("c")], 10)]);
        assert_eq!(doc, json!({"b": {}, "keep": 2}));
    }

    #[test]
    fn removing_a_leaf_of_a_surviving_element() {
        let local = json!({"l": [{"id": "a", "n": 1, "note": "x"}]});
        let mut m = Mirror::new();
        for (f, v) in [("id", json!("a")), ("n", json!(1)), ("note", json!("x"))] {
            m.merge(&set(vec![k("l"), id("a"), k(f)], v, 5));
        }
        let doc = apply_ops(&local, &mut m, &[rm(vec![k("l"), id("a"), k("note")], 10)]);
        assert_eq!(doc, json!({"l": [{"id": "a", "n": 1}]}));
    }

    #[test]
    fn order_leaf_disappearing_deletes_nothing() {
        let local = json!({"l": [{"id": "a"}]});
        let mut m = Mirror::new();
        m.merge(&set(vec![k("l"), id("a"), k("id")], json!("a"), 5));
        m.merge(&set(vec![k("l"), Seg::ORDER], json!(["a"]), 5));
        let doc = apply_ops(&local, &mut m, &[rm(vec![k("l"), Seg::ORDER], 10)]);
        assert_eq!(doc, local);
    }

    #[test]
    fn reorders_by_order_leaf() {
        let local = json!({"l": [{"id": "a"}, {"id": "b"}, {"id": "c"}]});
        let mut m = Mirror::new();
        let mut ops: Vec<Op> = ["a", "b", "c"]
            .iter()
            .map(|e| set(vec![k("l"), id(e), k("id")], json!(e), 5))
            .collect();
        ops.push(set(vec![k("l"), Seg::ORDER], json!(["a", "b", "c"]), 5));
        let doc = apply_ops(&local, &mut m, &ops);
        assert_eq!(ids(&doc, "l"), ["a", "b", "c"]);
        let doc = apply_ops(
            &doc,
            &mut m,
            &[set(vec![k("l"), Seg::ORDER], json!(["c", "a", "b"]), 10)],
        );
        assert_eq!(ids(&doc, "l"), ["c", "a", "b"]);
    }

    #[test]
    fn reorder_puts_unlisted_alive_elements_by_first_hlc_then_local_only_last() {
        let local = json!({"l": [
            {"id": "local-only"},
            {"id": "a"},
            {"id": "late"},
            {"id": "early"},
            {"no": "id"},
        ]});
        let mut m = Mirror::new();
        let ops = vec![
            set(vec![k("l"), id("a"), k("id")], json!("a"), 5),
            set(vec![k("l"), id("late"), k("id")], json!("late"), 30),
            set(vec![k("l"), id("early"), k("id")], json!("early"), 20),
            set(vec![k("l"), Seg::ORDER], json!(["a"]), 40),
        ];
        let doc = apply_ops(&local, &mut m, &ops);
        let got: Vec<_> = doc["l"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                e.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("<no-id>")
                    .to_string()
            })
            .collect();
        assert_eq!(got, ["a", "early", "late", "local-only", "<no-id>"]);
    }

    #[test]
    fn dead_ids_in_order_leaf_are_ignored() {
        let local = json!({"l": [{"id": "a"}]});
        let mut m = Mirror::new();
        let doc = apply_ops(
            &local,
            &mut m,
            &[
                set(vec![k("l"), id("a"), k("id")], json!("a"), 5),
                set(
                    vec![k("l"), Seg::ORDER],
                    json!(["ghost", "a", "ghost2"]),
                    10,
                ),
            ],
        );
        assert_eq!(ids(&doc, "l"), ["a"]);
    }

    #[test]
    fn revived_element_is_recreated_with_all_visible_leaves() {
        let mut m = Mirror::new();
        let mut doc = apply_ops(
            &json!({}),
            &mut m,
            &[
                set(vec![k("l"), id("a"), k("id")], json!("a"), 5),
                set(vec![k("l"), id("a"), k("name")], json!("N"), 5),
                set(vec![k("l"), id("a"), k("color")], json!("red"), 5),
                set(vec![k("l"), Seg::ORDER], json!(["a"]), 5),
            ],
        );
        doc = apply_ops(&doc, &mut m, &[tomb(vec![k("l"), id("a")], 10)]);
        assert_eq!(doc["l"], json!([]));
        // 墓碑之后有人只改了 name：整个元素回来
        doc = apply_ops(
            &doc,
            &mut m,
            &[set(vec![k("l"), id("a"), k("name")], json!("N2"), 20)],
        );
        assert_eq!(doc["l"], json!([{"id": "a", "name": "N2", "color": "red"}]));
    }

    #[test]
    fn nested_element_creation_and_reorder() {
        let mut m = Mirror::new();
        let doc = apply_ops(
            &json!({}),
            &mut m,
            &[
                set(vec![k("l"), id("a"), k("id")], json!("a"), 5),
                set(
                    vec![k("l"), id("a"), k("sub"), id("s1"), k("g")],
                    json!(1),
                    5,
                ),
                set(
                    vec![k("l"), id("a"), k("sub"), id("s2"), k("g")],
                    json!(2),
                    5,
                ),
                set(
                    vec![k("l"), id("a"), k("sub"), Seg::ORDER],
                    json!(["s2", "s1"]),
                    5,
                ),
                set(vec![k("l"), Seg::ORDER], json!(["a"]), 5),
            ],
        );
        assert_eq!(
            doc,
            json!({"l": [{"id": "a", "sub": [{"id": "s2", "g": 2}, {"id": "s1", "g": 1}]}]})
        );
    }

    #[test]
    fn elements_without_id_and_scalars_in_registered_arrays_are_preserved() {
        let local = json!({"l": [1, "x", {"id": "a"}]});
        let mut m = Mirror::new();
        let doc = apply_ops(
            &local,
            &mut m,
            &[
                set(vec![k("l"), id("a"), k("id")], json!("a"), 5),
                set(vec![k("l"), id("a"), k("v")], json!(1), 5),
            ],
        );
        assert_eq!(doc["l"].as_array().unwrap().len(), 3);
        assert_eq!(doc["l"][2], json!({"id": "a", "v": 1}));
    }

    #[test]
    fn order_leaf_removed_falls_back_to_first_hlc_order() {
        // 顺序叶子被删（或输给了更晚的删除）而元素还活着：各设备要退回统一的兜底顺序，不能各停各的
        let local = json!({"l": [{"id": "c"}, {"id": "a"}, {"id": "b"}]});
        let mut m = Mirror::new();
        let mut ops: Vec<Op> = [("a", 10), ("b", 20), ("c", 30)]
            .iter()
            .map(|(e, ms)| set(vec![k("l"), id(e), k("id")], json!(e), *ms))
            .collect();
        ops.push(set(vec![k("l"), Seg::ORDER], json!(["c", "a", "b"]), 40));
        let doc = apply_ops(&local, &mut m, &ops);
        assert_eq!(ids(&doc, "l"), ["c", "a", "b"]);
        let doc = apply_ops(&doc, &mut m, &[rm(vec![k("l"), Seg::ORDER], 50)]);
        assert_eq!(ids(&doc, "l"), ["a", "b", "c"]);
    }

    // ---------------- affected_vs_doc ----------------

    fn vs_doc(doc: &Value, m: &Mirror, platform: Platform) -> Vec<Path> {
        affected_vs_doc(doc, m, &RULES, platform, &mut IdMapRegistry::new())
    }

    #[test]
    fn vs_doc_is_empty_when_document_already_matches_mirror() {
        let doc = json!({"a": 1, "l": [{"id": "x", "n": 1}, {"id": "y"}], "window": {"w": 1}});
        let mut m = Mirror::new();
        for (p, v) in [
            (vec![k("a")], json!(1)),
            (vec![k("l"), id("x"), k("id")], json!("x")),
            (vec![k("l"), id("x"), k("n")], json!(1)),
            (vec![k("l"), id("y"), k("id")], json!("y")),
            (vec![k("l"), Seg::ORDER], json!(["x", "y"])),
        ] {
            m.merge(&set(p, v, 5));
        }
        assert!(vs_doc(&doc, &m, Platform::Desktop).is_empty());
    }

    #[test]
    fn vs_doc_lists_changed_missing_and_dead_leaves() {
        let doc = json!({"a": 1, "gone": 2, "l": [{"id": "dead"}, {"id": "x", "n": 1}]});
        let mut m = Mirror::new();
        for (p, v) in [
            (vec![k("a")], json!(100)),    // 值不同
            (vec![k("new")], json!(true)), // 文档里没有
            (vec![k("l"), id("x"), k("id")], json!("x")),
            (vec![k("l"), id("x"), k("n")], json!(1)),
            (vec![k("l"), id("dead"), k("id")], json!("dead")),
        ] {
            m.merge(&set(p, v, 5));
        }
        m.merge(&rm(vec![k("gone")], 6)); // 云端删了的字段
        m.merge(&tomb(vec![k("l"), id("dead")], 7)); // 云端删了的元素
        let aff = vs_doc(&doc, &m, Platform::Desktop);
        assert!(aff.contains(&vec![k("a")]));
        assert!(aff.contains(&vec![k("new")]));
        assert!(aff.contains(&vec![k("gone")]));
        assert!(aff.contains(&vec![k("l"), id("dead"), k("id")]));
        let out = apply_to_doc(&doc, &m, &aff, &RULES, Platform::Desktop);
        assert_eq!(out["a"], json!(100));
        assert_eq!(out["new"], json!(true));
        assert!(out.get("gone").is_none());
        assert_eq!(ids(&out, "l"), ["x"]);
    }

    #[test]
    fn vs_doc_drops_other_platforms_and_local_scope_paths() {
        let mut m = Mirror::new();
        m.merge(&set(
            vec![k("@android"), k("notify"), k("sound")],
            json!(true),
            5,
        ));
        m.merge(&set(
            vec![k("@desktop"), k("notify"), k("sound")],
            json!(false),
            5,
        ));
        m.merge(&set(vec![k("window"), k("w")], json!(9), 5)); // 本机认为是 Local
        let aff = vs_doc(&json!({}), &m, Platform::Desktop);
        assert_eq!(aff, vec![vec![k("@desktop"), k("notify"), k("sound")]]);
        let aff = vs_doc(&json!({}), &m, Platform::Android);
        assert_eq!(aff, vec![vec![k("@android"), k("notify"), k("sound")]]);
    }

    #[test]
    fn vs_doc_then_apply_is_a_fixed_point() {
        let mut m = Mirror::new();
        for o in [
            set(vec![k("a")], json!(1), 5),
            set(vec![k("l"), id("x"), k("id")], json!("x"), 5),
            set(vec![k("l"), id("y"), k("id")], json!("y"), 6),
            set(vec![k("l"), Seg::ORDER], json!(["y", "x"]), 7),
            set(vec![k("l"), id("late"), k("id")], json!("late"), 9),
        ] {
            m.merge(&o);
        }
        let doc = json!({"l": [{"id": "x"}], "extra": 1});
        let aff = vs_doc(&doc, &m, Platform::Desktop);
        let once = apply_to_doc(&doc, &m, &aff, &RULES, Platform::Desktop);
        // 文档里镜像不知道的 `extra` 会被当成多余的删掉，这是"落地前必须先捕获"的前提；
        // 先把它当作本机数据并入镜像（ZERO hlc）就不会了。
        assert!(once.get("extra").is_none());
        let aff2 = vs_doc(&once, &m, Platform::Desktop);
        let twice = apply_to_doc(&once, &m, &aff2, &RULES, Platform::Desktop);
        assert_eq!(once, twice);
        assert_eq!(ids(&once, "l"), ["y", "x", "late"]);
    }

    // ---------------- 平台 ----------------

    #[test]
    fn platform_prefixed_leaves_apply_only_on_matching_platform() {
        let ops = vec![
            set(
                vec![k("@desktop"), k("notify"), k("sound")],
                json!(true),
                10,
            ),
            set(
                vec![k("@android"), k("notify"), k("sound")],
                json!(false),
                10,
            ),
            set(vec![k("theme")], json!("dark"), 10),
        ];
        let mut m = Mirror::new();
        let d = apply_ops_on(&json!({}), &mut m, &ops, Platform::Desktop);
        assert_eq!(d, json!({"notify": {"sound": true}, "theme": "dark"}));
        let mut m2 = Mirror::new();
        let a = apply_ops_on(&json!({}), &mut m2, &ops, Platform::Android);
        assert_eq!(a, json!({"notify": {"sound": false}, "theme": "dark"}));
    }

    #[test]
    fn platform_scope_mismatch_between_versions_is_ignored() {
        // 对方带前缀但本机认为它是共享的 / 对方没带前缀但本机认为它是平台作用域的：都不写
        let mut m = Mirror::new();
        let doc = apply_ops(
            &json!({}),
            &mut m,
            &[
                set(vec![k("@desktop"), k("theme")], json!("x"), 10),
                set(vec![k("notify"), k("sound")], json!(true), 10),
            ],
        );
        assert_eq!(doc, json!({}));
    }

    #[test]
    fn unknown_platform_prefix_is_ignored() {
        let mut m = Mirror::new();
        let doc = apply_ops(
            &json!({}),
            &mut m,
            &[set(
                vec![k("@ios"), k("notify"), k("sound")],
                json!(true),
                10,
            )],
        );
        assert_eq!(doc, json!({}));
    }

    #[test]
    fn platform_scoped_id_lists_reorder_under_the_prefix() {
        // notify 在规则里是 Platform：它下面的 id 列表要用带前缀的顺序叶子重排
        let mut m = Mirror::new();
        let ops = vec![
            set(
                vec![k("@desktop"), k("notify"), k("rules"), id("r1"), k("id")],
                json!("r1"),
                5,
            ),
            set(
                vec![k("@desktop"), k("notify"), k("rules"), id("r2"), k("id")],
                json!("r2"),
                5,
            ),
            set(
                vec![k("@desktop"), k("notify"), k("rules"), Seg::ORDER],
                json!(["r2", "r1"]),
                5,
            ),
        ];
        let doc = apply_ops(&json!({}), &mut m, &ops);
        assert_eq!(
            doc,
            json!({"notify": {"rules": [{"id": "r2"}, {"id": "r1"}]}})
        );
    }

    // ---------------- 元素级过滤 ----------------

    #[test]
    fn item_filter_applies_listed_fields_only_and_never_creates_or_deletes_builtin() {
        let local = json!({"skills": [
            {"id": "builtin-a", "name": "A", "enabled": true},
            {"id": "user-1", "name": "U"},
        ]});
        let mut m = Mirror::new();
        let ops = vec![
            // 内置 a：enabled 应落地，name 不该（对方版本的内容不是我们的）
            set(
                vec![k("skills"), id("builtin-a"), k("enabled")],
                json!(false),
                10,
            ),
            set(
                vec![k("skills"), id("builtin-a"), k("name")],
                json!("HACK"),
                10,
            ),
            // 本机没有的内置 b：不凭空创建
            set(
                vec![k("skills"), id("builtin-b"), k("enabled")],
                json!(true),
                10,
            ),
            // 本机整条 Local 的临时元素：不写
            set(
                vec![k("skills"), id("tmp-1"), k("enabled")],
                json!(true),
                10,
            ),
            // 用户 skill 正常
            set(vec![k("skills"), id("user-1"), k("name")], json!("U2"), 10),
            set(
                vec![k("skills"), id("user-2"), k("id")],
                json!("user-2"),
                10,
            ),
        ];
        let doc = apply_ops(&local, &mut m, &ops);
        assert_eq!(
            doc,
            json!({"skills": [
                {"id": "builtin-a", "name": "A", "enabled": false},
                {"id": "user-1", "name": "U2"},
                {"id": "user-2"},
            ]})
        );
        // 对内置 a 的墓碑也不会删掉它
        let doc2 = apply_ops(
            &doc,
            &mut m,
            &[tomb(vec![k("skills"), id("builtin-a")], 50)],
        );
        assert!(ids(&doc2, "skills").contains(&"builtin-a".to_string()));
    }

    // ---------------- 其它形状 ----------------

    #[test]
    fn leaf_replaced_by_object_and_back() {
        let mut m = Mirror::new();
        let mut doc = apply_ops(&json!({}), &mut m, &[set(vec![k("a")], json!("str"), 5)]);
        assert_eq!(doc, json!({"a": "str"}));
        // a 变成了对象：旧叶子被 Remove、新叶子 Set
        doc = apply_ops(
            &doc,
            &mut m,
            &[
                rm(vec![k("a")], 10),
                set(vec![k("a"), k("z")], json!(1), 10),
            ],
        );
        assert_eq!(doc, json!({"a": {"z": 1}}));
        // 又变回去
        doc = apply_ops(
            &doc,
            &mut m,
            &[
                rm(vec![k("a"), k("z")], 20),
                set(vec![k("a")], json!("again"), 20),
            ],
        );
        assert_eq!(doc, json!({"a": "again"}));
    }

    #[test]
    fn stale_atomic_array_is_replaced_when_the_position_becomes_id_mapped() {
        let local = json!({"item": {"tags": []}});
        let mut m = Mirror::new();
        m.merge(&set(vec![k("item"), k("tags")], json!([]), 1));
        let doc = apply_ops(
            &local,
            &mut m,
            &[
                set(
                    vec![k("item"), k("tags"), id("t1"), k("id")],
                    json!("t1"),
                    10,
                ),
                set(vec![k("item"), k("tags"), Seg::ORDER], json!(["t1"]), 10),
            ],
        );
        assert_eq!(doc, json!({"item": {"tags": [{"id": "t1"}]}}));
    }

    #[test]
    fn at_prefixed_json_keys_round_trip_through_escaping() {
        let mut m = Mirror::new();
        let doc = apply_ops(&json!({}), &mut m, &[set(vec![k("@@weird")], json!(1), 10)]);
        assert_eq!(doc, json!({"@weird": 1}));
        let doc = apply_ops(&doc, &mut m, &[rm(vec![k("@@weird")], 20)]);
        assert_eq!(doc, json!({}));
    }

    #[test]
    fn root_level_array_document() {
        let local = json!([{"id": "a", "n": 1}]);
        let mut m = Mirror::new();
        let doc = apply_ops(
            &local,
            &mut m,
            &[
                set(vec![id("a"), k("id")], json!("a"), 5),
                set(vec![id("a"), k("n")], json!(1), 5),
                set(vec![id("b"), k("id")], json!("b"), 10),
                set(vec![Seg::ORDER], json!(["b", "a"]), 10),
            ],
        );
        assert_eq!(doc, json!([{"id": "b"}, {"id": "a", "n": 1}]));
        let doc = apply_ops(&doc, &mut m, &[tomb(vec![id("b")], 20)]);
        assert_eq!(doc, json!([{"id": "a", "n": 1}]));
    }

    #[test]
    fn applying_nothing_returns_equal_document() {
        let local = json!({"a": 1, "l": [{"id": "x"}]});
        let m = Mirror::new();
        assert_eq!(
            apply_to_doc(&local, &m, &[], &RULES, Platform::Desktop),
            local
        );
    }

    #[test]
    fn apply_is_idempotent() {
        let mut m = Mirror::new();
        let ops = vec![
            set(vec![k("a")], json!(1), 5),
            set(vec![k("l"), id("x"), k("id")], json!("x"), 5),
            set(vec![k("l"), id("y"), k("id")], json!("y"), 6),
            set(vec![k("l"), Seg::ORDER], json!(["y", "x"]), 7),
            tomb(vec![k("l"), id("x")], 8),
        ];
        let before = BTreeMap::new();
        for o in &ops {
            m.merge(o);
        }
        let aff = affected(&before, &m.visible());
        let once = apply_to_doc(&json!({}), &m, &aff, &RULES, Platform::Desktop);
        let twice = apply_to_doc(&once, &m, &aff, &RULES, Platform::Desktop);
        assert_eq!(once, twice);
        assert_eq!(once, json!({"a": 1, "l": [{"id": "y"}]}));
    }
}
