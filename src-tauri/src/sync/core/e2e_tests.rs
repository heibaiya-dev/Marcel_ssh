//! 端到端（纯内存）：多台模拟设备各持一份文档 + 镜像 + 基线 + 登记表 + 时钟，
//! 通过一个共享的「批次日志」交换 op（模拟存储端的 seg 文件），走完
//! 捕获(diff) → 合并(ingest) → 落地(apply) 的完整闭环。
//!
//! 验证的是各模块拼在一起后的性质：
//! - 收敛：一切静默后各设备的 Shared 部分相同，Local 部分各自保留，Platform 部分只在同平台设备间一致；
//! - 本机只上报自己真改过的东西（版本兼容）；
//! - 加入同步时「云端赢、本机独有保留」。
//!
//! 随机测试用固定种子，失败信息里带种子与步骤，可原样重跑。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::apply::{affected_vs_doc, apply_to_doc};
use super::diff::{diff, export_all_at, DiffCtx};
use super::flatten::flatten_scoped;
use super::hlc::{Hlc, HlcClock};
use super::mirror::{ingest_remote, merge_remote_ops, Mirror, Op, OpKind};
use super::path::{path_display_string, IdMapRegistry, Path, Platform, Seg};
use super::scope::{Scope, ScopeRules};
use super::test_util::Rng;

const SOURCE: &str = "settings";

static RULES: ScopeRules = ScopeRules {
    patterns: &[
        ("window", Scope::Local),
        ("items/*/localNote", Scope::Local),
        ("notify", Scope::Platform),
    ],
    item_filters: &[],
};

struct Device {
    name: &'static str,
    platform: Platform,
    doc: Value,
    baseline: Value,
    mirror: Mirror,
    idmaps: IdMapRegistry,
    clock: HlcClock,
    wall: u64,
    cursor: usize,
}

struct Batch {
    origin: usize,
    ops: Vec<Op>,
}

struct World {
    devs: Vec<Device>,
    log: Vec<Batch>,
}

impl Device {
    fn new(name: &'static str, platform: Platform, doc: Value, wall: u64) -> Device {
        Device {
            name,
            platform,
            doc,
            baseline: Value::Null,
            mirror: Mirror::new(),
            idmaps: IdMapRegistry::new(),
            clock: HlcClock::new(name),
            wall,
            cursor: 0,
        }
    }

    /// 捕获本机改动：diff → 并入自己的镜像 → 基线 = 当前文档。
    fn capture(&mut self, suppress_removes: bool) -> Vec<Op> {
        let mut ctx = DiffCtx {
            rules: &RULES,
            platform: self.platform,
            idmaps: &mut self.idmaps,
            clock: &mut self.clock,
            wall_ms: self.wall,
            suppress_removes,
        };
        let ops = diff(SOURCE, &self.baseline, &self.doc, &mut ctx);
        for o in &ops {
            self.mirror.merge(o);
        }
        self.baseline = self.doc.clone();
        ops
    }

    /// 收远端批次并落地：并入镜像 → 以"文档现状 vs 镜像可见集"为受影响路径 → 落地 → 新基线。
    /// 返回受影响路径数。**调用前必须已经捕获过本机改动。**
    fn receive(&mut self, batches: &[Batch]) -> usize {
        let ops: Vec<Op> = batches.iter().flat_map(|b| b.ops.clone()).collect();
        merge_remote_ops(
            &mut self.mirror,
            &ops,
            &mut self.idmaps,
            &mut self.clock,
            self.wall,
        );
        let aff = affected_vs_doc(
            &self.doc,
            &self.mirror,
            &RULES,
            self.platform,
            &mut self.idmaps,
        );
        self.doc = apply_to_doc(&self.doc, &self.mirror, &aff, &RULES, self.platform);
        self.baseline = self.doc.clone();
        aff.len()
    }

    fn shared_view(&self) -> BTreeMap<Path, Value> {
        flatten_scoped(&self.doc, &RULES, self.platform, &mut self.idmaps.clone())
            .into_iter()
            .filter(|(p, _)| !is_platform_path(p))
            .collect()
    }

    fn platform_view(&self) -> BTreeMap<Path, Value> {
        flatten_scoped(&self.doc, &RULES, self.platform, &mut self.idmaps.clone())
            .into_iter()
            .filter(|(p, _)| is_platform_path(p))
            .collect()
    }
}

fn is_platform_path(p: &Path) -> bool {
    matches!(p.first(), Some(Seg::Key(k)) if k.starts_with('@') && !k.starts_with("@@"))
}

impl World {
    fn new(devs: Vec<Device>) -> World {
        World {
            devs,
            log: Vec::new(),
        }
    }

    fn dev(&mut self, name: &str) -> &mut Device {
        self.devs.iter_mut().find(|d| d.name == name).unwrap()
    }

    fn idx(&self, name: &str) -> usize {
        self.devs.iter().position(|d| d.name == name).unwrap()
    }

    /// 一次同步：捕获 → 上传批次 → 拉取至多 `max` 个别人的批次并落地。
    fn sync_n(&mut self, i: usize, max: usize) {
        // 运行时一轮的标准顺序：先捕获本机改动 → 并入远端 → 文档现状 vs 镜像 → 落地。
        let ops = self.devs[i].capture(false);
        if !ops.is_empty() {
            self.log.push(Batch { origin: i, ops });
        }
        let cursor = self.devs[i].cursor;
        let end = cursor.saturating_add(max).min(self.log.len());
        let incoming: Vec<Batch> = self.log[cursor..end]
            .iter()
            .filter(|b| b.origin != i)
            .map(|b| Batch {
                origin: b.origin,
                ops: b.ops.clone(),
            })
            .collect();
        self.devs[i].cursor = end;
        self.devs[i].receive(&incoming);
    }

    fn sync(&mut self, name: &str) {
        let i = self.idx(name);
        self.sync_n(i, usize::MAX);
    }

    /// 一切静默：所有设备反复全量同步，直到没有新批次。
    fn quiesce(&mut self) {
        for _ in 0..4 {
            for i in 0..self.devs.len() {
                self.sync_n(i, usize::MAX);
            }
        }
    }

    /// 校验静默后的收敛性。失败信息只列出不一致的叶子（各设备的值 + 镜像里该叶子的状态），
    /// 而不是把两整份文档倒出来。
    fn assert_converged(&self, ctx: &str) {
        let first = &self.devs[0];
        for d in &self.devs[1..] {
            let (a, b) = (first.shared_view(), d.shared_view());
            if a != b {
                let mut report = String::new();
                let keys: std::collections::BTreeSet<&Path> = a.keys().chain(b.keys()).collect();
                for k in keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).take(6) {
                    report.push_str(&format!(
                        "
  {}: {}={:?} {}={:?}
    镜像叶子={:?}",
                        path_display_string(k),
                        first.name,
                        a.get(k),
                        d.name,
                        b.get(k),
                        first.mirror.leaf(k)
                    ));
                }
                panic!(
                    "{ctx}: {} 与 {} 的 Shared 部分不一致{report}",
                    first.name, d.name
                );
            }
            assert_eq!(first.mirror, d.mirror, "{ctx}: 镜像状态不一致");
        }
        // Platform 部分：同平台设备之间一致
        for a in &self.devs {
            for b in &self.devs {
                if a.platform == b.platform {
                    assert_eq!(
                        a.platform_view(),
                        b.platform_view(),
                        "{ctx}: 同平台设备的 Platform 部分不一致"
                    );
                }
            }
        }
    }
}

fn base_doc() -> Value {
    json!({
        "theme": "dark",
        "prefs": {"font": 12, "nested": {"a": "x"}},
        "window": {"w": 100},
        "notify": {"sound": true},
        "items": [
            {"id": "i1", "name": "one", "cfg": {"k": 1}, "list": [1, 2], "tags": [], "localNote": "n1"},
            {"id": "i2", "name": "two", "cfg": {"k": 2}, "list": [], "tags": [], "localNote": "n2"},
        ],
    })
}

fn pair() -> World {
    let mut w = World::new(vec![
        Device::new("A", Platform::Desktop, base_doc(), 1_000_000),
        Device::new("B", Platform::Desktop, base_doc(), 1_000_000),
    ]);
    // 两边起始数据相同：各自首次捕获 = 全量导出
    w.quiesce();
    w
}

fn item_ids(d: &Device) -> Vec<String> {
    d.doc["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| e["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn find_item<'a>(d: &'a mut Device, id: &str) -> &'a mut Value {
    d.doc["items"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|e| e["id"] == id)
        .unwrap()
}

// ---------------------------------------------------------------------------
// 脚本化场景
// ---------------------------------------------------------------------------

#[test]
fn identical_starting_data_produces_no_visible_change() {
    let w = pair();
    assert_eq!(w.devs[0].doc, base_doc());
    assert_eq!(w.devs[1].doc, base_doc());
    w.assert_converged("起始");
}

#[test]
fn a_changes_b_applies_b_changes_a_applies() {
    let mut w = pair();
    w.dev("A").wall += 10;
    w.dev("A").doc["theme"] = json!("light");
    w.dev("A").doc["prefs"]["font"] = json!(14);
    find_item(w.dev("A"), "i1")["name"] = json!("one-A");
    w.sync("A");
    w.sync("B");
    assert_eq!(w.dev("B").doc["theme"], json!("light"));
    assert_eq!(w.dev("B").doc["prefs"]["font"], json!(14));
    assert_eq!(find_item(w.dev("B"), "i1")["name"], json!("one-A"));

    w.dev("B").wall += 10;
    w.dev("B").doc["prefs"]["nested"]["a"] = json!("from-B");
    find_item(w.dev("B"), "i2")["cfg"]["k"] = json!(22);
    w.sync("B");
    w.sync("A");
    assert_eq!(w.dev("A").doc["prefs"]["nested"]["a"], json!("from-B"));
    assert_eq!(find_item(w.dev("A"), "i2")["cfg"]["k"], json!(22));
    w.assert_converged("A↔B");
}

#[test]
fn local_fields_stay_local_on_both_sides() {
    let mut w = pair();
    w.dev("A").wall += 10;
    w.dev("A").doc["window"]["w"] = json!(111);
    find_item(w.dev("A"), "i1")["localNote"] = json!("A-private");
    w.dev("B").wall += 10;
    w.dev("B").doc["window"]["w"] = json!(222);
    find_item(w.dev("B"), "i1")["localNote"] = json!("B-private");
    w.dev("B").doc["theme"] = json!("from-B");
    w.quiesce();
    assert_eq!(w.dev("A").doc["window"]["w"], json!(111));
    assert_eq!(w.dev("B").doc["window"]["w"], json!(222));
    assert_eq!(find_item(w.dev("A"), "i1")["localNote"], json!("A-private"));
    assert_eq!(find_item(w.dev("B"), "i1")["localNote"], json!("B-private"));
    assert_eq!(w.dev("A").doc["theme"], json!("from-B"));
    w.assert_converged("Local");
}

#[test]
fn concurrent_edit_of_the_same_field_later_hlc_wins_everywhere() {
    let mut w = pair();
    w.dev("A").wall += 100;
    w.dev("B").wall += 200; // B 的墙钟更晚
    w.dev("A").doc["theme"] = json!("A-theme");
    w.dev("B").doc["theme"] = json!("B-theme");
    // 同时捕获，谁也没看见谁
    let ia = w.idx("A");
    let ib = w.idx("B");
    let oa = w.devs[ia].capture(false);
    let ob = w.devs[ib].capture(false);
    w.log.push(Batch {
        origin: ia,
        ops: oa,
    });
    w.log.push(Batch {
        origin: ib,
        ops: ob,
    });
    w.quiesce();
    assert_eq!(w.dev("A").doc["theme"], json!("B-theme"));
    assert_eq!(w.dev("B").doc["theme"], json!("B-theme"));
    w.assert_converged("同字段并发");
}

#[test]
fn deleting_an_element_propagates_and_keeps_siblings() {
    let mut w = pair();
    w.dev("A").wall += 10;
    w.dev("A").doc["items"].as_array_mut().unwrap().remove(0);
    w.quiesce();
    assert_eq!(item_ids(w.dev("B")), ["i2"]);
    assert_eq!(item_ids(w.dev("A")), ["i2"]);
    w.assert_converged("删除元素");
}

#[test]
fn edit_concurrent_with_delete_revives_the_whole_element() {
    let mut w = pair();
    w.dev("A").wall += 100;
    w.dev("B").wall += 200;
    // A 删除 i1；B（没看到删除）随后只改了 i1 的 name
    w.dev("A").doc["items"].as_array_mut().unwrap().remove(0);
    find_item(w.dev("B"), "i1")["name"] = json!("edited-by-B");
    let ia = w.idx("A");
    let ib = w.idx("B");
    let oa = w.devs[ia].capture(false);
    let ob = w.devs[ib].capture(false);
    w.log.push(Batch {
        origin: ia,
        ops: oa,
    });
    w.log.push(Batch {
        origin: ib,
        ops: ob,
    });
    w.quiesce();
    for n in ["A", "B"] {
        let it = find_item(w.dev(n), "i1").clone();
        assert_eq!(it["name"], json!("edited-by-B"), "{n}");
        assert_eq!(
            it["cfg"],
            json!({"k": 1}),
            "{n}: 整个元素复活，其它字段也在"
        );
        assert_eq!(it["list"], json!([1, 2]), "{n}");
    }
    w.assert_converged("复活");
}

#[test]
fn local_delete_losing_to_a_concurrent_field_removal_revives_the_element_locally() {
    // 随机测试发现的回归：删除方的本机文档已经没有这个元素，而对方（时钟更快）对它做的是"删字段"。
    // 删字段也算改动 → 元素在镜像里复活 → 删除方必须把它重建出来，否则两边分叉。
    let mut w = pair();
    w.dev("A").wall += 100;
    w.dev("B").wall += 200;
    w.dev("A").doc["items"].as_array_mut().unwrap().remove(1); // A 删除 i2
    find_item(w.dev("B"), "i2")
        .as_object_mut()
        .unwrap()
        .remove("name"); // B 删掉 i2 的 name
    let ia = w.idx("A");
    let ib = w.idx("B");
    let oa = w.devs[ia].capture(false);
    let ob = w.devs[ib].capture(false);
    w.log.push(Batch {
        origin: ia,
        ops: oa,
    });
    w.log.push(Batch {
        origin: ib,
        ops: ob,
    });
    w.quiesce();
    for n in ["A", "B"] {
        let it = find_item(w.dev(n), "i2").clone();
        assert!(it.get("name").is_none(), "{n}: name 已被删除");
        assert_eq!(it["cfg"], json!({"k": 2}), "{n}: 其余字段随元素回来");
    }
    w.assert_converged("删除 vs 删字段");
}

#[test]
fn emptying_a_list_and_refilling_it_concurrently_converges_on_one_order() {
    // 顺序叶子被删（列表清空）与另一台设备并发加元素：Order 可能输给删除，元素却活着，
    // 此时所有设备必须落到同一个兜底顺序。
    let mut w = pair();
    w.dev("A").wall += 300;
    w.dev("B").wall += 100;
    w.dev("A").doc["items"] = json!([]); // A 清空列表（Order 被删）
    w.dev("B").doc["items"].as_array_mut().unwrap().extend([
        json!({"id": "x1", "name": "x"}),
        json!({"id": "x2", "name": "y"}),
    ]);
    let ia = w.idx("A");
    let ib = w.idx("B");
    let oa = w.devs[ia].capture(false);
    let ob = w.devs[ib].capture(false);
    w.log.push(Batch {
        origin: ia,
        ops: oa,
    });
    w.log.push(Batch {
        origin: ib,
        ops: ob,
    });
    w.quiesce();
    let (ids_a, ids_b) = (item_ids(w.dev("A")), item_ids(w.dev("B")));
    assert_eq!(ids_a, ids_b);
    assert_eq!(ids_a, ["x1", "x2"]);
    w.assert_converged("清空与并发新增");
}

#[test]
fn delete_after_edit_wins() {
    let mut w = pair();
    w.dev("A").wall += 100;
    w.dev("B").wall += 200;
    find_item(w.dev("A"), "i1")["name"] = json!("edited-by-A");
    w.dev("B").doc["items"].as_array_mut().unwrap().remove(0); // B 的删除更晚
    let ia = w.idx("A");
    let ib = w.idx("B");
    let oa = w.devs[ia].capture(false);
    let ob = w.devs[ib].capture(false);
    w.log.push(Batch {
        origin: ia,
        ops: oa,
    });
    w.log.push(Batch {
        origin: ib,
        ops: ob,
    });
    w.quiesce();
    assert_eq!(item_ids(w.dev("A")), ["i2"]);
    assert_eq!(item_ids(w.dev("B")), ["i2"]);
    w.assert_converged("删除更晚");
}

#[test]
fn concurrent_adds_keep_both_elements_with_a_consistent_order() {
    let mut w = pair();
    w.dev("A").wall += 100;
    w.dev("B").wall += 200;
    w.dev("A").doc["items"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "from-A", "name": "a"}));
    w.dev("B").doc["items"]
        .as_array_mut()
        .unwrap()
        .insert(0, json!({"id": "from-B", "name": "b"}));
    let ia = w.idx("A");
    let ib = w.idx("B");
    let oa = w.devs[ia].capture(false);
    let ob = w.devs[ib].capture(false);
    w.log.push(Batch {
        origin: ia,
        ops: oa,
    });
    w.log.push(Batch {
        origin: ib,
        ops: ob,
    });
    w.quiesce();
    let ids_a = item_ids(w.dev("A"));
    let ids_b = item_ids(w.dev("B"));
    assert_eq!(ids_a, ids_b, "两边顺序必须一致");
    assert_eq!(ids_a.len(), 4);
    assert!(ids_a.contains(&"from-A".to_string()) && ids_a.contains(&"from-B".to_string()));
    w.assert_converged("并发新增");
}

#[test]
fn reorder_propagates() {
    let mut w = pair();
    w.dev("A").wall += 10;
    w.dev("A").doc["items"].as_array_mut().unwrap().swap(0, 1);
    w.quiesce();
    assert_eq!(item_ids(w.dev("B")), ["i2", "i1"]);
    w.assert_converged("重排");
}

#[test]
fn removing_a_field_propagates_and_atomic_arrays_replace_wholesale() {
    let mut w = pair();
    w.dev("A").wall += 10;
    find_item(w.dev("A"), "i1")
        .as_object_mut()
        .unwrap()
        .remove("name");
    find_item(w.dev("A"), "i1")["list"] = json!([9]);
    find_item(w.dev("A"), "i2")["list"] = json!([]);
    w.dev("A").doc["prefs"]["nested"] = json!({});
    w.quiesce();
    assert!(find_item(w.dev("B"), "i1").get("name").is_none());
    assert_eq!(find_item(w.dev("B"), "i1")["list"], json!([9]));
    assert_eq!(find_item(w.dev("B"), "i2")["list"], json!([]));
    w.assert_converged("删字段");
}

#[test]
fn nested_elements_flow_through_the_pipeline() {
    let mut w = pair();
    w.dev("A").wall += 10;
    find_item(w.dev("A"), "i1")["tags"] =
        json!([{"id": "t1", "label": "x"}, {"id": "t2", "label": "y"}]);
    w.quiesce();
    assert_eq!(
        find_item(w.dev("B"), "i1")["tags"],
        json!([{"id": "t1", "label": "x"}, {"id": "t2", "label": "y"}])
    );
    // B 改 t1、删 t2、加 t3、重排
    w.dev("B").wall += 10;
    find_item(w.dev("B"), "i1")["tags"] =
        json!([{"id": "t3", "label": "z"}, {"id": "t1", "label": "x2"}]);
    w.quiesce();
    assert_eq!(
        find_item(w.dev("A"), "i1")["tags"],
        json!([{"id": "t3", "label": "z"}, {"id": "t1", "label": "x2"}])
    );
    // 把所有 tag 删光：回到空数组
    w.dev("A").wall += 10;
    find_item(w.dev("A"), "i1")["tags"] = json!([]);
    w.quiesce();
    assert_eq!(find_item(w.dev("B"), "i1")["tags"], json!([]));
    w.assert_converged("嵌套");
}

#[test]
fn platform_scope_only_syncs_between_same_platform() {
    let mut w = World::new(vec![
        Device::new("A", Platform::Desktop, base_doc(), 1_000_000),
        Device::new("B", Platform::Desktop, base_doc(), 1_000_000),
        Device::new("P", Platform::Android, base_doc(), 1_000_000),
    ]);
    w.quiesce();
    w.dev("A").wall += 10;
    w.dev("A").doc["notify"]["sound"] = json!(false);
    w.dev("P").wall += 20;
    w.dev("P").doc["notify"]["sound"] = json!("android-value");
    w.dev("P").doc["theme"] = json!("from-phone");
    w.quiesce();
    assert_eq!(
        w.dev("B").doc["notify"]["sound"],
        json!(false),
        "桌面之间同步"
    );
    assert_eq!(
        w.dev("A").doc["notify"]["sound"],
        json!(false),
        "桌面不被手机覆盖"
    );
    assert_eq!(
        w.dev("P").doc["notify"]["sound"],
        json!("android-value"),
        "手机不被桌面覆盖"
    );
    assert_eq!(
        w.dev("A").doc["theme"],
        json!("from-phone"),
        "Shared 部分三端同步"
    );
    w.assert_converged("平台");
}

#[test]
fn local_edit_made_before_apply_is_not_swallowed_when_capture_comes_first() {
    // 运行时契约：落地前必须先捕获一次。这里模拟「防抖窗口里本机又改了」。
    let mut w = pair();
    w.dev("B").wall += 10;
    w.dev("B").doc["theme"] = json!("B-remote");
    w.sync("B"); // 远端改动已上传

    w.dev("A").wall += 20;
    w.dev("A").doc["prefs"]["font"] = json!(30); // A 的本机改动，尚未被任何一轮捕获
    w.sync("A"); // sync = 捕获 → 再落地
    assert_eq!(w.dev("A").doc["theme"], json!("B-remote"));
    assert_eq!(w.dev("A").doc["prefs"]["font"], json!(30));
    w.sync("B");
    assert_eq!(
        w.dev("B").doc["prefs"]["font"],
        json!(30),
        "A 的改动没有被吞掉"
    );
    w.assert_converged("先捕获再落地");
}

#[test]
fn applying_without_capturing_first_would_lose_the_local_edit() {
    // 反面教材：钉住「为什么必须先捕获」。落地是把文档对齐到镜像，没捕获的本机改动镜像还不知道，
    // 会被镜像里的旧值盖回去，而且永远不会被上报。
    let mut w = pair();
    w.dev("B").wall += 10;
    w.dev("B").doc["theme"] = json!("B-remote");
    w.sync("B");

    let ia = w.idx("A");
    w.dev("A").wall += 20;
    w.dev("A").doc["prefs"]["font"] = json!(30);
    let incoming: Vec<Batch> = w.log[w.devs[ia].cursor..]
        .iter()
        .map(|b| Batch {
            origin: b.origin,
            ops: b.ops.clone(),
        })
        .collect();
    w.devs[ia].receive(&incoming); // 没有先 capture
    w.devs[ia].cursor = w.log.len();
    assert_eq!(
        w.devs[ia].doc["prefs"]["font"],
        json!(12),
        "未捕获的改动被镜像旧值盖回"
    );
    assert!(w.devs[ia].capture(false).is_empty(), "也永远不会上报");
}

#[test]
fn fields_unknown_to_an_older_version_survive_and_are_never_deleted_by_it() {
    let mut w = pair();
    // A（新版本）写了 B（旧版本）不认识的字段
    w.dev("A").wall += 10;
    w.dev("A").doc["future"] = json!({"x": 1, "y": [1, 2]});
    w.sync("A");
    w.sync("B");
    assert_eq!(w.dev("B").doc["future"], json!({"x": 1, "y": [1, 2]}));
    // 旧版本反序列化再序列化会丢掉它：模拟 B 提交后的文档与基线都不含 future
    let d = w.dev("B");
    d.doc.as_object_mut().unwrap().remove("future");
    d.baseline = d.doc.clone();
    d.wall += 10;
    d.doc["theme"] = json!("B-edit");
    w.sync("B");
    // B 的这轮捕获不能含对 future 的 Remove
    let removed_future = w
        .log
        .iter()
        .flat_map(|b| &b.ops)
        .any(|o| o.kind == OpKind::Remove && o.path.first() == Some(&Seg::key("future")));
    assert!(!removed_future);
    w.sync("A");
    assert_eq!(
        w.dev("A").doc["future"],
        json!({"x": 1, "y": [1, 2]}),
        "新版本的数据没被旧版本拆掉"
    );
    assert_eq!(w.dev("A").doc["theme"], json!("B-edit"));
}

#[test]
fn suppress_removes_after_upgrade_keeps_other_devices_fields() {
    let mut w = pair();
    // A 升级：新版本不再有 prefs.nested，也迁走了 i1.list
    w.dev("A").wall += 10;
    w.dev("A").doc["prefs"]
        .as_object_mut()
        .unwrap()
        .remove("nested");
    find_item(w.dev("A"), "i1")
        .as_object_mut()
        .unwrap()
        .remove("list");
    let ia = w.idx("A");
    let ops = w.devs[ia].capture(true); // 版本刚升级后的第一次捕获
    assert!(!ops.iter().any(|o| o.kind == OpKind::Remove));
    w.log.push(Batch { origin: ia, ops });
    w.quiesce();
    // B 仍在用旧字段：没被删
    assert_eq!(w.dev("B").doc["prefs"]["nested"], json!({"a": "x"}));
    assert_eq!(find_item(w.dev("B"), "i1")["list"], json!([1, 2]));
}

/// 「加入」的标准流程（规格 4.6）：拉取云端全部 → 本机已有数据以 ZERO hlc 并入 →
/// 以「本机文档现状 vs 镜像可见集」为受影响路径全量落地 → 新基线。
///
/// 返回本机数据对应的 ZERO op：它们已并入本机镜像，**但还要上传**（进 outbox），否则其它设备
/// 永远看不到本机独有的东西。
fn join(j: &mut Device, cloud_ops: &[Op]) -> Vec<Op> {
    ingest_remote(
        &mut j.mirror,
        cloud_ops,
        &mut j.idmaps,
        &mut j.clock,
        j.wall,
    );
    let zero = Hlc::zero_for(j.name);
    let zero_ops = export_all_at(SOURCE, &j.doc, &RULES, j.platform, &mut j.idmaps, &zero);
    for o in &zero_ops {
        j.mirror.merge(o);
    }
    let aff = affected_vs_doc(&j.doc, &j.mirror, &RULES, j.platform, &mut j.idmaps);
    j.doc = apply_to_doc(&j.doc, &j.mirror, &aff, &RULES, j.platform);
    j.baseline = j.doc.clone();
    zero_ops
}

#[test]
fn join_merges_existing_local_data_with_cloud_winning_conflicts() {
    // 云端：A 已有数据
    let mut a = Device::new("A", Platform::Desktop, base_doc(), 2_000_000);
    a.doc["theme"] = json!("cloud-theme");
    a.doc["items"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "cloud-only", "name": "c"}));
    let ops = a.capture(false);

    // 新设备 J：已有一份与云端冲突、又有独有内容的本机数据
    let j_doc = json!({
        "theme": "local-theme",
        "prefs": {"font": 99, "localOnlyPref": true},
        "window": {"w": 7},
        "items": [
            {"id": "i1", "name": "J-one", "extra": "J-extra", "localNote": "J-note"},
            {"id": "j-only", "name": "J only"},
        ],
    });
    let mut j = Device::new("J", Platform::Desktop, j_doc, 2_000_500);
    let zero_ops = join(&mut j, &ops);

    let doc = j.doc.clone();
    assert_eq!(doc["theme"], json!("cloud-theme"), "冲突以云端为准");
    assert_eq!(doc["prefs"]["font"], json!(12), "冲突以云端为准");
    assert_eq!(
        doc["prefs"]["localOnlyPref"],
        json!(true),
        "本机独有字段保留"
    );
    assert_eq!(doc["window"], json!({"w": 7}), "Local 不动");
    let ids: Vec<&str> = doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    for want in ["i1", "i2", "cloud-only", "j-only"] {
        assert!(ids.contains(&want), "{want} 应在 {ids:?}");
    }
    let i1 = doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "i1")
        .unwrap();
    assert_eq!(i1["name"], json!("one"), "冲突以云端为准");
    assert_eq!(i1["extra"], json!("J-extra"), "本机独有字段保留");
    assert_eq!(i1["localNote"], json!("J-note"));

    // 进入常规同步：J 独有的内容在下一轮被上传，A 随后拿到
    let mut w = World::new(vec![a, j]);
    w.log.push(Batch { origin: 0, ops });
    w.log.push(Batch {
        origin: 1,
        ops: zero_ops,
    });
    w.devs[0].cursor = 1;
    w.devs[1].cursor = 2;
    w.quiesce();
    let a_ids = item_ids(w.dev("A"));
    assert!(a_ids.contains(&"j-only".to_string()), "{a_ids:?}");
    assert_eq!(w.dev("A").doc["prefs"]["localOnlyPref"], json!(true));
    assert_eq!(w.dev("A").doc["theme"], json!("cloud-theme"));
    assert_eq!(
        find_item(w.dev("A"), "i1")["extra"],
        json!("J-extra"),
        "本机独有字段随后到了云端"
    );
    w.assert_converged("加入后");
}

#[test]
fn cloud_deletion_beats_joining_devices_local_copy() {
    // 云端：创建后又删掉了 i1
    let mut a = Device::new("A", Platform::Desktop, base_doc(), 2_000_000);
    let mut cloud = a.capture(false);
    a.wall += 10;
    a.doc["items"].as_array_mut().unwrap().remove(0);
    cloud.extend(a.capture(false));

    // J 本机还有 i1：加入后 i1 不会复活，也不会留在本机
    let mut j = Device::new("J", Platform::Desktop, base_doc(), 1_000_000);
    let zero_ops = join(&mut j, &cloud);
    assert_eq!(item_ids(&j), ["i2"]);

    let mut w = World::new(vec![a, j]);
    w.log.push(Batch {
        origin: 0,
        ops: cloud,
    });
    w.log.push(Batch {
        origin: 1,
        ops: zero_ops,
    });
    w.devs[0].cursor = 1;
    w.devs[1].cursor = 2;
    w.quiesce();
    assert_eq!(item_ids(w.dev("A")), ["i2"]);
    w.assert_converged("加入后");
}

#[test]
fn join_with_identical_data_changes_nothing_and_uploads_nothing_new() {
    let mut a = Device::new("A", Platform::Desktop, base_doc(), 2_000_000);
    let ops = a.capture(false);
    let mut j = Device::new("J", Platform::Desktop, base_doc(), 2_000_100);
    let _ = join(&mut j, &ops);
    assert_eq!(j.doc, base_doc());
    // 本机数据以 ZERO hlc 并入了镜像，但基线 = 当前：下一轮捕获没有任何新 op
    assert!(j.capture(false).is_empty());
}

// ---------------------------------------------------------------------------
// 随机多设备
// ---------------------------------------------------------------------------

struct Gen {
    rng: Rng,
    next_id: u32,
}

impl Gen {
    fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    fn new_item(&mut self, dev: &str) -> Value {
        let id = self.fresh_id("n");
        let mut it = json!({
            "id": id,
            "name": format!("name-{}", self.rng.below(5)),
            "cfg": {"k": self.rng.below(5)},
            "list": [self.rng.below(3)],
            "tags": [],
            "localNote": format!("{dev}-{}", self.rng.below(5)),
        });
        if self.rng.chance(30) {
            it["tags"] = json!([{"id": self.fresh_id("t"), "label": "x"}]);
        }
        it
    }
}

/// 对一台设备随机改一处。返回是否改了 Local 字段（用来核对 Local 不串）。
fn mutate(g: &mut Gen, d: &mut Device) {
    let name = d.name;
    let rng_kind = g.rng.below(16);
    let items_len = d.doc["items"].as_array().map_or(0, Vec::len);
    let pick_item = |g: &mut Gen| g.rng.below(items_len.max(1));
    match rng_kind {
        0 => d.doc["theme"] = json!(format!("theme-{}", g.rng.below(4))),
        1 => d.doc["prefs"]["font"] = json!(g.rng.below(30)),
        2 => {
            if g.rng.chance(50) {
                d.doc["prefs"]["nested"]["a"] = json!(format!("v{}", g.rng.below(4)));
            } else if let Some(o) = d.doc["prefs"]["nested"].as_object_mut() {
                o.remove("a");
            }
        }
        3 => d.doc["window"]["w"] = json!(format!("{name}-{}", g.rng.below(100))),
        4 => d.doc["notify"]["sound"] = json!(format!("{name}-{}", g.rng.below(5))),
        5 => {
            let it = g.new_item(name);
            let arr = d.doc["items"].as_array_mut().unwrap();
            let at = g.rng.below(arr.len() + 1);
            arr.insert(at, it);
        }
        6 if items_len > 0 => {
            let at = pick_item(g);
            d.doc["items"].as_array_mut().unwrap().remove(at);
        }
        7 if items_len > 0 => {
            let at = pick_item(g);
            d.doc["items"][at]["name"] = json!(format!("name-{}", g.rng.below(5)));
        }
        8 if items_len > 0 => {
            let at = pick_item(g);
            if g.rng.chance(70) {
                d.doc["items"][at]["cfg"]["k"] = json!(g.rng.below(5));
            } else {
                d.doc["items"][at]["cfg"] = json!({});
            }
        }
        9 if items_len > 0 => {
            let at = pick_item(g);
            let n = g.rng.below(3);
            d.doc["items"][at]["list"] =
                Value::Array((0..n).map(|_| json!(g.rng.below(4))).collect());
        }
        10 if items_len > 0 => {
            let at = pick_item(g);
            let tid = g.fresh_id("t");
            let tags = &mut d.doc["items"][at]["tags"];
            if !tags.is_array() {
                *tags = json!([]);
            }
            let arr = tags.as_array_mut().unwrap();
            let pos = g.rng.below(arr.len() + 1);
            arr.insert(
                pos,
                json!({"id": tid, "label": format!("l{}", g.rng.below(3))}),
            );
        }
        11 if items_len > 0 => {
            let at = pick_item(g);
            if let Some(arr) = d.doc["items"][at]["tags"].as_array_mut() {
                if !arr.is_empty() {
                    let p = g.rng.below(arr.len());
                    arr.remove(p);
                }
            }
        }
        12 if items_len > 0 => {
            let at = pick_item(g);
            if let Some(arr) = d.doc["items"][at]["tags"].as_array_mut() {
                if !arr.is_empty() {
                    let p = g.rng.below(arr.len());
                    arr[p]["label"] = json!(format!("l{}", g.rng.below(3)));
                }
            }
        }
        13 if items_len > 1 => {
            let a = g.rng.below(items_len);
            let b = g.rng.below(items_len);
            d.doc["items"].as_array_mut().unwrap().swap(a, b);
        }
        14 if items_len > 0 => {
            let at = pick_item(g);
            d.doc["items"][at]["localNote"] = json!(format!("{name}-{}", g.rng.below(50)));
        }
        15 if items_len > 0 => {
            let at = pick_item(g);
            if let Some(o) = d.doc["items"][at].as_object_mut() {
                o.remove("name");
            }
        }
        _ => d.doc["prefs"]["font"] = json!(g.rng.below(30)),
    }
}

/// 一次随机场景：3 台设备（2 桌面 + 1 手机），随机改动 / 部分同步 / 重复投递，最后静默并校验。
/// 重复投递时严格走「捕获→上传→落地」，不丢任何本机改动。
fn run_random(seed: u64, steps: usize) {
    let mut g = Gen {
        rng: Rng::new(seed),
        next_id: 0,
    };
    let mut w = World::new(vec![
        Device::new("A", Platform::Desktop, base_doc(), 1_000_000),
        Device::new("B", Platform::Desktop, base_doc(), 1_000_000 + 5_000),
        Device::new("C", Platform::Android, base_doc(), 1_000_000 - 3_000),
    ]);
    w.quiesce();

    // 每台设备自己设置过的 Local 值
    let mut window_w: [Option<Value>; 3] = [None, None, None];
    let mut local_notes: [BTreeMap<String, Value>; 3] =
        [BTreeMap::new(), BTreeMap::new(), BTreeMap::new()];

    for _ in 0..steps {
        let i = g.rng.below(3);
        w.devs[i].wall += g.rng.below(40) as u64;
        for _ in 0..1 + g.rng.below(3) {
            let mut d = std::mem::replace(
                &mut w.devs[i],
                Device::new("tmp", Platform::Desktop, Value::Null, 0),
            );
            mutate(&mut g, &mut d);
            w.devs[i] = d;
        }
        // 刷新"本机私有值"的期望（以文档里现有为准——只有本机自己会改它们）
        window_w[i] = Some(w.devs[i].doc["window"].clone());
        local_notes[i] = w.devs[i].doc["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        Some((e["id"].as_str()?.to_string(), e.get("localNote")?.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        if g.rng.chance(60) {
            let max = if g.rng.chance(50) {
                usize::MAX
            } else {
                1 + g.rng.below(3)
            };
            w.sync_n(i, max);
            // 同步（落地）可能新建 / 删除元素：重新取本机私有值的期望
            window_w[i] = Some(w.devs[i].doc["window"].clone());
            local_notes[i] = w.devs[i].doc["items"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            Some((e["id"].as_str()?.to_string(), e.get("localNote")?.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        if g.rng.chance(10) && !w.log.is_empty() {
            let k = g.rng.below(w.log.len());
            if w.log[k].origin != i {
                let ops = w.devs[i].capture(false);
                if !ops.is_empty() {
                    w.log.push(Batch { origin: i, ops });
                }
                let b = Batch {
                    origin: w.log[k].origin,
                    ops: w.log[k].ops.clone(),
                };
                w.devs[i].receive(&[b]);
            }
        }
    }
    w.quiesce();

    let ctx = format!("seed {seed}");
    w.assert_converged(&ctx);

    // Local 部分各自保留
    for (i, d) in w.devs.iter().enumerate() {
        if let Some(expect) = &window_w[i] {
            assert_eq!(
                &d.doc["window"], expect,
                "{ctx}: {} 的 window 被改动",
                d.name
            );
        }
        for (id, note) in &local_notes[i] {
            if let Some(it) = d.doc["items"]
                .as_array()
                .and_then(|a| a.iter().find(|e| e["id"] == *id))
            {
                // 元素被远端删掉又复活时是重建出来的，Local 字段随旧元素一起没了——这是预期；
                // 不允许的是它变成了别的值（尤其是别的设备的值）。
                if let Some(now) = it.get("localNote") {
                    assert_eq!(now, note, "{ctx}: {} 的 {id}.localNote 被改动", d.name);
                }
            }
        }
    }

    // 文档反映镜像：Shared 非顺序叶子 == 镜像可见集（桌面设备看自己的视角）。
    // 空数组叶子两边都排除：未登记前的原子 `[]` 叶子在镜像里可能作为陈旧值重新露面，语义上等价于"没有"。
    for d in &w.devs {
        let doc_view: BTreeMap<Path, Value> = d
            .shared_view()
            .into_iter()
            .filter(|(p, v)| !p.last().is_some_and(Seg::is_order) && v != &json!([]))
            .collect();
        let mirror_view: BTreeMap<Path, Value> = d
            .mirror
            .visible()
            .into_iter()
            .filter(|(p, v)| {
                !is_platform_path(p) && !p.last().is_some_and(Seg::is_order) && v != &json!([])
            })
            .collect();
        assert_eq!(
            doc_view, mirror_view,
            "{ctx}: {} 的文档与镜像可见集不一致",
            d.name
        );
    }
}

/// 压力倍数：`SYNC_STRESS=20 cargo test --lib sync::core::e2e_tests` 把随机场景数放大 20 倍。
fn stress() -> u64 {
    std::env::var("SYNC_STRESS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

#[test]
fn random_multi_device_convergence_short() {
    for seed in 0..150 * stress() {
        run_random(1000 + seed, 25);
    }
}

#[test]
fn random_multi_device_convergence_long() {
    for seed in 0..30 * stress() {
        run_random(50_000 + seed, 120);
    }
}
