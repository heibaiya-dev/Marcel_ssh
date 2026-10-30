//! Op 与镜像：同步的数据结构本体。
//!
//! 镜像 [`Mirror`]（每个同步源一份）是「已知的、所有设备操作的并集」，是一个状态型 CRDT：
//! - 每个叶子保留 hlc 最大的那次写入（LWW）；删除叶子也是一次写入（`value = None`）；
//! - 删除一个 id 映射元素（整棵子树）记一个**墓碑**，墓碑取 hlc 最大者；
//! - 合并是「取并集再各自取最大」，满足交换、结合、幂等——任意顺序、任意重复合并同一批 op，
//!   得到完全一致的镜像（测试钉住，含固定种子的随机收敛性测试）。
//!
//! 可见性是镜像状态的纯函数（[`Mirror::visible`]），所以只要状态收敛，各设备落地出来的东西就一致：
//! - 一个 id 映射元素「活着」⇔ 它名下所有叶子条目（**含已删除的**）里最大的 hlc **大于**墓碑
//!   （无墓碑 = 活着）。活着的元素，它名下所有 `Some` 叶子都可见——墓碑之后有人改了这个元素，
//!   整个元素复活，而不是只剩被改的那一个字段。嵌套的元素逐层判定。
//!   「删字段」也算改动（与「写字段」同等）：否则本机把复活元素唯一那个较新的叶子删掉，
//!   元素就会在镜像里悄悄变回「已删除」——而本机文档里它还好好的，没有任何一次"远端合并前后对比"
//!   能发现这个变化，两台设备就此分叉（随机多设备测试发现的）。
//! - 同一路径上不会同时可见「叶子」与「它的后代」（类型换了的字段会出现这种事）：
//!   叶子的 hlc 严格大于它名下所有 `Some` 叶子条目（不论后代所在元素是否还活着）时叶子赢（后代隐去），
//!   否则后代赢（叶子隐去）。这样旧叶子一旦被后代压住就是永久的，后代之后被删光也不会"诈尸"。
//!
//! 已知的取舍：墓碑之后用**同一个 id** 重建元素，墓碑之前遗留的、新元素没有的字段会随整元素复活
//! 一起冒出来。id 是 UUID 时只会发生在「撤销删除」这类场景，那时新元素本来就带着全部旧字段。

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::hlc::{Hlc, HlcClock};
use super::path::{IdMapRegistry, Path, Seg};

/// 一次操作的种类。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "lowercase")]
pub enum OpKind {
    /// 写一个叶子。
    Set(Value),
    /// 删一个叶子。
    Remove,
    /// 删一个 id 映射元素（整棵子树）。`path` 以 `Seg::Id` 结尾。
    Tombstone,
}

/// 一条操作。`source` 是同步源 key（`settings` / `connections` …），镜像本身不看它，由调用方分流。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub source: String,
    pub path: Path,
    pub kind: OpKind,
    pub hlc: Hlc,
}

/// 镜像里的一个叶子：`value = None` 表示「被删除」（也要保留，删除同样是要参与 LWW 的一次写入）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafEntry {
    pub value: Option<Value>,
    pub hlc: Hlc,
}

/// 两次写入谁赢。hlc 大者赢；hlc 完全相同（不该发生，除非 bug / 损坏）时用内容兜底，
/// 保证任何情况下都是全序——收敛性不依赖「hlc 全局唯一」这个假设。
fn cmp_entry(a: &LeafEntry, b: &LeafEntry) -> Ordering {
    a.hlc.cmp(&b.hlc).then_with(|| match (&a.value, &b.value) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => {
            if x == y {
                Ordering::Equal
            } else {
                x.to_string().cmp(&y.to_string())
            }
        }
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mirror {
    leaves: BTreeMap<Path, LeafEntry>,
    tombstones: BTreeMap<Path, Hlc>,
}

impl Mirror {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty() && self.tombstones.is_empty()
    }

    pub fn leaf_count(&self) -> usize {
        self.leaves.len()
    }

    pub fn tombstone_count(&self) -> usize {
        self.tombstones.len()
    }

    pub fn leaf(&self, path: &[Seg]) -> Option<&LeafEntry> {
        self.leaves.get(path)
    }

    pub fn tombstone(&self, path: &[Seg]) -> Option<&Hlc> {
        self.tombstones.get(path)
    }

    pub fn leaves(&self) -> impl Iterator<Item = (&Path, &LeafEntry)> {
        self.leaves.iter()
    }

    pub fn tombstones(&self) -> impl Iterator<Item = (&Path, &Hlc)> {
        self.tombstones.iter()
    }

    /// 镜像里出现过的最大 hlc（含墓碑与已删除的叶子）。从快照 / 持久化恢复后用它推进本机时钟。
    pub fn max_hlc(&self) -> Option<Hlc> {
        self.leaves
            .values()
            .map(|e| &e.hlc)
            .chain(self.tombstones.values())
            .max()
            .cloned()
    }

    /// 合并一条 op。返回镜像状态是否变化（赢家变了）。同一 op 重复合并返回 `false`（幂等）。
    pub fn merge(&mut self, op: &Op) -> bool {
        match &op.kind {
            OpKind::Set(v) => self.merge_leaf(
                &op.path,
                LeafEntry {
                    value: Some(v.clone()),
                    hlc: op.hlc.clone(),
                },
            ),
            OpKind::Remove => self.merge_leaf(
                &op.path,
                LeafEntry {
                    value: None,
                    hlc: op.hlc.clone(),
                },
            ),
            OpKind::Tombstone => self.merge_tombstone(&op.path, &op.hlc),
        }
    }

    fn merge_leaf(&mut self, path: &[Seg], entry: LeafEntry) -> bool {
        match self.leaves.get_mut(path) {
            Some(cur) => {
                if cmp_entry(&entry, cur) == Ordering::Greater {
                    *cur = entry;
                    true
                } else {
                    false
                }
            }
            None => {
                self.leaves.insert(path.to_vec(), entry);
                true
            }
        }
    }

    fn merge_tombstone(&mut self, path: &[Seg], hlc: &Hlc) -> bool {
        match self.tombstones.get_mut(path) {
            Some(cur) => {
                if hlc > cur {
                    *cur = hlc.clone();
                    true
                } else {
                    false
                }
            }
            None => {
                self.tombstones.insert(path.to_vec(), hlc.clone());
                true
            }
        }
    }

    /// 并入另一份镜像（别的设备的快照）。等价于把它里面的所有 op 逐条合并进来。
    pub fn merge_mirror(&mut self, other: &Mirror) -> bool {
        let mut changed = false;
        for (p, e) in &other.leaves {
            changed |= self.merge_leaf(p, e.clone());
        }
        for (p, h) in &other.tombstones {
            changed |= self.merge_tombstone(p, h);
        }
        changed
    }

    /// 从镜像里所有路径学习 id 映射位置（加入同步 / 读到快照后用）。
    pub fn learn_into(&self, idmaps: &mut IdMapRegistry) {
        for p in self.leaves.keys().chain(self.tombstones.keys()) {
            idmaps.learn_path(p);
        }
    }

    /// 元素名下最早的叶子 hlc（只看 `Some` 叶子）。落地时给「顺序叶子没提到的活元素」排位用。
    pub fn element_first_hlc(&self, element: &[Seg]) -> Option<&Hlc> {
        self.leaves
            .range::<[Seg], _>((Bound::Included(element), Bound::Unbounded))
            .take_while(|(p, _)| p.starts_with(element))
            .filter(|(_, e)| e.value.is_some())
            .map(|(_, e)| &e.hlc)
            .min()
    }

    /// 当前可见的叶子（语义见模块注释）。路径带着平台前缀（如果有）。
    pub fn visible(&self) -> BTreeMap<Path, Value> {
        let cands = self.visible_candidates();
        let n = cands.len();

        // 叶子与后代的冲突：已排序，某叶子的所有后代恰好紧跟在它后面（连续一段）。
        // 比较的是该叶子与它名下**所有 Some 叶子条目**里最大的 hlc——不看后代是否可见 / 所在元素是否活着。
        // 这样「叶子被后代压住」是永久的：后代元素之后被删光，也不会让早已过时的旧叶子（比如换型前的
        // 原子 `[]` / `null`）重新冒出来；要让旧叶子回来，只有它自己被重新写入（hlc 更大）。
        let mut dropped = vec![false; n];
        for i in 0..n {
            let (path, hlc, _) = cands[i];
            let desc_max = self
                .leaves
                .range::<[Seg], _>((Bound::Excluded(path.as_slice()), Bound::Unbounded))
                .take_while(|(p, _)| p.starts_with(path))
                .filter(|(_, e)| e.value.is_some())
                .map(|(_, e)| &e.hlc)
                .max();
            if let Some(m) = desc_max {
                if hlc > m {
                    // 叶子压过全部后代：后代隐去
                    let mut j = i + 1;
                    while j < n && cands[j].0.starts_with(path) {
                        dropped[j] = true;
                        j += 1;
                    }
                } else {
                    dropped[i] = true;
                }
            }
        }

        cands
            .into_iter()
            .zip(dropped)
            .filter(|(_, d)| !*d)
            .map(|((p, _, v), _)| (p.clone(), v.clone()))
            .collect()
    }

    /// 通过元素存活判定的 `Some` 叶子（已按路径排序）。
    fn visible_candidates(&self) -> Vec<(&Path, &Hlc, &Value)> {
        // 只有挂了墓碑的元素才需要算「名下最大 hlc」；没有墓碑的元素天然活着。
        // 已删除的叶子也算数（见模块注释）。
        let mut elem_max: BTreeMap<&[Seg], &Hlc> = BTreeMap::new();
        if !self.tombstones.is_empty() {
            for (path, e) in &self.leaves {
                for (i, seg) in path.iter().enumerate() {
                    if seg.is_id() {
                        let pre = &path[..=i];
                        if self.tombstones.contains_key(pre) {
                            let slot = elem_max.entry(pre).or_insert(&e.hlc);
                            if &e.hlc > *slot {
                                *slot = &e.hlc;
                            }
                        }
                    }
                }
            }
        }
        let alive = |pre: &[Seg]| -> bool {
            match self.tombstones.get(pre) {
                None => true,
                Some(t) => elem_max.get(pre).is_some_and(|m| *m > t),
            }
        };

        let mut out = Vec::new();
        for (path, e) in &self.leaves {
            let Some(v) = &e.value else { continue };
            let ok = self.tombstones.is_empty()
                || path
                    .iter()
                    .enumerate()
                    .all(|(i, seg)| !seg.is_id() || alive(&path[..=i]));
            if ok {
                out.push((path, &e.hlc, v));
            }
        }
        out
    }
}

/// 两次可见集之间发生变化的路径（值变了、出现了、消失了），含顺序叶子。结果有序、无重复。
pub fn affected(before: &BTreeMap<Path, Value>, after: &BTreeMap<Path, Value>) -> Vec<Path> {
    let mut out = Vec::new();
    let mut b = before.iter().peekable();
    let mut a = after.iter().peekable();
    loop {
        match (b.peek(), a.peek()) {
            (None, None) => break,
            (Some((bp, _)), None) => {
                out.push((*bp).clone());
                b.next();
            }
            (None, Some((ap, _))) => {
                out.push((*ap).clone());
                a.next();
            }
            (Some((bp, bv)), Some((ap, av))) => match bp.cmp(ap) {
                Ordering::Less => {
                    out.push((*bp).clone());
                    b.next();
                }
                Ordering::Greater => {
                    out.push((*ap).clone());
                    a.next();
                }
                Ordering::Equal => {
                    if bv != av {
                        out.push((*bp).clone());
                    }
                    b.next();
                    a.next();
                }
            },
        }
    }
    out
}

/// 并入一批远端 op：学习 id 映射位置、把本机时钟推进到见过的最大 hlc、合并进镜像。
///
/// 时钟推进是正确性要求而不是礼节：之后本机发出的任何 op（比如对刚收到的元素的删除）
/// 的 hlc 必须大于它所依据的那些 op，否则墓碑会输给它想删的东西。所以别自己手写这段。
pub fn merge_remote_ops(
    mirror: &mut Mirror,
    ops: &[Op],
    idmaps: &mut IdMapRegistry,
    clock: &mut HlcClock,
    wall_ms: u64,
) {
    for op in ops {
        idmaps.learn_path(&op.path);
        clock.observe(&op.hlc, wall_ms);
        mirror.merge(op);
    }
}

/// 并入一批远端 op 并返回受影响的路径（**仅**比较合并前后的可见集）。
///
/// 运行时落地**不要用它**，用 [`merge_remote_ops`] + `apply::affected_vs_doc`（文档现状 vs 镜像）：
/// 本机的 op 并不总能赢（对方时钟更快的并发编辑 / 删除），也可能改变别的叶子的可见性，
/// 这些"文档已经偏离镜像"的地方只比合并前后是发现不了的（随机多设备测试里真的分叉过）。
/// 本函数只适合没有文档参与的场合（镜像层的单元测试、单步合并）。
pub fn ingest_remote(
    mirror: &mut Mirror,
    ops: &[Op],
    idmaps: &mut IdMapRegistry,
    clock: &mut HlcClock,
    wall_ms: u64,
) -> Vec<Path> {
    let before = mirror.visible();
    merge_remote_ops(mirror, ops, idmaps, clock, wall_ms);
    affected(&before, &mirror.visible())
}

/// 并入远端设备的整份镜像（快照）：学习 id 映射位置、推进时钟、合并。语义同 [`merge_remote_ops`]。
pub fn merge_remote_mirror(
    mirror: &mut Mirror,
    remote: &Mirror,
    idmaps: &mut IdMapRegistry,
    clock: &mut HlcClock,
    wall_ms: u64,
) {
    remote.learn_into(idmaps);
    if let Some(h) = remote.max_hlc() {
        clock.observe(&h, wall_ms);
    }
    mirror.merge_mirror(remote);
}

/// 并入远端整份镜像并返回受影响的路径（仅比较合并前后的可见集，使用上的限制见 [`ingest_remote`]）。
pub fn ingest_remote_mirror(
    mirror: &mut Mirror,
    remote: &Mirror,
    idmaps: &mut IdMapRegistry,
    clock: &mut HlcClock,
    wall_ms: u64,
) -> Vec<Path> {
    let before = mirror.visible();
    merge_remote_mirror(mirror, remote, idmaps, clock, wall_ms);
    affected(&before, &mirror.visible())
}

// ---------------------------------------------------------------------------
// 序列化：路径不能当 JSON 的 key，镜像存成两个列表。
//   {"leaves":[{"p":[..],"h":{..},"v":<值>,"rm":true?}],"tombstones":[{"p":[..],"h":{..}}]}
// `rm: true` = 已删除的叶子（此时没有 v）。不能拿 `v: null` 表示删除——null 本身是合法的叶子值。
// ---------------------------------------------------------------------------

static NULL: Value = Value::Null;

#[derive(Serialize)]
struct LeafWireRef<'a> {
    p: &'a Path,
    h: &'a Hlc,
    #[serde(skip_serializing_if = "Value::is_null")]
    v: &'a Value,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    rm: bool,
}

#[derive(Serialize)]
struct TombWireRef<'a> {
    p: &'a Path,
    h: &'a Hlc,
}

struct LeavesSer<'a>(&'a BTreeMap<Path, LeafEntry>);
struct TombsSer<'a>(&'a BTreeMap<Path, Hlc>);

impl Serialize for LeavesSer<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(|(p, e)| LeafWireRef {
            p,
            h: &e.hlc,
            v: e.value.as_ref().unwrap_or(&NULL),
            rm: e.value.is_none(),
        }))
    }
}

impl Serialize for TombsSer<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(|(p, h)| TombWireRef { p, h }))
    }
}

impl Serialize for Mirror {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Mirror", 2)?;
        st.serialize_field("leaves", &LeavesSer(&self.leaves))?;
        st.serialize_field("tombstones", &TombsSer(&self.tombstones))?;
        st.end()
    }
}

#[derive(Deserialize)]
struct LeafWire {
    p: Path,
    h: Hlc,
    #[serde(default)]
    v: Value,
    #[serde(default)]
    rm: bool,
}

#[derive(Deserialize)]
struct TombWire {
    p: Path,
    h: Hlc,
}

#[derive(Deserialize)]
struct MirrorWire {
    #[serde(default)]
    leaves: Vec<LeafWire>,
    #[serde(default)]
    tombstones: Vec<TombWire>,
}

impl<'de> Deserialize<'de> for Mirror {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = MirrorWire::deserialize(d)?;
        let mut m = Mirror::new();
        // 走 merge 而不是直接 insert：文件里万一有重复路径也能按 LWW 收敛
        for l in w.leaves {
            m.merge_leaf(
                &l.p,
                LeafEntry {
                    value: if l.rm { None } else { Some(l.v) },
                    hlc: l.h,
                },
            );
        }
        for t in w.tombstones {
            m.merge_tombstone(&t.p, &t.h);
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::core::test_util::Rng;
    use serde_json::json;

    fn k(s: &str) -> Seg {
        Seg::key(s)
    }
    fn id(s: &str) -> Seg {
        Seg::id(s)
    }
    fn h(ms: u64, counter: u32, dev: &str) -> Hlc {
        Hlc {
            ms,
            counter,
            device: dev.into(),
        }
    }
    fn set(path: Path, v: Value, hlc: Hlc) -> Op {
        Op {
            source: "t".into(),
            path,
            kind: OpKind::Set(v),
            hlc,
        }
    }
    fn rm(path: Path, hlc: Hlc) -> Op {
        Op {
            source: "t".into(),
            path,
            kind: OpKind::Remove,
            hlc,
        }
    }
    fn tomb(path: Path, hlc: Hlc) -> Op {
        Op {
            source: "t".into(),
            path,
            kind: OpKind::Tombstone,
            hlc,
        }
    }
    fn merged(ops: &[Op]) -> Mirror {
        let mut m = Mirror::new();
        for o in ops {
            m.merge(o);
        }
        m
    }
    fn el(l: &str, e: &str) -> Path {
        vec![k(l), id(e)]
    }
    fn leaf(l: &str, e: &str, f: &str) -> Path {
        vec![k(l), id(e), k(f)]
    }

    // ---------------- LWW ----------------

    #[test]
    fn lww_latest_hlc_wins_regardless_of_arrival_order() {
        let a = set(vec![k("x")], json!(1), h(10, 0, "A"));
        let b = set(vec![k("x")], json!(2), h(20, 0, "B"));
        for ops in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let m = merged(&ops);
            assert_eq!(m.visible()[&vec![k("x")]], json!(2));
        }
    }

    #[test]
    fn merge_reports_whether_winner_changed() {
        let mut m = Mirror::new();
        assert!(m.merge(&set(vec![k("x")], json!(1), h(10, 0, "A"))));
        assert!(
            !m.merge(&set(vec![k("x")], json!(0), h(5, 0, "B"))),
            "旧的不改变赢家"
        );
        assert!(m.merge(&set(vec![k("x")], json!(3), h(11, 0, "B"))));
    }

    #[test]
    fn same_hlc_same_op_is_idempotent() {
        let mut m = Mirror::new();
        let op = set(vec![k("x")], json!(1), h(10, 0, "A"));
        assert!(m.merge(&op));
        assert!(!m.merge(&op));
        assert!(!m.merge(&op));
        let t = tomb(el("l", "e"), h(10, 0, "A"));
        assert!(m.merge(&t));
        assert!(!m.merge(&t));
    }

    #[test]
    fn counter_and_device_break_ties() {
        let m = merged(&[
            set(vec![k("x")], json!("a"), h(10, 0, "A")),
            set(vec![k("x")], json!("b"), h(10, 1, "A")),
        ]);
        assert_eq!(m.visible()[&vec![k("x")]], json!("b"));
        let m = merged(&[
            set(vec![k("x")], json!("a"), h(10, 0, "A")),
            set(vec![k("x")], json!("b"), h(10, 0, "B")),
        ]);
        assert_eq!(m.visible()[&vec![k("x")]], json!("b"));
    }

    #[test]
    fn identical_hlc_with_different_content_still_converges() {
        // 不该发生，但一旦发生（bug / 损坏）也不能让两台设备各执一词
        let a = set(vec![k("x")], json!("a"), h(10, 0, "A"));
        let b = set(vec![k("x")], json!("b"), h(10, 0, "A"));
        let r = rm(vec![k("x")], h(10, 0, "A"));
        let m1 = merged(&[a.clone(), b.clone(), r.clone()]);
        let m2 = merged(&[r, b, a]);
        assert_eq!(m1, m2);
        assert!(
            m1.leaf(&[k("x")]).unwrap().value.is_some(),
            "Some 压过 None"
        );
    }

    #[test]
    fn remove_then_older_set_stays_removed_and_newer_set_revives_leaf() {
        let m = merged(&[
            set(vec![k("x")], json!(1), h(10, 0, "A")),
            rm(vec![k("x")], h(20, 0, "B")),
            set(vec![k("x")], json!(2), h(15, 0, "C")),
        ]);
        assert!(m.visible().is_empty());
        let m = merged(&[
            set(vec![k("x")], json!(1), h(10, 0, "A")),
            rm(vec![k("x")], h(20, 0, "B")),
            set(vec![k("x")], json!(3), h(30, 0, "C")),
        ]);
        assert_eq!(m.visible()[&vec![k("x")]], json!(3));
    }

    #[test]
    fn set_null_is_a_real_value_distinct_from_removed() {
        let m = merged(&[set(vec![k("x")], json!(null), h(10, 0, "A"))]);
        assert_eq!(m.visible()[&vec![k("x")]], json!(null));
        let m = merged(&[
            set(vec![k("x")], json!(null), h(10, 0, "A")),
            rm(vec![k("x")], h(11, 0, "A")),
        ]);
        assert!(m.visible().is_empty());
    }

    // ---------------- 墓碑与复活 ----------------

    fn element_ops(e: &str, ms: u64, dev: &str) -> Vec<Op> {
        vec![
            set(leaf("l", e, "id"), json!(e), h(ms, 0, dev)),
            set(
                leaf("l", e, "name"),
                json!(format!("name-{e}")),
                h(ms, 0, dev),
            ),
            set(leaf("l", e, "color"), json!("red"), h(ms, 0, dev)),
        ]
    }

    #[test]
    fn tombstone_hides_whole_element() {
        let mut ops = element_ops("e1", 10, "A");
        ops.push(tomb(el("l", "e1"), h(20, 0, "A")));
        let m = merged(&ops);
        assert!(m.visible().is_empty());
    }

    #[test]
    fn older_tombstone_does_not_hide_newer_element() {
        let mut ops = vec![tomb(el("l", "e1"), h(5, 0, "A"))];
        ops.extend(element_ops("e1", 10, "A"));
        let m = merged(&ops);
        assert_eq!(m.visible().len(), 3);
    }

    #[test]
    fn edit_after_tombstone_revives_whole_element_not_just_the_field() {
        let mut ops = element_ops("e1", 10, "A");
        ops.push(tomb(el("l", "e1"), h(20, 0, "A")));
        // B 没看到删除，之后（hlc 更大）只改了 name
        ops.push(set(
            leaf("l", "e1", "name"),
            json!("renamed"),
            h(25, 0, "B"),
        ));
        let v = merged(&ops).visible();
        assert_eq!(v.len(), 3, "整个元素复活：{v:#?}");
        assert_eq!(v[&leaf("l", "e1", "name")], json!("renamed"));
        assert_eq!(v[&leaf("l", "e1", "color")], json!("red"));
        assert_eq!(v[&leaf("l", "e1", "id")], json!("e1"));
    }

    #[test]
    fn edit_before_tombstone_loses_to_it() {
        let mut ops = element_ops("e1", 10, "A");
        ops.push(set(
            leaf("l", "e1", "name"),
            json!("renamed"),
            h(15, 0, "B"),
        ));
        ops.push(tomb(el("l", "e1"), h(20, 0, "A")));
        assert!(merged(&ops).visible().is_empty());
    }

    #[test]
    fn removing_a_field_after_the_tombstone_also_counts_as_an_edit() {
        // 墓碑之后别人对元素做的是「删字段」：与「写字段」同样算改动，元素复活（只是少了那个字段）
        let mut ops = element_ops("e1", 10, "A");
        ops.push(tomb(el("l", "e1"), h(20, 0, "A")));
        ops.push(rm(leaf("l", "e1", "color"), h(25, 0, "B")));
        let v = merged(&ops).visible();
        assert_eq!(v.len(), 2, "{v:#?}");
        assert!(!v.contains_key(&leaf("l", "e1", "color")));
    }

    #[test]
    fn local_removal_of_the_only_newer_leaf_must_not_silently_kill_a_revived_element() {
        // 回归（随机多设备测试发现）：C 删除 → B 之后改了 cfg → A 看到后又把 cfg 删掉。
        // A 的这次删字段合并进镜像时，元素必须仍然活着，否则本机文档与镜像就分叉了。
        let mut ops = element_ops("e1", 10, "A");
        ops.push(tomb(el("l", "e1"), h(20, 0, "C")));
        ops.push(set(leaf("l", "e1", "cfg"), json!(1), h(30, 0, "B")));
        ops.push(rm(leaf("l", "e1", "cfg"), h(40, 0, "A")));
        let v = merged(&ops).visible();
        assert!(v.contains_key(&leaf("l", "e1", "name")), "{v:#?}");
    }

    #[test]
    fn nested_elements_are_judged_level_by_level() {
        let inner = |s: &str| vec![k("l"), id("e1"), k("sub"), id(s)];
        let mut ops = vec![
            set(leaf("l", "e1", "id"), json!("e1"), h(10, 0, "A")),
            set(
                [inner("s1"), vec![k("g")]].concat(),
                json!(1),
                h(10, 0, "A"),
            ),
            set(
                [inner("s2"), vec![k("g")]].concat(),
                json!(2),
                h(10, 0, "A"),
            ),
        ];
        // 只删内层 s1
        ops.push(tomb(inner("s1"), h(20, 0, "A")));
        let v = merged(&ops).visible();
        assert!(!v.contains_key(&[inner("s1"), vec![k("g")]].concat()));
        assert!(v.contains_key(&[inner("s2"), vec![k("g")]].concat()));
        assert!(v.contains_key(&leaf("l", "e1", "id")));
        // 再删外层：内层 s2 虽然自己没墓碑，也跟着隐去
        ops.push(tomb(el("l", "e1"), h(30, 0, "A")));
        assert!(merged(&ops).visible().is_empty());
        // 外层之后又有人改了内层 s2 → 外层复活，s2 随之可见，s1 仍被它自己的墓碑压着
        ops.push(set(
            [inner("s2"), vec![k("g")]].concat(),
            json!(22),
            h(40, 0, "B"),
        ));
        let v = merged(&ops).visible();
        assert_eq!(v[&[inner("s2"), vec![k("g")]].concat()], json!(22));
        assert!(v.contains_key(&leaf("l", "e1", "id")));
        assert!(!v.contains_key(&[inner("s1"), vec![k("g")]].concat()));
    }

    #[test]
    fn order_leaf_is_not_under_any_element_and_survives_tombstones() {
        let ops = vec![
            set(vec![k("l"), Seg::ORDER], json!(["e1"]), h(10, 0, "A")),
            set(leaf("l", "e1", "id"), json!("e1"), h(10, 0, "A")),
            tomb(el("l", "e1"), h(20, 0, "A")),
        ];
        let v = merged(&ops).visible();
        assert_eq!(v.len(), 1);
        assert!(v.contains_key(&vec![k("l"), Seg::ORDER]));
    }

    #[test]
    fn element_first_hlc_is_min_of_some_leaves() {
        let m = merged(&[
            set(leaf("l", "e1", "a"), json!(1), h(30, 0, "A")),
            set(leaf("l", "e1", "b"), json!(1), h(10, 0, "A")),
            rm(leaf("l", "e1", "c"), h(5, 0, "A")), // 已删除的不算
            set(leaf("l", "e2", "a"), json!(1), h(1, 0, "A")),
        ]);
        assert_eq!(m.element_first_hlc(&el("l", "e1")), Some(&h(10, 0, "A")));
        assert_eq!(m.element_first_hlc(&el("l", "e3")), None);
    }

    // ---------------- 叶子与后代冲突 ----------------

    #[test]
    fn leaf_vs_descendant_conflict_newer_side_wins() {
        // 字段 a 原来是字符串，后来变成对象 {z}：叶子 a 更旧 → 后代赢
        let m = merged(&[
            set(vec![k("a")], json!("str"), h(10, 0, "A")),
            set(vec![k("a"), k("z")], json!(1), h(20, 0, "B")),
        ]);
        let v = m.visible();
        assert_eq!(v.len(), 1);
        assert!(v.contains_key(&vec![k("a"), k("z")]));
        // 反过来：叶子更新 → 叶子赢，后代隐去
        let m = merged(&[
            set(vec![k("a"), k("z")], json!(1), h(10, 0, "B")),
            set(vec![k("a")], json!("str"), h(20, 0, "A")),
        ]);
        let v = m.visible();
        assert_eq!(v.len(), 1);
        assert!(v.contains_key(&vec![k("a")]));
    }

    #[test]
    fn leaf_vs_descendant_chain_is_consistent() {
        let m = merged(&[
            set(vec![k("a")], json!(1), h(5, 0, "A")),
            set(vec![k("a"), k("b")], json!(2), h(10, 0, "A")),
            set(vec![k("a"), k("b"), k("c")], json!(3), h(7, 0, "A")),
        ]);
        let v = m.visible();
        assert_eq!(v.len(), 1);
        assert!(v.contains_key(&vec![k("a"), k("b")]));
    }

    #[test]
    fn registered_array_leaf_is_shadowed_by_id_children() {
        // 原来是原子 []，后来成了 id 映射：旧的原子叶子被新的子树压住
        let m = merged(&[
            set(vec![k("tags")], json!([]), h(5, 0, "A")),
            set(
                vec![k("tags"), id("t1"), k("id")],
                json!("t1"),
                h(10, 0, "A"),
            ),
            set(vec![k("tags"), Seg::ORDER], json!(["t1"]), h(10, 0, "A")),
        ]);
        let v = m.visible();
        assert!(!v.contains_key(&vec![k("tags")]));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn shadowed_stale_leaf_stays_hidden_after_its_descendants_die() {
        // 旧的原子叶子被 id 子树压住后，子树里的元素即使全被删光，旧叶子也不能"诈尸"
        let m = merged(&[
            set(vec![k("tags")], json!(null), h(5, 0, "A")),
            set(
                vec![k("tags"), id("t1"), k("id")],
                json!("t1"),
                h(10, 0, "B"),
            ),
            tomb(vec![k("tags"), id("t1")], h(20, 0, "B")),
        ]);
        assert!(m.visible().is_empty(), "{:?}", m.visible());
        // 旧叶子自己被重新写入（hlc 更大）才会回来；此时（子树已死）它是唯一可见的
        let mut m2 = m.clone();
        m2.merge(&set(vec![k("tags")], json!([]), h(30, 0, "A")));
        let v = m2.visible();
        assert_eq!(v.len(), 1);
        assert_eq!(v[&vec![k("tags")]], json!([]));
    }

    #[test]
    fn leaf_newer_than_all_descendants_hides_them_even_when_they_are_dead_elements() {
        let m = merged(&[
            set(vec![k("a"), id("x"), k("v")], json!(1), h(5, 0, "A")),
            tomb(vec![k("a"), id("x")], h(6, 0, "A")),
            set(vec![k("a")], json!("scalar"), h(10, 0, "B")),
        ]);
        let v = m.visible();
        assert_eq!(v.len(), 1);
        assert!(v.contains_key(&vec![k("a")]));
    }

    #[test]
    fn visible_never_has_a_leaf_that_is_a_prefix_of_another() {
        for seed in 0..100 {
            let ops = random_ops(&mut Rng::new(seed), 40);
            let v = merged(&ops).visible();
            let paths: Vec<&Path> = v.keys().collect();
            for (i, p) in paths.iter().enumerate() {
                for q in &paths[i + 1..] {
                    assert!(
                        !q.starts_with(p),
                        "seed {seed}: {p:?} 与后代 {q:?} 同时可见"
                    );
                }
            }
        }
    }

    // ---------------- affected ----------------

    #[test]
    fn affected_reports_changed_added_removed() {
        let mut before = BTreeMap::new();
        before.insert(vec![k("a")], json!(1));
        before.insert(vec![k("b")], json!(2));
        before.insert(vec![k("c")], json!(3));
        let mut after = BTreeMap::new();
        after.insert(vec![k("a")], json!(1)); // 不变
        after.insert(vec![k("b")], json!(20)); // 变了
        after.insert(vec![k("d")], json!(4)); // 新出现
                                              // c 消失
        assert_eq!(
            affected(&before, &after),
            vec![vec![k("b")], vec![k("c")], vec![k("d")]]
        );
        assert!(affected(&before, &before).is_empty());
        assert_eq!(affected(&BTreeMap::new(), &after).len(), 3);
    }

    #[test]
    fn affected_after_tombstone_lists_every_hidden_leaf() {
        let mut m = merged(&element_ops("e1", 10, "A"));
        let before = m.visible();
        m.merge(&tomb(el("l", "e1"), h(20, 0, "B")));
        let a = affected(&before, &m.visible());
        assert_eq!(a.len(), 3);
        // 复活时同样整元素都算受影响
        let before = m.visible();
        m.merge(&set(leaf("l", "e1", "name"), json!("n2"), h(30, 0, "B")));
        let a = affected(&before, &m.visible());
        assert_eq!(a.len(), 3);
    }

    // ---------------- 序列化 ----------------

    #[test]
    fn mirror_json_round_trip_keeps_removed_and_null_distinct() {
        let m = merged(&[
            set(vec![k("a")], json!(null), h(10, 0, "A")),
            set(vec![k("b")], json!({"x": [1, 2]}), h(11, 0, "A")),
            rm(vec![k("c")], h(12, 1, "B")),
            set(leaf("l", "e", "f"), json!("v"), h(13, 0, "A")),
            tomb(el("l", "dead"), h(14, 0, "B")),
            set(vec![k("l"), Seg::ORDER], json!(["e"]), h(15, 0, "A")),
        ]);
        let s = serde_json::to_string(&m).unwrap();
        let back: Mirror = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.leaf(&[k("a")]).unwrap().value, Some(json!(null)));
        assert_eq!(back.leaf(&[k("c")]).unwrap().value, None);
    }

    #[test]
    fn mirror_deserialize_tolerates_missing_sections_and_unknown_fields() {
        let back: Mirror = serde_json::from_str(r#"{"extra":1}"#).unwrap();
        assert!(back.is_empty());
        let back: Mirror = serde_json::from_str(
            r#"{"leaves":[{"p":["a"],"h":{"ms":1,"counter":0,"device":"A"},"v":5,"future":true}]}"#,
        )
        .unwrap();
        assert_eq!(back.visible()[&vec![k("a")]], json!(5));
    }

    #[test]
    fn op_json_round_trip_all_kinds() {
        let ops = vec![
            set(vec![k("a"), id("x"), k("b")], json!(null), h(1, 0, "A")),
            set(vec![k("a")], json!({"o": [1]}), h(1, 1, "A")),
            rm(vec![k("a")], h(2, 0, "A")),
            tomb(el("l", "e"), h(3, 0, "A")),
        ];
        let s = serde_json::to_string(&ops).unwrap();
        let back: Vec<Op> = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ops);
        assert!(s.contains(r#""kind":{"t":"remove"}"#), "{s}");
    }

    // ---------------- 收敛性 ----------------

    /// 在一个小而拥挤的路径宇宙里随机生成 op：故意让路径、hlc 大量撞车，
    /// 包含嵌套元素、顺序叶子、墓碑、删除、叶子与后代冲突，以及 hlc 完全相同但内容不同的情形。
    fn random_ops(rng: &mut Rng, n: usize) -> Vec<Op> {
        let devs = ["A", "B", "C"];
        let elems = ["e1", "e2", "e3"];
        let subs = ["s1", "s2"];
        let mut ops = Vec::new();
        for _ in 0..n {
            let dev = *rng.pick(&devs);
            let hlc = h(1 + rng.below(8) as u64, rng.below(2) as u32, dev);
            let e = *rng.pick(&elems);
            let s = *rng.pick(&subs);
            let (path, tombable): (Path, bool) = match rng.below(9) {
                0 => (vec![k("a")], false),
                1 => (vec![k("a"), k("z")], false), // 与上一条构成叶子/后代冲突
                2 => (vec![k("b"), k("c")], false),
                3 => (leaf("l", e, "f1"), false),
                4 => (leaf("l", e, "f2"), false),
                5 => (vec![k("l"), id(e), k("sub"), id(s), k("g")], false),
                6 => (vec![k("l"), Seg::ORDER], false),
                7 => (vec![k("l"), id(e), k("sub"), Seg::ORDER], false),
                _ => {
                    if rng.chance(50) {
                        (el("l", e), true)
                    } else {
                        (vec![k("l"), id(e), k("sub"), id(s)], true)
                    }
                }
            };
            let kind = if tombable {
                OpKind::Tombstone
            } else if rng.chance(25) {
                OpKind::Remove
            } else {
                OpKind::Set(json!(rng.below(4)))
            };
            ops.push(Op {
                source: "t".into(),
                path,
                kind,
                hlc,
            });
        }
        ops
    }

    #[test]
    fn convergence_any_order_any_duplication() {
        for seed in 0..200u64 {
            let mut rng = Rng::new(seed);
            let n = 5 + rng.below(40);
            let ops = random_ops(&mut rng, n);

            // 三份副本：不同顺序、含重复
            let mut replicas: Vec<Mirror> = Vec::new();
            for _ in 0..3 {
                let mut o = ops.clone();
                let extra: Vec<Op> = o.iter().filter(|_| rng.chance(30)).cloned().collect();
                o.extend(extra);
                rng.shuffle(&mut o);
                replicas.push(merged(&o));
            }
            assert_eq!(replicas[0], replicas[1], "seed {seed}: 镜像状态不一致");
            assert_eq!(replicas[1], replicas[2], "seed {seed}: 镜像状态不一致");
            let v0 = replicas[0].visible();
            assert_eq!(v0, replicas[1].visible(), "seed {seed}: 可见集不一致");
            assert_eq!(v0, replicas[2].visible(), "seed {seed}: 可见集不一致");

            // 再合并一遍同一批 op：什么都不该变
            let mut again = replicas[0].clone();
            for op in &ops {
                assert!(!again.merge(op), "seed {seed}: 重复合并改变了镜像");
            }
            assert_eq!(again, replicas[0]);
        }
    }

    #[test]
    fn convergence_via_mirror_merges_matches_op_merges() {
        for seed in 0..200u64 {
            let mut rng = Rng::new(1000 + seed);
            let n = 6 + rng.below(40);
            let ops = random_ops(&mut rng, n);
            let reference = merged(&ops);

            // 把 op 随机切给三台设备，各自只合并自己那份，再以不同顺序互相合并整份镜像
            let mut parts: Vec<Vec<Op>> = vec![Vec::new(), Vec::new(), Vec::new()];
            for op in &ops {
                parts[rng.below(3)].push(op.clone());
            }
            let ms: Vec<Mirror> = parts.iter().map(|p| merged(p)).collect();

            let mut x = ms[0].clone();
            x.merge_mirror(&ms[1]);
            x.merge_mirror(&ms[2]);
            let mut y = ms[2].clone();
            y.merge_mirror(&ms[0]);
            y.merge_mirror(&ms[1]);
            let mut yz = ms[1].clone();
            yz.merge_mirror(&ms[2]);
            let mut z = ms[0].clone();
            z.merge_mirror(&yz); // 结合律：a ∪ (b ∪ c)
            z.merge_mirror(&ms[0]); // 幂等

            assert_eq!(x, reference, "seed {seed}");
            assert_eq!(y, reference, "seed {seed}");
            assert_eq!(z, reference, "seed {seed}");
        }
    }

    #[test]
    fn convergence_survives_serialization_round_trips() {
        for seed in 0..50u64 {
            let mut rng = Rng::new(5000 + seed);
            let ops = random_ops(&mut rng, 30);
            let m = merged(&ops);
            let back: Mirror = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
            assert_eq!(back, m, "seed {seed}");
            assert_eq!(back.visible(), m.visible());
        }
    }

    // ---------------- ingest ----------------

    #[test]
    fn ingest_remote_advances_clock_learns_idmaps_and_reports_affected() {
        let mut m = Mirror::new();
        let mut reg = IdMapRegistry::new();
        let mut clock = HlcClock::new("ME");
        let ops = vec![
            set(leaf("l", "e1", "id"), json!("e1"), h(9000, 2, "OTHER")),
            set(vec![k("x")], json!(1), h(9000, 1, "OTHER")),
        ];
        let aff = ingest_remote(&mut m, &ops, &mut reg, &mut clock, 1000);
        assert_eq!(aff.len(), 2);
        assert!(reg.contains_array(&[k("l")]));
        // 之后本机发出的 hlc 必须大于见过的所有远端 hlc
        let mine = clock.now(1000);
        assert!(mine > h(9000, 2, "OTHER"));
        // 重复投递：没有新的受影响路径
        let aff2 = ingest_remote(&mut m, &ops, &mut reg, &mut clock, 1000);
        assert!(aff2.is_empty());
    }

    #[test]
    fn ingest_remote_mirror_equivalent_to_ops() {
        let ops = element_ops("e1", 10, "A");
        let remote = merged(&ops);
        let mut local = Mirror::new();
        let mut reg = IdMapRegistry::new();
        let mut clock = HlcClock::new("ME");
        let aff = ingest_remote_mirror(&mut local, &remote, &mut reg, &mut clock, 0);
        assert_eq!(aff.len(), 3);
        assert_eq!(local, remote);
        assert!(reg.contains_array(&[k("l")]));
        assert!(clock.now(0) > h(10, 0, "A"));
    }

    #[test]
    fn max_hlc_covers_tombstones_and_removed_leaves() {
        let m = merged(&[
            set(vec![k("a")], json!(1), h(10, 0, "A")),
            rm(vec![k("b")], h(30, 0, "A")),
            tomb(el("l", "e"), h(20, 0, "A")),
        ]);
        assert_eq!(m.max_hlc(), Some(h(30, 0, "A")));
        assert_eq!(Mirror::new().max_hlc(), None);
    }
}
