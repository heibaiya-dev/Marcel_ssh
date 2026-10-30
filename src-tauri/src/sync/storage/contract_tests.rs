//! 适配器共用的契约测试：`SyncStorage` 的约定（见 `storage` 模块文档）只在这一处写成断言，
//! 内存 / WebDAV / SFTP 各自的测试都调 [`run`]。
//!
//! 为什么要共用：三个适配器行为必须一致，同步引擎才能不关心背后是谁。分头写测试，迟早有一个
//! 适配器悄悄偏离（比如 SFTP 不覆盖已有文件、WebDAV 的 `list` 把自己也列出来）。

use super::{StorageError, SyncStorage};

/// 对一个**空的**存储（root 可以不存在）跑完整的契约。结束时把自己写的东西都删干净。
pub(crate) async fn run(s: &dyn SyncStorage) {
    // 探针：root 不存在时负责建出来，且不留痕迹。
    s.probe().await.expect("probe 应当成功");
    assert!(
        s.list("").await.unwrap().is_empty(),
        "probe 之后 root 应是空的"
    );

    // 不存在 → 空 / None，不是错误。
    assert!(s.list("devices").await.unwrap().is_empty());
    assert_eq!(
        s.get("devices/dev1/seg-000000000001.msync").await.unwrap(),
        None
    );
    // 删除不存在的东西也算成功。
    s.delete("devices/nope/seg-1.msync").await.unwrap();
    s.delete("devices/nope").await.unwrap();

    // put 自动建父目录；覆盖同名文件。
    let seg = "devices/dev1/seg-000000000001.msync";
    s.put(seg, b"hello").await.unwrap();
    assert_eq!(s.get(seg).await.unwrap().as_deref(), Some(&b"hello"[..]));
    s.put(seg, b"hello again, longer").await.unwrap();
    assert_eq!(
        s.get(seg).await.unwrap().as_deref(),
        Some(&b"hello again, longer"[..]),
        "覆盖写必须真的覆盖（SFTP 的 rename 默认不覆盖已有文件）"
    );
    s.put(seg, b"short").await.unwrap();
    assert_eq!(
        s.get(seg).await.unwrap().as_deref(),
        Some(&b"short"[..]),
        "覆盖成更短的内容时不能残留旧内容的尾巴"
    );

    // 各种名字与二进制内容：中文、空格、加号、百分号、括号都得原样往返。
    let odd = "devices/dev1/我的 文件+1(2)%25.msync";
    let blob: Vec<u8> = (0..=255u8).chain([0, 0, 255]).collect();
    s.put(odd, &blob).await.unwrap();
    assert_eq!(s.get(odd).await.unwrap(), Some(blob.clone()));

    // 空文件。
    s.put("empty.bin", b"").await.unwrap();
    assert_eq!(s.get("empty.bin").await.unwrap(), Some(Vec::new()));

    // 列目录：只给直接子项，名字已还原（不是编码后的），目录带 is_dir，文件带大小。
    let mut root = s.list("").await.unwrap();
    root.sort_by(|a, b| a.name.cmp(&b.name));
    let root: Vec<(&str, bool, u64)> = root
        .iter()
        .map(|e| (e.name.as_str(), e.is_dir, e.size))
        .collect();
    assert_eq!(root, vec![("devices", true, 0), ("empty.bin", false, 0)]);

    let devices = s.list("devices").await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(
        (devices[0].name.as_str(), devices[0].is_dir),
        ("dev1", true)
    );

    let mut files = s.list("devices/dev1/").await.unwrap(); // 首尾多余的 / 无所谓
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let files: Vec<(&str, bool, u64)> = files
        .iter()
        .map(|e| (e.name.as_str(), e.is_dir, e.size))
        .collect();
    assert_eq!(
        files,
        vec![
            ("seg-000000000001.msync", false, 5),
            ("我的 文件+1(2)%25.msync", false, blob.len() as u64),
        ]
    );

    // 删文件。
    s.delete(seg).await.unwrap();
    assert_eq!(s.get(seg).await.unwrap(), None);
    s.delete(seg).await.unwrap();

    // 删目录：连内容（含子目录）一起删，同级其它东西不动。
    s.put("devices/dev2/snap-000000000001.msync", b"a")
        .await
        .unwrap();
    s.put("devices/dev2/sub/deep.msync", b"b").await.unwrap();
    s.delete("devices/dev2").await.unwrap();
    assert_eq!(s.get("devices/dev2/sub/deep.msync").await.unwrap(), None);
    let after: Vec<String> = s
        .list("devices")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(after, vec!["dev1".to_string()]);
    assert!(s.get(odd).await.unwrap().is_some(), "删 dev2 不该动 dev1");

    // 越界 / 非法路径一律拒绝，而且不会去碰存储。
    for bad in ["../x", "a/../b", "a\\b"] {
        assert!(
            matches!(s.get(bad).await, Err(StorageError::Other(_))),
            "get {bad:?}"
        );
        assert!(
            matches!(s.put(bad, b"x").await, Err(StorageError::Other(_))),
            "put {bad:?}"
        );
        assert!(
            matches!(s.delete(bad).await, Err(StorageError::Other(_))),
            "delete {bad:?}"
        );
        assert!(
            matches!(s.list(bad).await, Err(StorageError::Other(_))),
            "list {bad:?}"
        );
    }
    // 不能把根本身删了，也不能往根路径上写文件。
    assert!(matches!(s.delete("").await, Err(StorageError::Other(_))));
    assert!(matches!(
        s.put("/", b"x").await,
        Err(StorageError::Other(_))
    ));

    // 收尾：把自己建的都删干净，root 回到空。
    s.delete("devices").await.unwrap();
    s.delete("empty.bin").await.unwrap();
    assert!(s.list("").await.unwrap().is_empty());
}
