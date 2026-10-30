//! 旧版本遗留的落盘文件：登记表里**没有**位置的历史产物，只在这里允许拼路径。
//!
//! 旧版同步（基于服务器的方案）已经整体移除，但它在用户配置目录里留下过几个文件。
//! 这些文件不再有任何读者，留着只会让人困惑（以为数据还在同步），所以每次启动都清一遍：
//! 只做 `remove_file`，文件不存在是常态（几乎所有用户都没有），开销可以忽略。
//!
//! 注意**不要**清 `session_snapshot.json`：它来历不明，宁可留着也不误删。

use std::path::Path;

/// 旧版同步留在配置目录里的文件名。
pub const LEGACY_SYNC_FILES: &[&str] = &[
    "sync_local_versions.json",
    "sync_profile.json",
    "sync_pending_conflicts.json",
];

/// 删除旧版同步遗留的文件，返回**真的删掉了**的文件名（给日志用）。
/// 单个文件删不掉只记日志、继续删下一个：这是清理，不是启动的前提条件。
pub fn cleanup_legacy_sync_files(config_dir: &Path) -> Vec<&'static str> {
    let mut removed = Vec::new();
    for name in LEGACY_SYNC_FILES {
        let path = config_dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(*name),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("清理旧版同步遗留文件 {name} 失败: {e}"),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_the_legacy_sync_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in LEGACY_SYNC_FILES {
            std::fs::write(dir.path().join(name), "{}").unwrap();
        }
        // 来历不明的 session_snapshot.json 与正常配置都不能动
        std::fs::write(dir.path().join("session_snapshot.json"), "{}").unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();

        let removed = cleanup_legacy_sync_files(dir.path());
        assert_eq!(removed.len(), LEGACY_SYNC_FILES.len());
        for name in LEGACY_SYNC_FILES {
            assert!(!dir.path().join(name).exists(), "{name} 应被删除");
        }
        assert!(dir.path().join("session_snapshot.json").exists());
        assert!(dir.path().join("settings.json").exists());
    }

    #[test]
    fn missing_files_are_not_an_error_and_cleanup_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cleanup_legacy_sync_files(dir.path()).is_empty());
        std::fs::write(dir.path().join("sync_profile.json"), "{}").unwrap();
        assert_eq!(
            cleanup_legacy_sync_files(dir.path()),
            vec!["sync_profile.json"]
        );
        assert!(cleanup_legacy_sync_files(dir.path()).is_empty());
    }
}
