//! 从 GitHub 地址引入 skill —— 纯解析部分。
//!
//! 这里只做 URL → (owner, repo, 子路径) 的解析，不碰网络；下载与归档抽取由
//! 命令层复用 `commands::market`（镜像 / 代理兜底 / 大小上限）与
//! `skills::parser`（仓库 archive 的子树解析）完成。拆开是为了让解析逻辑
//! 可以脱离网络单测。

use crate::error::AppError;

/// 用户贴的 GitHub 地址解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSource {
    pub owner: String,
    pub repo: String,
    /// URL 里显式写的分支/标签（`tree` / `blob` 后的第一段）。
    /// `None` = 未指定，按 `main` / `master` 依次尝试。
    pub branch: Option<String>,
    /// 指向仓库内的子路径（相对仓库根，如 `skills`、`skills/foo`）。
    /// `None` = 仓库根 —— 此时进入「发现模式」，先列出仓库内所有 skill 让用户挑。
    pub subpath: Option<String>,
}

impl SkillSource {
    /// `owner/repo`，用于展示与去重。
    pub fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// 解析 GitHub 地址。
///
/// 支持三种形态：
/// - `https://github.com/{owner}/{repo}` —— 仓库根（发现模式）
/// - `https://github.com/{owner}/{repo}/tree/{ref}[/{path...}]` —— 子目录
/// - `https://github.com/{owner}/{repo}/blob/{ref}/{path...}` —— 单个文件
///   （取该文件所在目录作为 skill 根）
///
/// `{ref}` 按第一段解析（分支名含 `/` 的情况不支持 —— 这类分支极少，且发现
/// 模式完全不依赖 `ref`：归档下载与子树定位都靠 archive 自身的包装目录）。
pub fn parse_skill_url(raw: &str) -> Result<SkillSource, AppError> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(AppError::Config("请填写 GitHub 地址".into()));
    }
    // 去掉 query / fragment，再去掉尾部斜杠
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let url = url.trim_end_matches('/');
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("github.com/"))
        .ok_or_else(|| {
            AppError::Config("仅支持 github.com 的地址（如 https://github.com/owner/repo）".into())
        })?;

    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let owner = parts
        .next()
        .ok_or_else(|| AppError::Config("地址缺少仓库所有者".into()))?
        .to_string();
    let repo = parts
        .next()
        .ok_or_else(|| AppError::Config("地址缺少仓库名".into()))?
        .trim_end_matches(".git")
        .to_string();
    if repo.is_empty() {
        return Err(AppError::Config("地址缺少仓库名".into()));
    }

    let subpath;
    let mut branch = None;
    match parts.next() {
        None => subpath = None,
        Some("tree") => {
            branch = parts
                .next()
                .ok_or_else(|| AppError::Config("tree 地址缺少分支名".into()))?
                .to_string()
                .into();
            let path: Vec<&str> = parts.collect();
            subpath = if path.is_empty() {
                None
            } else {
                Some(path.join("/"))
            };
        }
        Some("blob") => {
            branch = parts
                .next()
                .ok_or_else(|| AppError::Config("blob 地址缺少分支名".into()))?
                .to_string()
                .into();
            let path: Vec<&str> = parts.collect();
            if path.is_empty() {
                return Err(AppError::Config("blob 地址缺少文件路径".into()));
            }
            // 去掉文件名，取所在目录；文件直接在仓库根时根就是仓库根
            let dir = &path[..path.len() - 1];
            subpath = if dir.is_empty() {
                None
            } else {
                Some(dir.join("/"))
            };
        }
        Some(other) => {
            return Err(AppError::Config(format!(
                "不支持的地址形式（/{other}）：请贴仓库地址、tree 目录地址或 blob 文件地址"
            )));
        }
    }

    Ok(SkillSource {
        owner,
        repo,
        branch,
        subpath,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_root_is_discovery_mode() {
        let s = parse_skill_url("https://github.com/ShortX-Repo/ShortX-Files").unwrap();
        assert_eq!(s.owner, "ShortX-Repo");
        assert_eq!(s.repo, "ShortX-Files");
        assert_eq!(s.subpath, None);
    }

    #[test]
    fn trailing_slash_and_git_suffix_are_tolerated() {
        let s = parse_skill_url("https://github.com/o/r/").unwrap();
        assert_eq!(s.subpath, None);
        let s = parse_skill_url("https://github.com/o/r.git").unwrap();
        assert_eq!(s.repo, "r");
    }

    #[test]
    fn tree_url_yields_subpath() {
        let s = parse_skill_url("https://github.com/ShortX-Repo/ShortX-Files/tree/main/skills")
            .unwrap();
        assert_eq!(s.subpath.as_deref(), Some("skills"));
    }

    #[test]
    fn tree_url_with_nested_path() {
        let s = parse_skill_url("https://github.com/o/r/tree/main/a/b/c").unwrap();
        assert_eq!(s.subpath.as_deref(), Some("a/b/c"));
    }

    #[test]
    fn tree_url_without_path_is_root() {
        let s = parse_skill_url("https://github.com/o/r/tree/main").unwrap();
        assert_eq!(s.subpath, None);
    }

    #[test]
    fn blob_url_takes_parent_directory() {
        let s = parse_skill_url("https://github.com/o/r/blob/main/skills/SKILL.md").unwrap();
        assert_eq!(s.subpath.as_deref(), Some("skills"));
    }

    #[test]
    fn blob_url_at_repo_root_is_root() {
        let s = parse_skill_url("https://github.com/o/r/blob/main/SKILL.md").unwrap();
        assert_eq!(s.subpath, None);
    }

    #[test]
    fn query_and_fragment_are_stripped() {
        let s = parse_skill_url("https://github.com/o/r/tree/main/skills?tab=readme#x").unwrap();
        assert_eq!(s.subpath.as_deref(), Some("skills"));
    }

    #[test]
    fn branch_is_captured_when_present() {
        let s = parse_skill_url("https://github.com/o/r/tree/dev/skills").unwrap();
        assert_eq!(s.branch.as_deref(), Some("dev"));
        assert_eq!(s.subpath.as_deref(), Some("skills"));
        let s = parse_skill_url("https://github.com/o/r").unwrap();
        assert_eq!(s.branch, None);
    }

    #[test]
    fn non_github_url_is_rejected() {
        let err = parse_skill_url("https://gitlab.com/o/r").unwrap_err();
        assert!(format!("{err}").contains("github.com"), "got: {err}");
    }

    #[test]
    fn unsupported_path_form_is_rejected() {
        let err = parse_skill_url("https://github.com/o/r/issues/3").unwrap_err();
        assert!(format!("{err}").contains("不支持的地址形式"), "got: {err}");
    }

    #[test]
    fn empty_url_is_rejected() {
        assert!(parse_skill_url("   ").is_err());
    }
}
