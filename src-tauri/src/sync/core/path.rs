//! 叶子路径与「id 映射登记表」。
//!
//! 同步的最小单位是 JSON 里的**叶子**（标量 / 原子数组），路径是从文档根到叶子的一串 [`Seg`]。
//! 数组的处理是整个模型里最微妙的一处：连接列表这种「元素带 `id`」的数组，如果按下标当路径，
//! 两台设备各自在中间插一条就全乱了；所以这类数组按 id 当 key 展开（`Seg::Id`），顺序另存为一个
//! 特殊叶子（`Seg::Order`）。哪些数组按 id 映射由 [`IdMapRegistry`] 登记——**只增不减**，一旦登记过，
//! 即便某天它变成空数组、或被某台设备清空，也仍然按 id 映射，不会在「原子数组」与「id 映射」
//! 之间来回横跳（那会让镜像里的同一路径同时存在叶子和子树）。
//!
//! 约定：
//! - `Seg::Key` 保存的是**线上形式**的 key：真实 JSON key 若以 `@` 开头，前面多补一个 `@`
//!   （`@x` → `@@x`），因为以单个 `@` 开头的 key 保留给平台前缀（`@desktop` / `@android`）。
//!   [`escape_key`] / [`unescape_key`] 是唯一的互转入口，别在别处手拼。
//! - 平台前缀只出现在**镜像 / op 的路径**最前面，不出现在本机文档里。

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 路径的一段。序列化形式：字符串 / `{"id":".."}` / `{"order":null}`。
///
/// 变体声明顺序决定排序（Key < Id < Order），不要调——镜像里所有「某前缀下的子树是连续一段」
/// 的范围查询都建立在这个全序上。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Seg {
    Key(String),
    Id { id: String },
    Order { order: () },
}

impl Seg {
    /// 顺序叶子段。
    pub const ORDER: Seg = Seg::Order { order: () };

    pub fn key(k: impl Into<String>) -> Seg {
        Seg::Key(k.into())
    }

    pub fn id(id: impl Into<String>) -> Seg {
        Seg::Id { id: id.into() }
    }

    pub fn is_id(&self) -> bool {
        matches!(self, Seg::Id { .. })
    }

    pub fn is_order(&self) -> bool {
        matches!(self, Seg::Order { .. })
    }
}

pub type Path = Vec<Seg>;

/// 真实 JSON key → 线上 key（见模块注释）。
pub fn escape_key(k: &str) -> String {
    if k.starts_with('@') {
        format!("@{k}")
    } else {
        k.to_string()
    }
}

/// 线上 key → 真实 JSON key。
pub fn unescape_key(k: &str) -> &str {
    if k.starts_with("@@") {
        &k[1..]
    } else {
        k
    }
}

/// 运行平台。同步里只有两种 UI 形态的设备；平台作用域的叶子按它加前缀。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    Desktop,
    Android,
}

impl Platform {
    pub fn current() -> Platform {
        if cfg!(target_os = "android") {
            Platform::Android
        } else {
            Platform::Desktop
        }
    }

    /// 路径前缀 key，如 `@desktop`。
    pub fn tag(self) -> &'static str {
        match self {
            Platform::Desktop => "@desktop",
            Platform::Android => "@android",
        }
    }

    /// 写进远端设备信息里的名字（`"desktop"` / `"android"`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Desktop => "desktop",
            Platform::Android => "android",
        }
    }

    pub fn prefix_seg(self) -> Seg {
        Seg::Key(self.tag().to_string())
    }
}

/// 路径最前面平台前缀的情况。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixKind {
    /// 没有平台前缀（共享叶子）。
    None,
    /// 是本平台的前缀。
    Own,
    /// 别的平台的前缀（含将来新增、本版本不认识的平台）。
    Other,
}

/// 拆出路径开头的平台前缀。以单个 `@` 开头的 key 一律视为平台前缀：
/// 将来加了 `@ios`，老版本也能认出「这不是我的」而不是把它当普通 key 写进文档。
pub fn split_platform_prefix(path: &[Seg], own: Platform) -> (PrefixKind, &[Seg]) {
    match path.first() {
        Some(Seg::Key(k)) if k.starts_with('@') && !k.starts_with("@@") => {
            let kind = if k == own.tag() {
                PrefixKind::Own
            } else {
                PrefixKind::Other
            };
            (kind, &path[1..])
        }
        _ => (PrefixKind::None, path),
    }
}

/// 去掉平台前缀（不论哪个平台）。
pub fn strip_any_platform_prefix(path: &[Seg]) -> &[Seg] {
    match path.first() {
        Some(Seg::Key(k)) if k.starts_with('@') && !k.starts_with("@@") => &path[1..],
        _ => path,
    }
}

/// 把路径里的 id 换成 `*`（给人看 / 给「新字段守护」快照比对用）。顺序叶子显示为 `#order`。
/// 平台前缀保留（`@desktop/...`）。
pub fn path_pattern_string(path: &[Seg]) -> String {
    path.iter()
        .map(|s| match s {
            Seg::Key(k) => unescape_key_for_display(k),
            Seg::Id { .. } => "*".to_string(),
            Seg::Order { .. } => "#order".to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// 带具体 id 的人类可读路径（同步记录里用）：`connections/[abc]/name`。
pub fn path_display_string(path: &[Seg]) -> String {
    path.iter()
        .map(|s| match s {
            Seg::Key(k) => unescape_key_for_display(k),
            Seg::Id { id } => format!("[{id}]"),
            Seg::Order { .. } => "#order".to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn unescape_key_for_display(k: &str) -> String {
    // 平台前缀（单 @）原样显示；转义过的 key 还原
    unescape_key(k).to_string()
}

/// 路径模式：把所有 `Id` 段的 id 清空。
fn canonical_pattern(path: &[Seg]) -> Path {
    path.iter()
        .map(|s| match s {
            Seg::Id { .. } => Seg::Id { id: String::new() },
            other => other.clone(),
        })
        .collect()
}

/// 「哪些数组按 id 映射」的登记表（每个同步源一份，只增不减，持久化在 `state.json`）。
///
/// 存的是**路径模式**——数组所在位置，其中的 id 位置统一记成空 id（通配）。
/// `connections` → `[Key(connections)]`；某连接下的 `tags` 数组 → `[Key(connections), Id(""), Key(tags)]`。
///
/// 序列化为模式列表（透明）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdMapRegistry {
    patterns: BTreeSet<Path>,
}

impl IdMapRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn patterns(&self) -> impl Iterator<Item = &Path> {
        self.patterns.iter()
    }

    /// 位于 `array_path` 的数组是否已登记为 id 映射。
    pub fn contains_array(&self, array_path: &[Seg]) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        self.patterns.contains(&canonical_pattern(array_path))
    }

    /// 登记；返回是否是新登记。
    pub fn register_array(&mut self, array_path: &[Seg]) -> bool {
        self.patterns.insert(canonical_pattern(array_path))
    }

    /// 从一条远端路径学习：路径里每个 `Id` 段之前的前缀都是 id 映射数组的位置；
    /// 以 `Order` 结尾时，它前面的前缀也是。路径开头的平台前缀不属于文档结构，先剥掉。
    /// 返回是否有新登记。
    pub fn learn_path(&mut self, path: &[Seg]) -> bool {
        let path = strip_any_platform_prefix(path);
        let mut grew = false;
        for (i, seg) in path.iter().enumerate() {
            if seg.is_id() || (seg.is_order() && i + 1 == path.len()) {
                grew |= self.register_array(&path[..i]);
            }
        }
        grew
    }

    /// 并入另一份登记表（快照里带来的）。返回是否有新登记。
    pub fn merge_from(&mut self, other: &IdMapRegistry) -> bool {
        let before = self.patterns.len();
        self.patterns.extend(other.patterns.iter().cloned());
        self.patterns.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn seg_serialization_forms() {
        assert_eq!(serde_json::to_value(Seg::key("a")).unwrap(), json!("a"));
        assert_eq!(
            serde_json::to_value(Seg::id("x1")).unwrap(),
            json!({"id": "x1"})
        );
        assert_eq!(
            serde_json::to_value(Seg::ORDER).unwrap(),
            json!({"order": null})
        );
    }

    #[test]
    fn seg_round_trip_all_forms() {
        let path: Path = vec![
            Seg::key("connections"),
            Seg::id("abc"),
            Seg::key("tags"),
            Seg::ORDER,
        ];
        let s = serde_json::to_string(&path).unwrap();
        assert_eq!(s, r#"["connections",{"id":"abc"},"tags",{"order":null}]"#);
        let back: Path = serde_json::from_str(&s).unwrap();
        assert_eq!(back, path);
        // 空串 key / 空串 id 也要分得清
        let tricky: Path = vec![Seg::key(""), Seg::id("")];
        let back: Path = serde_json::from_str(&serde_json::to_string(&tricky).unwrap()).unwrap();
        assert_eq!(back, tricky);
    }

    #[test]
    fn seg_order_total_order_key_id_order() {
        assert!(Seg::key("z") < Seg::id("a"));
        assert!(Seg::id("z") < Seg::ORDER);
        assert!(Seg::key("a") < Seg::key("b"));
    }

    #[test]
    fn subtree_is_contiguous_in_btree_order() {
        // 镜像里的范围查询依赖：某前缀的所有后代在排序里紧挨着
        let mut set: BTreeSet<Path> = BTreeSet::new();
        let e = vec![Seg::key("l"), Seg::id("e1")];
        set.insert(e.clone());
        set.insert([e.clone(), vec![Seg::key("a")]].concat());
        set.insert([e.clone(), vec![Seg::key("a"), Seg::key("b")]].concat());
        set.insert(vec![Seg::key("l"), Seg::id("e2"), Seg::key("a")]);
        set.insert(vec![Seg::key("l"), Seg::ORDER]);
        set.insert(vec![Seg::key("k")]);
        let flags: Vec<bool> = set.iter().map(|p| p.starts_with(&e)).collect();
        let first = flags.iter().position(|b| *b).unwrap();
        let last = flags.iter().rposition(|b| *b).unwrap();
        assert!(
            flags[first..=last].iter().all(|b| *b),
            "后代必须连续：{flags:?}"
        );
    }

    #[test]
    fn key_escape_round_trip() {
        assert_eq!(escape_key("name"), "name");
        assert_eq!(escape_key("@x"), "@@x");
        assert_eq!(escape_key("@@x"), "@@@x");
        for k in ["name", "@x", "@@x", "@", "", "a@b"] {
            assert_eq!(unescape_key(&escape_key(k)), k);
        }
    }

    #[test]
    fn platform_prefix_split() {
        let p = vec![Seg::key("@desktop"), Seg::key("a")];
        assert_eq!(
            split_platform_prefix(&p, Platform::Desktop),
            (PrefixKind::Own, &p[1..])
        );
        assert_eq!(
            split_platform_prefix(&p, Platform::Android),
            (PrefixKind::Other, &p[1..])
        );
        // 将来的未知平台也是「别人的」
        let q = vec![Seg::key("@ios"), Seg::key("a")];
        assert_eq!(
            split_platform_prefix(&q, Platform::Desktop).0,
            PrefixKind::Other
        );
        // 转义过的 key 不是前缀
        let r = vec![Seg::key("@@desktop"), Seg::key("a")];
        assert_eq!(
            split_platform_prefix(&r, Platform::Desktop),
            (PrefixKind::None, &r[..])
        );
        let s = vec![Seg::key("a")];
        assert_eq!(
            split_platform_prefix(&s, Platform::Desktop).0,
            PrefixKind::None
        );
        assert_eq!(
            split_platform_prefix(&[], Platform::Desktop).0,
            PrefixKind::None
        );
    }

    #[test]
    fn pattern_and_display_strings() {
        let p = vec![Seg::key("connections"), Seg::id("abc"), Seg::key("name")];
        assert_eq!(path_pattern_string(&p), "connections/*/name");
        assert_eq!(path_display_string(&p), "connections/[abc]/name");
        let o = vec![Seg::key("connections"), Seg::ORDER];
        assert_eq!(path_pattern_string(&o), "connections/#order");
    }

    #[test]
    fn registry_register_and_contains_ignores_ids() {
        let mut r = IdMapRegistry::new();
        assert!(!r.contains_array(&[Seg::key("a")]));
        assert!(r.register_array(&[Seg::key("a"), Seg::id("x"), Seg::key("tags")]));
        // 不同 id 下的同一位置是同一个模式
        assert!(r.contains_array(&[Seg::key("a"), Seg::id("y"), Seg::key("tags")]));
        assert!(!r.contains_array(&[Seg::key("a"), Seg::key("tags")]));
        assert!(!r.register_array(&[Seg::key("a"), Seg::id("zzz"), Seg::key("tags")]));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn registry_learns_from_paths() {
        let mut r = IdMapRegistry::new();
        let p = vec![
            Seg::key("list"),
            Seg::id("e1"),
            Seg::key("sub"),
            Seg::id("s1"),
            Seg::key("name"),
        ];
        assert!(r.learn_path(&p));
        assert!(r.contains_array(&[Seg::key("list")]));
        assert!(r.contains_array(&[Seg::key("list"), Seg::id("other"), Seg::key("sub")]));
        assert!(!r.learn_path(&p), "再学一次没有新登记");
        // Order 叶子 → 它前面的前缀
        let mut r2 = IdMapRegistry::new();
        assert!(r2.learn_path(&[Seg::key("servers"), Seg::ORDER]));
        assert!(r2.contains_array(&[Seg::key("servers")]));
        // 墓碑路径以 Id 结尾
        let mut r3 = IdMapRegistry::new();
        assert!(r3.learn_path(&[Seg::key("servers"), Seg::id("s")]));
        assert!(r3.contains_array(&[Seg::key("servers")]));
    }

    #[test]
    fn registry_learning_strips_platform_prefix() {
        let mut r = IdMapRegistry::new();
        r.learn_path(&[
            Seg::key("@android"),
            Seg::key("items"),
            Seg::id("a"),
            Seg::key("v"),
        ]);
        assert!(r.contains_array(&[Seg::key("items")]));
        assert!(!r.contains_array(&[Seg::key("@android"), Seg::key("items")]));
    }

    #[test]
    fn registry_serde_round_trip_and_merge() {
        let mut r = IdMapRegistry::new();
        r.register_array(&[Seg::key("a")]);
        r.register_array(&[Seg::key("b"), Seg::id("1"), Seg::key("c")]);
        let s = serde_json::to_string(&r).unwrap();
        let back: IdMapRegistry = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
        let mut m = IdMapRegistry::new();
        m.register_array(&[Seg::key("z")]);
        assert!(m.merge_from(&r));
        assert!(!m.merge_from(&r));
        assert_eq!(m.len(), 3);
    }
}
