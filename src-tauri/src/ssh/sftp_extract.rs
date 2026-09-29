#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveType {
    Zip,
    TarGz,
    TarBz2,
    TarXz,
    Tar,
}

/// Compound extensions ordered longest-first so `.tar.gz` matches before `.gz`.
const ARCHIVE_EXTENSIONS: &[(&str, ArchiveType)] = &[
    (".tar.gz", ArchiveType::TarGz),
    (".tar.bz2", ArchiveType::TarBz2),
    (".tar.xz", ArchiveType::TarXz),
    (".tgz", ArchiveType::TarGz),
    (".tbz2", ArchiveType::TarBz2),
    (".txz", ArchiveType::TarXz),
    (".tar", ArchiveType::Tar),
    (".zip", ArchiveType::Zip),
];

pub(crate) fn get_archive_type(filename: &str) -> Option<ArchiveType> {
    let lower = filename.to_ascii_lowercase();
    for &(ext, kind) in ARCHIVE_EXTENSIONS {
        if lower.ends_with(ext) {
            return Some(kind);
        }
    }
    None
}

/// Build a shell command to extract an archive to a target dir.
///
/// Returns "OK" on success.
pub(crate) fn build_extract_to_dir_cmd(
    archive_path: &str,
    target_dir: &str,
    kind: ArchiveType,
) -> String {
    let dir = crate::util::shell_escape(target_dir);
    let arc = crate::util::shell_escape(archive_path);
    let tmp = "$(mktemp -d /tmp/marcel-extract-XXXXXX)";
    let extract = match kind {
        ArchiveType::Zip => format!("unzip -q {arc} -d \"$tmp\""),
        ArchiveType::TarGz => format!("tar xzf {arc} -C \"$tmp\""),
        ArchiveType::TarBz2 => format!("tar xjf {arc} -C \"$tmp\""),
        ArchiveType::TarXz => format!("tar xJf {arc} -C \"$tmp\""),
        ArchiveType::Tar => format!("tar xf {arc} -C \"$tmp\""),
    };
    // 冲突检测：把解压结果留在临时目录，逐条比对目标目录里是否已有同名条目，
    // 命中即中止（解压「绝不覆盖」的承诺靠它）。三个细节都不能省：
    // 1. `-print0` + `read -r -d ''`：文件名可以含换行，按行读会把一个名字拆成
    //    两个 —— 既漏判真冲突（覆盖用户已有文件），也误报假冲突；
    // 2. `-e` 会跟随符号链接，目标侧是「悬空软链」时它为假，只判 `-e` 会放过冲突：
    //    随后 `cp` 至少会中途报错并留下半份已拷入的文件（GNU cp 拒绝写穿悬空软链，
    //    跟随目标软链的实现则直接写到目标目录之外），故补判 `-L`；
    // 3. `cp -a -n` 兜底：检测与复制之间仍有竞态窗口（TOCTOU），`-n` 保证任何
    //    情况下都不覆盖已存在的目标条目。
    format!(
        "tmp={tmp} && trap 'rm -rf \"$tmp\"' EXIT && {extract} && mkdir -p {dir} && cd \"$tmp\" && conflict_file=\"$tmp/.marcel-conflict\" && find . -mindepth 1 -print0 | while IFS= read -r -d '' p; do rel=${{p#./}}; if [ -e {dir}/\"$rel\" ] || [ -L {dir}/\"$rel\" ]; then echo CONFLICT: \"$rel\" > \"$conflict_file\"; break; fi; done && if [ -s \"$conflict_file\" ]; then cat \"$conflict_file\"; exit 1; fi && cp -a -n \"$tmp\"/. {dir}/ && echo OK"
    )
}

pub(crate) fn build_unzip_check_cmd() -> &'static str {
    "command -v unzip >/dev/null 2>&1 && echo OK || echo MISSING_UNZIP"
}

pub(crate) fn build_tar_check_cmd() -> &'static str {
    "command -v tar >/dev/null 2>&1 && echo OK || echo MISSING_TAR"
}

pub(crate) fn build_zip_check_cmd() -> &'static str {
    "command -v zip >/dev/null 2>&1 && echo OK || echo MISSING_ZIP"
}

/// Build a shell command to compress a directory into an archive.
///
/// `source_dir` must be an absolute path. It is split into parent + basename
/// so that `tar -C <parent> -- <basename>` / `zip ... -- <basename>` produces an
/// archive containing the directory itself (not its contents flattened).
///
/// Returns "OK" on success, "FAILED" on non-zero exit. The caller checks for
/// the "OK" marker to determine success (same convention as extract).
///
/// 已存在的 `target_path` 会先被删除再重建：覆盖 = 整包重写，而不是增量更新
/// （zip 的 add/update 不会清理源目录中已删除的旧成员）。
pub(crate) fn build_compress_to_archive_cmd(
    source_dir: &str,
    target_path: &str,
    kind: ArchiveType,
) -> Result<String, &'static str> {
    // Trim trailing '/' so rsplit_once gives the correct basename.
    // Root "/" is rejected upstream by the system-path blacklist.
    let trimmed = source_dir.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("源路径无效");
    }
    let (parent, dirname) = trimmed
        .rsplit_once('/')
        .map(|(p, n)| (p, n))
        .unwrap_or(("", trimmed));
    if dirname.is_empty() {
        return Err("源路径无效");
    }
    // parent == "" means source is a top-level dir like /home; tar -C "" fails,
    // so normalize to "/".
    let parent = if parent.is_empty() { "/" } else { parent };

    let parent_esc = crate::util::shell_escape(parent);
    let dirname_esc = crate::util::shell_escape(dirname);
    let target_esc = crate::util::shell_escape(target_path);

    let compress = match kind {
        ArchiveType::TarGz => {
            // `--` 结束选项解析：目录名以 '-' 开头时（Linux 上合法），否则 tar
            // 会把它当选项（GNU tar: invalid option -- 'e'）。`--` 在 `-C <parent>`
            // 之后、目录名之前，位置合法。
            format!("tar -czf {target_esc} -C {parent_esc} -- {dirname_esc}")
        }
        ArchiveType::Zip => {
            // zip needs to chdir to parent first; -r recursive, -q quiet (we
            // still want stderr for errors), -y store symlinks as-is.
            // `--` 同样用于结束选项解析：Info-ZIP zip 3.0（fileio.c 的 get_option，
            // doubledash_ends_options 默认开启）规定 `--` 之后的参数一律按文件名
            // 处理，且 `--` 只能出现在归档名之后（zip.c: "can't use -- before
            // archive name"）—— 本命令把它们放在归档名之后，满足该约束。
            format!("cd {parent_esc} && zip -rqy {target_esc} -- {dirname_esc}")
        }
        _ => return Err("压缩仅支持 tar.gz 和 zip"),
    };

    // 压缩前先删掉目标：zip 是「新增/更新」语义 —— zip 3.0 手册：对已存在的包，
    // 「zip will replace identically named entries ... or add entries for new names」，
    // 例子里的 foo/file2 「unchanged from before」——即源目录里已经删掉（含敏感的）
    // 文件会作为旧成员留在包里。tar 是截断重写，本不受影响，这里统一处理以保持
    // 两种格式行为一致（也顺手换掉同名目录/软链这类无法直接写入的目标，避免写穿
    // 软链）。不 overwrite 时调用方已用 `test -e` 拦下「目标已存在」，故此处无条件
    // 删除是安全的。
    let remove_target = format!("rm -f -- {target_esc}");

    Ok(format!(
        "{remove_target} && {compress} && echo OK || echo FAILED"
    ))
}

pub(crate) fn has_tool(check_output: &str) -> bool {
    check_output.lines().any(|line| line.trim() == "OK")
}

// Keep the old name as an alias for backward compatibility.
pub(crate) fn has_unzip(check_output: &str) -> bool {
    has_tool(check_output)
}

/// Build a shell command to extract a zip archive to target dir (used by folder upload).
///
/// Returns "OK" on success so the caller can distinguish success from
/// partial/failed extraction.
pub(crate) fn build_extract_cmd(archive_path: &str, target_dir: &str) -> String {
    build_extract_to_dir_cmd(archive_path, target_dir, ArchiveType::Zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unzip_check_command_reports_ok_or_missing() {
        let cmd = build_unzip_check_cmd();
        assert!(cmd.contains("command -v unzip"));
        assert!(cmd.contains("echo OK"));
        assert!(cmd.contains("echo MISSING_UNZIP"));
    }

    #[test]
    fn detects_available_unzip_from_exact_ok_line() {
        assert!(has_unzip("OK\n"));
        assert!(has_unzip("some warning\nOK\n"));
    }

    #[test]
    fn treats_missing_or_ambiguous_output_as_unavailable() {
        assert!(!has_unzip("MISSING_UNZIP\n"));
        assert!(!has_unzip(""));
        assert!(!has_unzip("NOT_OK\n"));
        // 工具检查的判据是「整行恰好 OK」：其它输出里出现的 OK 子串不算命中
        // （解压路径的调用方用的是子串 contains("OK")，见模块外的 sftp.rs）。
        assert!(!has_unzip("CONFLICT: OK.txt\n"));
        assert!(!has_tool("zip warning: OK\n"));
        assert!(has_tool("  OK  \n")); // 前后空白 trim 后仍是整行 OK
    }

    #[test]
    fn tar_check_command_works() {
        let cmd = build_tar_check_cmd();
        assert!(cmd.contains("command -v tar"));
        assert!(has_tool("OK\n"));
        assert!(!has_tool("MISSING_TAR\n"));
    }

    #[test]
    fn detects_archive_types() {
        assert_eq!(get_archive_type("a.zip"), Some(ArchiveType::Zip));
        assert_eq!(get_archive_type("a.tar"), Some(ArchiveType::Tar));
        assert_eq!(get_archive_type("a.tar.gz"), Some(ArchiveType::TarGz));
        assert_eq!(get_archive_type("a.tgz"), Some(ArchiveType::TarGz));
        assert_eq!(get_archive_type("a.tar.bz2"), Some(ArchiveType::TarBz2));
        assert_eq!(get_archive_type("a.tbz2"), Some(ArchiveType::TarBz2));
        assert_eq!(get_archive_type("a.tar.xz"), Some(ArchiveType::TarXz));
        assert_eq!(get_archive_type("a.txz"), Some(ArchiveType::TarXz));
        assert_eq!(get_archive_type("a.txt"), None);
        assert_eq!(get_archive_type("a.gz"), None); // bare .gz is not a tar.gz
        assert_eq!(get_archive_type("a.TAR.GZ"), Some(ArchiveType::TarGz));
    }

    #[test]
    fn build_tar_extract_cmd_uses_correct_flags() {
        let cmd = build_extract_to_dir_cmd("/tmp/a.tar.gz", "/home/user", ArchiveType::TarGz);
        assert!(cmd.contains("tar xzf"));
        assert!(cmd.contains("-C"));
        assert!(cmd.contains("mkdir -p"));
    }

    #[test]
    fn build_extract_cmd_produces_valid_zip_command() {
        let cmd = build_extract_cmd("/tmp/a.zip", "/home/user");
        assert!(cmd.contains("unzip -q"));
        assert!(!cmd.contains("unzip -o"));
        assert!(cmd.contains("CONFLICT"));
        assert!(cmd.contains("cp -a"));
    }

    /// 解压的冲突检测必须扛得住：文件名含换行、目标侧悬空软链、以及检测与复制
    /// 之间的竞态（cp -a -n 兜底）。对全部归档类型都要成立。
    #[test]
    fn extract_cmd_conflict_check_is_robust() {
        for kind in [
            ArchiveType::Zip,
            ArchiveType::TarGz,
            ArchiveType::TarBz2,
            ArchiveType::TarXz,
            ArchiveType::Tar,
        ] {
            let cmd = build_extract_to_dir_cmd("/tmp/a.zip", "/home/user", kind);
            // 文件名可含换行：NUL 分隔遍历，不能按行读
            assert!(cmd.contains("-print0"), "{kind:?}");
            assert!(cmd.contains("read -r -d ''"), "{kind:?}");
            // 悬空软链：-e 跟随链接为假，必须补判 -L，否则冲突漏判（GNU cp 会中途
            // 报错并留下部分拷贝，跟随目标软链的实现会写到目录之外）
            assert!(
                cmd.contains("[ -e '/home/user'/\"$rel\" ] || [ -L '/home/user'/\"$rel\" ]"),
                "{kind:?}"
            );
            // 兜底：检测与复制之间存在竞态窗口，-n 保证绝不覆盖
            assert!(
                cmd.contains("&& cp -a -n \"$tmp\"/. '/home/user'/ && echo OK"),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn zip_check_command_works() {
        let cmd = build_zip_check_cmd();
        assert!(cmd.contains("command -v zip"));
        assert!(has_tool("OK\n"));
        assert!(!has_tool("MISSING_ZIP\n"));
    }

    #[test]
    fn compress_cmd_tar_gz_splits_parent_and_basename() {
        let cmd =
            build_compress_to_archive_cmd("/home/user/foo", "/tmp/foo.tar.gz", ArchiveType::TarGz)
                .unwrap();
        assert!(cmd.contains("tar -czf"));
        assert!(cmd.contains("-C '/home/user'"));
        assert!(cmd.contains("'foo'"));
        assert!(cmd.contains("'/tmp/foo.tar.gz'"));
        assert!(cmd.ends_with("&& echo OK || echo FAILED"));
    }

    #[test]
    fn compress_cmd_zip_uses_cd_and_zip_rqy() {
        let cmd = build_compress_to_archive_cmd("/home/user/foo", "/tmp/foo.zip", ArchiveType::Zip)
            .unwrap();
        assert!(cmd.contains("cd '/home/user'"));
        assert!(cmd.contains("zip -rqy"));
        assert!(cmd.contains("'/tmp/foo.zip'"));
        assert!(cmd.contains("'foo'"));
    }

    /// 覆盖 = 整包重写：zip 是 add/update 语义（不会删掉源目录中已消失的旧成员），
    /// tar 虽为截断重写也统一处理，所以两种格式都必须先删目标再压缩。
    #[test]
    fn compress_cmd_removes_existing_target_before_writing() {
        for (kind, target, tool) in [
            (ArchiveType::TarGz, "/tmp/foo.tar.gz", "tar -czf"),
            (ArchiveType::Zip, "/tmp/foo.zip", "zip -rqy"),
        ] {
            let cmd = build_compress_to_archive_cmd("/home/user/foo", target, kind).unwrap();
            assert!(
                cmd.starts_with(&format!("rm -f -- '{target}' && ")),
                "{kind:?}: {cmd}"
            );
            // 删除必须发生在打包之前，否则旧成员会被 zip 保留下来
            let rm_at = cmd.find("rm -f --").unwrap();
            let write_at = cmd.find(tool).unwrap();
            assert!(rm_at < write_at, "{kind:?}: {cmd}");
        }
    }

    /// 以 '-' 开头的目录名是合法的，但会被 tar/zip 当成选项：两条命令都必须用
    /// `--` 结束选项解析。tar 用 `-C <parent> -- <name>`，zip 的 `--` 必须在
    /// 归档名之后（Info-ZIP zip 3.0 的硬约束）。
    #[test]
    fn compress_cmd_ends_options_before_dirname() {
        let tar =
            build_compress_to_archive_cmd("/home/user/-weird", "/tmp/o.tar.gz", ArchiveType::TarGz)
                .unwrap();
        assert!(tar.contains("-C '/home/user' -- '-weird'"), "{tar}");

        let zip =
            build_compress_to_archive_cmd("/home/user/-weird", "/tmp/o.zip", ArchiveType::Zip)
                .unwrap();
        assert!(zip.contains("zip -rqy '/tmp/o.zip' -- '-weird'"), "{zip}");

        // 普通名字同样带 `--`（位置固定在归档名之后，不随名字变化）
        let plain = build_compress_to_archive_cmd("/home/user/foo", "/tmp/o.zip", ArchiveType::Zip)
            .unwrap();
        assert!(plain.contains("zip -rqy '/tmp/o.zip' -- 'foo'"), "{plain}");
        // `--` 不能出现在归档名之前：zip 会直接报 "can't use -- before archive name"
        assert!(plain.find("-- 'foo'").unwrap() > plain.find("'/tmp/o.zip'").unwrap());
    }

    #[test]
    fn compress_cmd_normalizes_root_parent() {
        // /home (top-level dir) → parent should be "/"
        let cmd =
            build_compress_to_archive_cmd("/home", "/tmp/home.tar.gz", ArchiveType::TarGz).unwrap();
        assert!(cmd.contains("-C '/'"));
        assert!(cmd.contains("'home'"));
    }

    #[test]
    fn compress_cmd_trims_trailing_slash() {
        let cmd =
            build_compress_to_archive_cmd("/home/user/foo/", "/tmp/foo.tar.gz", ArchiveType::TarGz)
                .unwrap();
        // trailing / must not produce empty basename
        assert!(cmd.contains("'foo'"));
        assert!(!cmd.contains("''"));
    }

    #[test]
    fn compress_cmd_rejects_invalid_paths() {
        // empty after trim → root "/"
        assert!(build_compress_to_archive_cmd("/", "/x.tar.gz", ArchiveType::TarGz).is_err());
        // unsupported format
        assert!(
            build_compress_to_archive_cmd("/home/user/foo", "/tmp/foo.tar", ArchiveType::Tar)
                .is_err()
        );
    }

    #[test]
    fn compress_cmd_escapes_special_chars_in_dirname() {
        // dirname with space and quote must be shell-escaped
        let cmd = build_compress_to_archive_cmd(
            "/home/user/my dir",
            "/tmp/out.tar.gz",
            ArchiveType::TarGz,
        )
        .unwrap();
        // 'my dir' is the escaped form of "my dir"
        assert!(cmd.contains("'my dir'"));
    }
}
