use std::io::Read;

use zip::ZipArchive;

use crate::skills::store::SkillResource;

/// 附属资源文件上限（只收 `.md` 文本）。skill 内容是给模型读的指令，
/// 单个参考文档再大也不该超过 1MB；超限说明导入的不是 skill 包。
pub const MAX_RESOURCE_BYTES: usize = 1024 * 1024;
/// 附属资源合计上限。
pub const MAX_TOTAL_RESOURCE_BYTES: usize = 4 * 1024 * 1024;
/// 附属资源数量上限。
pub const MAX_RESOURCE_COUNT: usize = 100;

/// Result of parsing a skill file package.
///
/// `prompt` 是入口（`SKILL.md` 正文），`resources` 是同一 skill 根目录下的
/// 其它 `.md`（相对根目录的路径）。没有资源的 skill 序列化时不带该字段，
/// 与旧前端保持兼容。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedSkill {
    pub name: String,
    pub description: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<SkillResource>,
}

/// 校验附属资源路径与体积（导入与命令层回传统一把关）。
///
/// 只接受相对路径 + `.md`，拒绝绝对路径与 `..` / `.` / 空片段。交付侧只做
/// 白名单等值查找、本不存在穿越，但存进 `skills.json` 前仍要挡住畸形路径。
pub fn validate_resources(resources: &[SkillResource]) -> Result<(), String> {
    if resources.len() > MAX_RESOURCE_COUNT {
        return Err(format!("资源文件数量超过上限（{} 个）", MAX_RESOURCE_COUNT));
    }
    let mut total = 0usize;
    let mut seen = std::collections::HashSet::new();
    for r in resources {
        if !seen.insert(r.path.as_str()) {
            return Err(format!("资源路径重复: {}", r.path));
        }
        let p = r.path.replace('\\', "/");
        if p.starts_with('/') {
            return Err(format!("资源路径必须是相对路径: {}", r.path));
        }
        if p.split('/')
            .any(|seg| seg.is_empty() || seg == ".." || seg == ".")
        {
            return Err(format!("资源路径含非法片段: {}", r.path));
        }
        if !p.to_lowercase().ends_with(".md") {
            return Err(format!("只支持 .md 资源文件: {}", r.path));
        }
        // 根目录的 SKILL.md 是入口（正文存在 prompt 里），资源再叫这个名字会和入口混淆
        if p.eq_ignore_ascii_case("SKILL.md") {
            return Err("资源不能命名为 SKILL.md（它是 skill 的入口文件）".to_string());
        }
        if r.content.len() > MAX_RESOURCE_BYTES {
            return Err(format!(
                "资源文件 {} 超过单文件上限（{} KB）",
                r.path,
                MAX_RESOURCE_BYTES / 1024
            ));
        }
        total += r.content.len();
        if total > MAX_TOTAL_RESOURCE_BYTES {
            return Err(format!(
                "资源文件合计超过上限（{} KB）",
                MAX_TOTAL_RESOURCE_BYTES / 1024
            ));
        }
    }
    Ok(())
}

fn parse_yaml_frontmatter(content: &str) -> Result<ParsedSkill, String> {
    let content = content.trim();

    // Strip UTF-8 BOM if present
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    // 分隔符必须是「独占一行的 ---」：description 里写 `a --- b`、YAML 多行
    // 字符串里出现横线都不能被当成结束符。
    let mut lines = content.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_end() != "---" {
        return Err("SKILL.md must start with --- YAML frontmatter delimiter".into());
    }
    let mut offset = first.len();
    let mut close: Option<(usize, usize)> = None; // (yaml 结束, 正文开始)
    for line in lines {
        if line.trim_end() == "---" {
            close = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let (yaml_end, body_start) = close.ok_or("Missing closing --- after YAML frontmatter")?;

    let yaml_block = &content[first.len()..yaml_end].trim();
    let markdown_body = content[body_start..].trim();

    // Parse YAML
    let data: serde_yaml::Value =
        serde_yaml::from_str(yaml_block).map_err(|e| format!("YAML 解析失败: {}", e))?;

    let name = data
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("YAML frontmatter 缺少 name 字段")?
        .trim()
        .to_string();

    if name.is_empty() {
        return Err("name 字段不能为空".into());
    }

    let description = data
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // The rest of the markdown body becomes the prompt
    let prompt = if markdown_body.is_empty() {
        return Err("SKILL.md frontmatter 后缺少 prompt 内容".into());
    } else {
        markdown_body.to_string()
    };

    Ok(ParsedSkill {
        name,
        description,
        prompt,
        resources: Vec::new(),
    })
}

/// Parse a single .md file directly (no sibling resources).
pub(crate) fn process_md(content: &str, file_name: &str) -> Result<ParsedSkill, String> {
    let skill_name = file_name
        .trim_end_matches(".md")
        .trim_end_matches(".MD")
        .trim_end_matches(".Md");
    parse_yaml_frontmatter(content).map(|mut p| {
        if p.name.is_empty() {
            p.name = skill_name.to_string();
        }
        p
    })
}

/// 压缩包内的一个 `.md` 条目。
#[derive(Clone)]
pub(crate) struct Entry {
    /// 归档内的完整相对路径（如 `repo-main/skills/references/x.md`）。
    path: String,
    bytes: Vec<u8>,
}

/// 目录分隔符统一成 `/`（zip 规范用 `/`，但防御性处理）。
fn normalize_path(name: &str) -> String {
    name.replace('\\', "/")
}

/// 是否应跳过的归档条目：目录、隐藏文件、macOS 元数据。
fn is_skippable(path: &str) -> bool {
    path.ends_with('/')
        || path.starts_with('.')
        || path.contains("/.")
        || path.starts_with("__MACOSX/")
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

fn is_skill_md(path: &str) -> bool {
    basename(path).eq_ignore_ascii_case("SKILL.md")
}

/// 收集压缩包内所有 `.md` 条目（跳过目录、隐藏项与 macOS 元数据）。
pub(crate) fn collect_md_entries(data: &[u8]) -> Result<Vec<Entry>, String> {
    let reader = std::io::Cursor::new(data);
    let mut archive = ZipArchive::new(reader).map_err(|e| format!("zip 解析失败: {}", e))?;

    let mut entries = Vec::new();
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("读取 zip 内文件失败: {}", e))?;
        let path = normalize_path(file.name());
        if is_skippable(&path) || !path.to_lowercase().ends_with(".md") {
            continue;
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| format!("读取 zip 内文件失败: {}", e))?;
        entries.push(Entry { path, bytes: buf });
    }
    Ok(entries)
}

/// 从一组 `.md` 条目构建 ParsedSkill。
///
/// `root` = 指定的 skill 根目录前缀（相对归档根的路径，如 `skills` 或
/// `repo-main/skills`）；`None` = 自动识别（以唯一的 `SKILL.md` 所在目录为根，
/// 找不到时退回「包内只有一个 .md」的旧行为）。
fn build_from_entries(entries: Vec<Entry>, root: Option<&str>) -> Result<ParsedSkill, String> {
    let prefix = root.map(|r| r.trim_matches('/').to_string());

    // 1) 定位入口 SKILL.md
    let entry_path = match &prefix {
        Some(p) => {
            // 指定了根：优先取根目录正下方的 SKILL.md，否则退回根目录正下方的
            // 唯一 .md（兼容把单个 md 放在子目录里的包）。
            let direct: Vec<&Entry> = entries
                .iter()
                .filter(|e| parent_dir(&e.path) == p)
                .collect();
            direct
                .iter()
                .find(|e| is_skill_md(&e.path))
                .map(|e| e.path.clone())
                .or_else(|| {
                    if direct.len() == 1 {
                        Some(direct[0].path.clone())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| format!("指定目录 {} 下未找到 SKILL.md", p))?
        }
        None => {
            let skill_mds: Vec<&Entry> = entries.iter().filter(|e| is_skill_md(&e.path)).collect();
            match skill_mds.len() {
                1 => skill_mds[0].path.clone(),
                0 => {
                    if entries.len() == 1 {
                        // 兼容旧行为：包内只有一个 .md，就把它当入口。
                        entries[0].path.clone()
                    } else if entries.is_empty() {
                        return Err("压缩包内未找到 .md 文件".into());
                    } else {
                        let names: Vec<_> = entries.iter().map(|e| e.path.clone()).collect();
                        return Err(format!(
                            "压缩包内未找到 SKILL.md（含 {} 个 .md：{}）。请放入 SKILL.md 作为入口，或指定 skill 目录。",
                            names.len(),
                            names.join(", ")
                        ));
                    }
                }
                n => {
                    let names: Vec<_> = skill_mds.iter().map(|e| e.path.clone()).collect();
                    return Err(format!(
                        "压缩包内包含 {} 个 SKILL.md，请只保留一个：{}",
                        n,
                        names.join(", ")
                    ));
                }
            }
        }
    };

    // 2) skill 根 = 入口所在目录
    let skill_root = parent_dir(&entry_path).to_string();

    // 3) 解析入口
    let entry = entries
        .iter()
        .find(|e| e.path == entry_path)
        .expect("入口条目必然存在");
    let content = String::from_utf8(entry.bytes.clone())
        .map_err(|e| format!("{} 不是有效的 UTF-8 文本: {}", entry_path, e))?;
    let mut parsed = process_md(&content, basename(&entry_path))?;

    // 4) 根目录下的其它 .md → 附属资源（相对根的路径）
    let root_prefix = if skill_root.is_empty() {
        String::new()
    } else {
        format!("{}/", skill_root)
    };
    // 子目录里若还有别的 SKILL.md，那一整棵子树属于另一个 skill，不是本 skill 的资源
    // （否则外层与内层 skill 会各带一份同样的文件）。
    let nested_roots: Vec<String> = entries
        .iter()
        .filter(|e| is_skill_md(&e.path) && e.path != entry_path)
        .map(|e| format!("{}/", parent_dir(&e.path)))
        .filter(|d| d != "/" && d.starts_with(&root_prefix) && *d != root_prefix)
        .collect();
    let mut resources: Vec<SkillResource> = Vec::new();
    let mut total: usize = 0;
    for e in entries.iter() {
        if e.path == entry_path {
            continue;
        }
        if nested_roots.iter().any(|d| e.path.starts_with(d.as_str())) {
            continue;
        }
        // 只收入口所在目录及其子目录下的文件
        if !root_prefix.is_empty() && !e.path.starts_with(&root_prefix) {
            continue;
        }
        let rel = if root_prefix.is_empty() {
            e.path.clone()
        } else {
            e.path[root_prefix.len()..].to_string()
        };
        if rel.is_empty() {
            continue;
        }
        if e.bytes.len() > MAX_RESOURCE_BYTES {
            return Err(format!(
                "资源文件 {} 超过单文件上限（{} KB）",
                rel,
                MAX_RESOURCE_BYTES / 1024
            ));
        }
        if resources.len() >= MAX_RESOURCE_COUNT {
            return Err(format!("资源文件数量超过上限（{} 个）", MAX_RESOURCE_COUNT));
        }
        total += e.bytes.len();
        if total > MAX_TOTAL_RESOURCE_BYTES {
            return Err(format!(
                "资源文件合计超过上限（{} KB）",
                MAX_TOTAL_RESOURCE_BYTES / 1024
            ));
        }
        let text = String::from_utf8(e.bytes.clone())
            .map_err(|_| format!("资源文件 {} 不是有效的 UTF-8 文本", rel))?;
        resources.push(SkillResource {
            path: rel,
            content: text,
        });
    }
    // 稳定顺序：按路径字典序，保证工具返回的清单与测试可预期
    resources.sort_by(|a, b| a.path.cmp(&b.path));

    parsed.resources = resources;
    Ok(parsed)
}

/// 自动识别 skill 根：解压后以唯一的 `SKILL.md` 为锚，同目录其它 `.md` 作资源。
pub(crate) fn process_zip(data: &[u8]) -> Result<ParsedSkill, String> {
    let entries = collect_md_entries(data)?;
    build_from_entries(entries, None)
}

// ─── 仓库 archive（GitHub 下载的 `{repo}-{branch}.zip`）─────────────────────

/// 仓库 archive 的顶层包装目录名。GitHub 生成的归档把所有内容包在
/// `{repo}-{branch}/` 下，剥掉它之后子路径才与 URL 里的路径对齐。
fn archive_top_dir(entries: &[Entry]) -> Result<String, String> {
    entries
        .first()
        .map(|e| e.path.split('/').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "压缩包内未找到 .md 文件".to_string())
}

/// 一个从仓库里发现的 skill（供前端多选列表展示）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSkill {
    /// 相对仓库根的目录（如 `skills`、`skills/foo`）。
    pub subpath: String,
    pub name: String,
    pub description: String,
    pub resource_count: usize,
}

/// 扫描仓库 archive，列出所有含 `SKILL.md` 的目录。
///
/// 只读一遍归档：先按 `SKILL.md` 定位根，再对每个根复用同批条目构建
/// ParsedSkill（用于拿 name / description / 资源数量）。
pub(crate) fn discover_repo_skills(data: &[u8]) -> Result<Vec<DiscoveredSkill>, String> {
    let entries = collect_md_entries(data)?;
    let top = archive_top_dir(&entries)?;
    let top_prefix = format!("{}/", top);

    // 仓库根相对的 skill 根目录（去重 + 排序，保证清单稳定）
    let mut roots: Vec<String> = Vec::new();
    for e in entries.iter() {
        if !is_skill_md(&e.path) {
            continue;
        }
        let Some(rel) = e.path.strip_prefix(&top_prefix) else {
            continue;
        };
        let dir = parent_dir(rel).to_string();
        if !roots.contains(&dir) {
            roots.push(dir);
        }
    }
    roots.sort();

    let mut out = Vec::new();
    for dir in roots {
        let full_root = if dir.is_empty() {
            top.clone()
        } else {
            format!("{}/{}", top, dir)
        };
        let parsed = build_from_entries(entries.clone(), Some(&full_root))?;
        out.push(DiscoveredSkill {
            subpath: dir,
            name: parsed.name,
            description: parsed.description,
            resource_count: parsed.resources.len(),
        });
    }
    if out.is_empty() {
        return Err("仓库内未找到任何 SKILL.md".into());
    }
    Ok(out)
}

/// 从仓库 archive 里解析指定子路径（相对仓库根）的 skill。
pub(crate) fn process_repo_archive(data: &[u8], subpath: &str) -> Result<ParsedSkill, String> {
    let entries = collect_md_entries(data)?;
    let top = archive_top_dir(&entries)?;
    let sub = subpath.trim_matches('/');
    let root = if sub.is_empty() {
        top
    } else {
        format!("{}/{}", top, sub)
    };
    build_from_entries(entries, Some(&root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = SimpleFileOptions::default();
            for (name, content) in files {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(content.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        buf
    }

    const ENTRY: &str = "---\nname: demo\ndescription: d\n---\n\n# Demo\n\n正文";

    #[test]
    fn single_md_without_skill_md_still_works() {
        // 兼容旧行为：包内只有一个 md、没有 SKILL.md
        let data = zip_of(&[("my-skill.md", ENTRY)]);
        let parsed = process_zip(&data).unwrap();
        assert_eq!(parsed.name, "demo");
        assert!(parsed.resources.is_empty());
    }

    #[test]
    fn skill_md_anchor_collects_sibling_md_as_resources() {
        let data = zip_of(&[
            ("skills/SKILL.md", ENTRY),
            ("skills/references/triggers.md", "# triggers"),
            ("skills/references/actions.md", "# actions"),
        ]);
        let parsed = process_zip(&data).unwrap();
        assert_eq!(parsed.name, "demo");
        assert_eq!(parsed.prompt, "# Demo\n\n正文");
        let paths: Vec<_> = parsed.resources.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["references/actions.md", "references/triggers.md"]
        );
    }

    #[test]
    fn non_md_files_are_ignored() {
        let data = zip_of(&[
            ("SKILL.md", ENTRY),
            ("references/x.md", "# x"),
            ("scripts/run.sh", "echo hi"),
            ("README.txt", "hi"),
        ]);
        let parsed = process_zip(&data).unwrap();
        assert_eq!(parsed.resources.len(), 1);
        assert_eq!(parsed.resources[0].path, "references/x.md");
    }

    #[test]
    fn hidden_and_macos_entries_are_skipped() {
        let data = zip_of(&[
            ("SKILL.md", ENTRY),
            (".DS_Store", "junk"),
            ("__MACOSX/._SKILL.md", "junk"),
            ("references/.hidden.md", "# hidden"),
        ]);
        let parsed = process_zip(&data).unwrap();
        assert!(parsed.resources.is_empty());
    }

    #[test]
    fn multiple_skill_md_is_rejected_with_names() {
        let data = zip_of(&[("a/SKILL.md", ENTRY), ("b/SKILL.md", ENTRY)]);
        let err = process_zip(&data).unwrap_err();
        assert!(err.contains("2 个 SKILL.md"), "got: {err}");
    }

    #[test]
    fn multiple_md_without_skill_md_is_rejected() {
        let data = zip_of(&[("a.md", ENTRY), ("b.md", ENTRY)]);
        let err = process_zip(&data).unwrap_err();
        assert!(err.contains("未找到 SKILL.md"), "got: {err}");
    }

    #[test]
    fn oversized_resource_is_rejected() {
        let big = "x".repeat(MAX_RESOURCE_BYTES + 1);
        let data = zip_of(&[("SKILL.md", ENTRY), ("references/big.md", &big)]);
        let err = process_zip(&data).unwrap_err();
        assert!(err.contains("超过单文件上限"), "got: {err}");
    }

    // ── 仓库 archive（带 {repo}-{branch} 包装）──

    const ENTRY_B: &str = "---\nname: second\ndescription: e\n---\n\n# Second\n\n正文";

    #[test]
    fn repo_archive_discovery_lists_all_skills() {
        let data = zip_of(&[
            ("repo-main/skills/SKILL.md", ENTRY),
            ("repo-main/skills/references/a.md", "# a"),
            ("repo-main/other/SKILL.md", ENTRY_B),
            ("repo-main/rule/x.txt", "not md"),
        ]);
        let found = discover_repo_skills(&data).unwrap();
        let dirs: Vec<_> = found.iter().map(|d| d.subpath.as_str()).collect();
        assert_eq!(dirs, vec!["other", "skills"]);
        let skills = found.iter().find(|d| d.subpath == "skills").unwrap();
        assert_eq!(skills.name, "demo");
        assert_eq!(skills.resource_count, 1);
    }

    #[test]
    fn repo_archive_parses_specified_subpath() {
        let data = zip_of(&[
            ("repo-main/skills/SKILL.md", ENTRY),
            ("repo-main/skills/references/a.md", "# a"),
            ("repo-main/other/SKILL.md", ENTRY_B),
        ]);
        let parsed = process_repo_archive(&data, "skills").unwrap();
        assert_eq!(parsed.name, "demo");
        assert_eq!(parsed.resources.len(), 1);
        assert_eq!(parsed.resources[0].path, "references/a.md");
    }

    #[test]
    fn repo_archive_without_skill_md_reports_error() {
        let data = zip_of(&[("repo-main/README.md", "# hi")]);
        let err = discover_repo_skills(&data).unwrap_err();
        assert!(err.contains("未找到任何 SKILL.md"), "got: {err}");
    }

    #[test]
    fn nested_skill_subtree_is_not_a_resource_of_outer_skill() {
        let data = zip_of(&[
            ("repo-main/SKILL.md", ENTRY),
            ("repo-main/references/a.md", "# a"),
            ("repo-main/inner/SKILL.md", ENTRY_B),
            ("repo-main/inner/references/b.md", "# b"),
        ]);
        let outer = process_repo_archive(&data, "").unwrap();
        let paths: Vec<_> = outer.resources.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["references/a.md"]);
        let inner = process_repo_archive(&data, "inner").unwrap();
        let paths: Vec<_> = inner.resources.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["references/b.md"]);
    }

    #[test]
    fn frontmatter_delimiter_must_be_on_its_own_line() {
        let md = "---\nname: x\ndescription: a --- b\n---\n\n正文 --- 还在";
        let parsed = process_md(md, "x.md").unwrap();
        assert_eq!(parsed.description, "a --- b");
        assert_eq!(parsed.prompt, "正文 --- 还在");
        // CRLF 与结尾分隔符后无换行
        let md = "---\r\nname: y\r\n---\r\nbody";
        assert_eq!(process_md(md, "y.md").unwrap().prompt, "body");
        // 没有独占一行的结束符
        assert!(process_md("---\nname: z\ndescription: a --- b\n", "z.md").is_err());
    }

    #[test]
    fn validate_resources_rejects_duplicate_paths() {
        let r = |p: &str| SkillResource {
            path: p.into(),
            content: "x".into(),
        };
        assert!(validate_resources(&[r("a.md"), r("a.md")]).is_err());
        assert!(validate_resources(&[r("a.md"), r("b.md")]).is_ok());
    }

    #[test]
    fn validate_resources_rejects_bad_paths() {
        let ok = |path: &str| {
            vec![SkillResource {
                path: path.to_string(),
                content: "# x".into(),
            }]
        };
        assert!(validate_resources(&ok("references/a.md")).is_ok());
        assert!(validate_resources(&ok("../escape.md")).is_err());
        assert!(validate_resources(&ok("/abs.md")).is_err());
        assert!(validate_resources(&ok("a//b.md")).is_err());
        assert!(validate_resources(&ok("a/b.txt")).is_err());
        // 根目录的 SKILL.md 是入口，不能当资源；子目录里同名不受影响
        assert!(validate_resources(&ok("SKILL.md")).is_err());
        assert!(validate_resources(&ok("skill.MD")).is_err());
        assert!(validate_resources(&ok("references/SKILL.md")).is_ok());
        assert!(validate_resources(&[]).is_ok());
    }
}
