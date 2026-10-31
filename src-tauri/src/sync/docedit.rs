//! 对文档 JSON 的几个小手术：同步记录的「撤销」、加入同步时的连接去重 / 引用改写用。
//!
//! 刻意不复用 `core::apply`：那边是「按镜像落地」，这里是「按我指定的路径改一处」，
//! 而且这里不碰作用域规则（调用方已经知道自己在改什么）。路径是 `Seg` 序列，**文档内路径**
//! （不含平台前缀），`Seg::Key` 是线上形式的 key（`@@x` 要还原）。

use serde_json::{json, Map, Value};

use super::core::path::unescape_key;
use super::core::Seg;

fn element_id(v: &Value) -> Option<&str> {
    v.as_object()?.get("id")?.as_str()
}

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

fn coerce(v: &mut Value, next: &Seg) {
    if next.is_id() {
        if !v.is_array() {
            *v = Value::Array(Vec::new());
        }
    } else if !v.is_object() {
        *v = Value::Object(Map::new());
    }
}

/// 写一个叶子，沿途缺的对象 / 数组 / 元素自动建。路径空或以顺序段结尾返回 `false`。
pub fn set_leaf(root: &mut Value, path: &[Seg], value: Value) -> bool {
    let Some(first) = path.first() else {
        return false;
    };
    coerce(root, first);
    let mut cur = root;
    for (i, seg) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        match seg {
            Seg::Key(k) => {
                let Some(obj) = cur.as_object_mut() else {
                    return false;
                };
                let key = unescape_key(k).to_string();
                if last {
                    obj.insert(key, value);
                    return true;
                }
                let child = obj.entry(key).or_insert(Value::Null);
                coerce(child, &path[i + 1]);
                cur = child;
            }
            Seg::Id { id } => {
                if last {
                    return false; // 元素本身不是叶子
                }
                let Some(arr) = cur.as_array_mut() else {
                    return false;
                };
                let pos = match arr.iter().position(|e| element_id(e) == Some(id.as_str())) {
                    Some(p) => p,
                    None => {
                        arr.push(json!({ "id": id }));
                        arr.len() - 1
                    }
                };
                cur = &mut arr[pos];
            }
            Seg::Order { .. } => return false,
        }
    }
    false
}

/// 删一个叶子（不存在就什么也不做）。返回是否真的删了。
pub fn remove_leaf(root: &mut Value, path: &[Seg]) -> bool {
    let Some((last, parent)) = path.split_last() else {
        return false;
    };
    let Seg::Key(k) = last else { return false };
    match get_mut(root, parent) {
        Some(Value::Object(m)) => m.remove(unescape_key(k)).is_some(),
        _ => false,
    }
}

/// 读一个叶子。
pub fn get_leaf<'a>(root: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path {
        cur = match seg {
            Seg::Key(k) => cur.as_object()?.get(unescape_key(k))?,
            Seg::Id { id } => cur
                .as_array()?
                .iter()
                .find(|e| element_id(e) == Some(id.as_str()))?,
            Seg::Order { .. } => return None,
        };
    }
    Some(cur)
}

/// 把 `path`（以 `Seg::Id` 结尾）指向的元素从列表里整个移除。返回是否真的移除了。
pub fn remove_element(root: &mut Value, path: &[Seg]) -> bool {
    let Some((Seg::Id { id }, list_path)) = path.split_last() else {
        return false;
    };
    match get_mut(root, list_path) {
        Some(Value::Array(arr)) => {
            let before = arr.len();
            arr.retain(|e| element_id(e) != Some(id.as_str()));
            arr.len() != before
        }
        _ => false,
    }
}

/// 把整个元素放回列表末尾（列表里已有同 id 的元素就不动）。返回是否真的放回了。
pub fn restore_element(root: &mut Value, path: &[Seg], element: Value) -> bool {
    let Some((Seg::Id { id }, list_path)) = path.split_last() else {
        return false;
    };
    if get_leaf(root, path).is_some() {
        return false;
    }
    // 列表本身可能整个不存在（比如删光后序列化省略了它），沿途建出来
    if get_mut(root, list_path).is_none() && !set_leaf(root, list_path, Value::Array(Vec::new())) {
        return false;
    }
    match get_mut(root, list_path) {
        Some(Value::Array(arr)) => {
            let mut el = element;
            if element_id(&el).is_none() {
                if let Value::Object(m) = &mut el {
                    m.insert("id".into(), Value::String(id.clone()));
                } else {
                    return false;
                }
            }
            arr.push(el);
            true
        }
        _ => false,
    }
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

    #[test]
    fn set_and_remove_leaf_round_trip() {
        let mut doc = json!({"a": {"b": 1}});
        assert!(set_leaf(&mut doc, &[k("a"), k("c")], json!("x")));
        assert_eq!(doc, json!({"a": {"b": 1, "c": "x"}}));
        assert!(remove_leaf(&mut doc, &[k("a"), k("b")]));
        assert!(!remove_leaf(&mut doc, &[k("a"), k("b")]), "已经没有了");
        assert_eq!(doc, json!({"a": {"c": "x"}}));
    }

    #[test]
    fn set_leaf_creates_missing_parents_and_elements() {
        let mut doc = json!({});
        assert!(set_leaf(
            &mut doc,
            &[k("l"), id("e1"), k("name")],
            json!("n")
        ));
        assert_eq!(doc, json!({"l": [{"id": "e1", "name": "n"}]}));
        assert!(set_leaf(
            &mut doc,
            &[k("l"), id("e1"), k("host")],
            json!("h")
        ));
        assert_eq!(doc["l"][0]["host"], json!("h"));
        assert_eq!(doc["l"].as_array().unwrap().len(), 1, "不重复建元素");
    }

    #[test]
    fn remove_and_restore_element() {
        let mut doc = json!({"l": [{"id": "a", "n": 1}, {"id": "b", "n": 2}]});
        assert!(remove_element(&mut doc, &[k("l"), id("a")]));
        assert_eq!(doc, json!({"l": [{"id": "b", "n": 2}]}));
        assert!(!remove_element(&mut doc, &[k("l"), id("a")]));

        assert!(restore_element(
            &mut doc,
            &[k("l"), id("a")],
            json!({"id": "a", "n": 1})
        ));
        assert_eq!(doc["l"][1], json!({"id": "a", "n": 1}));
        // 已经有同 id 的元素：不覆盖
        assert!(!restore_element(
            &mut doc,
            &[k("l"), id("a")],
            json!({"id": "a", "n": 99})
        ));
        assert_eq!(doc["l"][1]["n"], json!(1));
    }

    #[test]
    fn restore_element_into_a_missing_list() {
        let mut doc = json!({});
        assert!(restore_element(
            &mut doc,
            &[k("l"), id("a")],
            json!({"id": "a"})
        ));
        assert_eq!(doc, json!({"l": [{"id": "a"}]}));
    }

    #[test]
    fn at_prefixed_keys_are_unescaped() {
        let mut doc = json!({});
        assert!(set_leaf(&mut doc, &[k("@@x")], json!(1)));
        assert_eq!(doc, json!({"@x": 1}));
        assert!(remove_leaf(&mut doc, &[k("@@x")]));
        assert_eq!(doc, json!({}));
    }
}
