//! Spotlight 深扫（`mdfind`）：补充精确路径枚举漏掉的关联残留。
//!
//! 仅作为 `thin uninstall --deep` 的可选补充。相比按固定 token 生成精确候选，
//! Spotlight 能命中「带后缀 / 嵌套」的名字，例如
//! `~/Library/Application Scripts/com.foo.bar.FinderOpen`。
//!
//! 安全约束（宁可漏，不可错）：
//! 1. 仅查询用户主目录，且结果必须落在受信任的用户级根之下
//!    （`~/Library`、`~/.config`、`~/.cache`、`~/.local`）；
//! 2. **文件名（末段）**必须命中强 token（App 名 / bundle id / 条件表 token，长度 >= 5），
//!    而不是整条路径 contains——避免 `Documents/x/chrome.rs` 之类误报；
//! 3. 跳过受保护路径与废纸篓；
//! 4. 结果数量设上限，且 `mdfind` 带超时。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 单次深扫最大结果数（防止极端情况下爆量）
const MAX_RESULTS: usize = 400;

/// `mdfind` 超时
const TIMEOUT: Duration = Duration::from_secs(20);

/// 受信任的用户级根（相对 home 的第一段）
const TRUSTED_ROOTS: &[&str] = &["Library", ".config", ".cache", ".local"];

/// 归一化：只保留 ASCII 字母数字并小写（与 `apps::normalize_token` 语义一致）
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 构造 `mdfind` 谓词：命中任一 term 的文件名（大小写不敏感）。
///
/// 过滤掉过短或含查询元字符的 term；无有效 term 时返回 `None`。
fn predicate(terms: &[String]) -> Option<String> {
    let clauses: Vec<String> = terms
        .iter()
        .map(|t| t.trim())
        .filter(|t| norm(t).len() >= 5)
        .filter(|t| !t.contains(['\'', '"', '*', '?', '\\', '(', ')']))
        .map(|t| format!("kMDItemFSName == '*{t}*'c"))
        .collect();
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" || "))
    }
}

/// 路径是否落在受信任的用户级根之下
fn is_trusted(home: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(home) else {
        return false;
    };
    match rel.components().next() {
        Some(first) => TRUSTED_ROOTS.contains(&first.as_os_str().to_string_lossy().as_ref()),
        None => false,
    }
}

/// 文件名（末段）是否命中任一强 token
fn name_matches(path: &Path, strong: &[String]) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let n = norm(&name.to_string_lossy());
    strong.iter().any(|t| n.contains(t))
}

/// 在用户主目录下做一次 Spotlight 深扫，返回通过安全过滤的候选路径。
///
/// `terms` 由调用方提供（App 名 / bundle id / 条件表 token）。后端不可用、
/// 超时、查询失败或 Spotlight 未索引时均返回空 `Vec`（调用方按「没有补充」处理）。
pub fn search(home: &Path, terms: &[String]) -> Vec<PathBuf> {
    let Some(pred) = predicate(terms) else {
        return Vec::new();
    };
    let home_s = home.to_string_lossy().into_owned();
    let Some(out) = crate::proc::output_with_timeout(
        "mdfind",
        &["-onlyin", home_s.as_str(), pred.as_str()],
        TIMEOUT,
    ) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }

    let strong: Vec<String> = terms
        .iter()
        .map(|t| norm(t))
        .filter(|t| t.len() >= 5)
        .collect();

    let mut found: Vec<PathBuf> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let p = PathBuf::from(line);
        if !is_trusted(home, &p) {
            continue;
        }
        if p.components().any(|c| c.as_os_str() == ".Trash") {
            continue;
        }
        if !name_matches(&p, &strong) {
            continue;
        }
        if crate::clean::static_protection_reason(&p).is_some() {
            continue;
        }
        found.push(p);
        if found.len() >= MAX_RESULTS {
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_builds_or_query_and_skips_weak_terms() {
        let p = predicate(&[
            "Google Chrome".to_string(),
            "com.google.Chrome".to_string(),
            "ab".to_string(),
            "bad'quote".to_string(),
        ])
        .unwrap();
        assert!(p.contains("'*Google Chrome*'c"));
        assert!(p.contains("'*com.google.Chrome*'c"));
        assert!(!p.contains("ab"));
        assert!(!p.contains("bad"));
    }

    #[test]
    fn predicate_none_without_strong_terms() {
        assert!(predicate(&["ab".to_string()]).is_none());
        assert!(predicate(&[]).is_none());
    }

    #[test]
    fn only_trusted_roots_are_accepted() {
        let home = Path::new("/Users/x");
        assert!(is_trusted(
            home,
            Path::new("/Users/x/Library/Application Support/Foo")
        ));
        assert!(is_trusted(home, Path::new("/Users/x/.config/foo")));
        // 个人目录（Documents）不进入深扫
        assert!(!is_trusted(home, Path::new("/Users/x/Documents/foo")));
        assert!(!is_trusted(home, Path::new("/Users/x/Desktop/foo")));
        assert!(!is_trusted(home, Path::new("/Library/Foo")));
    }

    #[test]
    fn name_must_match_strong_token() {
        let strong = vec!["googlechrome".to_string()];
        assert!(name_matches(
            Path::new("/Users/x/Library/Google Chrome Brand.plist"),
            &strong
        ));
        // 只含末段 "chrome" 的无关文件不匹配
        assert!(!name_matches(
            Path::new("/Users/x/Library/Application Support/Code/CachedData/x/chrome"),
            &strong
        ));
        assert!(!name_matches(
            Path::new("/Users/x/Documents/chrome.rs"),
            &strong
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn search_never_returns_untrusted_paths() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        let terms = vec!["com.apple.Safari".to_string(), "Safari".to_string()];
        for p in search(&home, &terms) {
            assert!(
                is_trusted(&home, &p),
                "深扫返回了非受信任路径: {}",
                p.display()
            );
            assert!(
                crate::clean::static_protection_reason(&p).is_none(),
                "深扫返回了受保护路径: {}",
                p.display()
            );
        }
    }
}
