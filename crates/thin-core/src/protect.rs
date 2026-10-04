//! 运行期保护名单：被标记的路径**及其子树**永不清理，也不计入可回收。
//!
//! 典型用法：正在开发的 Rust/Node 项目，`target/` 会被内置规则命中；
//! `thin protect add .` 后，该项目的构建产物不再被 `clean` 搬走，
//! 避免每次 `cargo install` 全量重编。
//!
//! 名单持久化在 `~/.thin/protected.json`（可用 `THIN_HOME` 覆盖），
//! 与规则解耦——保护是「这次不清理」，不是「删掉规则」。

use crate::clean::thin_home;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Protected {
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

fn path_in(home: &Path) -> PathBuf {
    home.join("protected.json")
}

/// 读取保护名单（文件缺失或损坏时视为空名单）。
pub fn load() -> Vec<PathBuf> {
    load_in(&thin_home())
}

pub fn load_in(home: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(path_in(home))
        .ok()
        .and_then(|s| serde_json::from_str::<Protected>(&s).ok())
        .map(|p| p.paths)
        .unwrap_or_default()
}

/// 目标路径是否被保护（精确匹配或位于某保护目录之内）。
pub fn is_protected(path: &Path) -> bool {
    is_protected_in(&thin_home(), path)
}

pub fn is_protected_in(home: &Path, path: &Path) -> bool {
    matches(&load_in(home), path)
}

/// 在已加载的名单上判断，避免逐项重复读盘（供 scan 批量使用）。
pub fn matches(list: &[PathBuf], path: &Path) -> bool {
    let canon = soft_canon(path);
    list.iter().any(|p| canon == *p || canon.starts_with(p))
}

/// 加入保护名单，返回规范化后的绝对路径。重复加入是幂等的。
pub fn add(path: &Path) -> Result<PathBuf> {
    add_in(&thin_home(), path)
}

pub fn add_in(home: &Path, path: &Path) -> Result<PathBuf> {
    let canon = path
        .canonicalize()
        .with_context(|| format!("路径不存在: {}", path.display()))?;
    if canon == Path::new("/") {
        bail!("不能保护根目录");
    }
    let home_dir = std::env::var("HOME")
        .ok()
        .and_then(|h| PathBuf::from(h).canonicalize().ok());
    if home_dir.as_deref() == Some(canon.as_path()) {
        bail!("不能保护整个主目录（会使 thin 失去意义）");
    }

    let mut list = load_in(home);
    if !list.contains(&canon) {
        list.push(canon.clone());
        save_in(home, &list)?;
    }
    Ok(canon)
}

/// 从保护名单移除；`path` 必须与名单中的条目一致（会做规范化）。
pub fn remove(path: &Path) -> Result<bool> {
    remove_in(&thin_home(), path)
}

pub fn remove_in(home: &Path, path: &Path) -> Result<bool> {
    let canon = soft_canon(path);
    let mut list = load_in(home);
    let before = list.len();
    list.retain(|p| *p != canon);
    let removed = list.len() != before;
    if removed {
        save_in(home, &list)?;
    }
    Ok(removed)
}

pub fn save_in(home: &Path, list: &[PathBuf]) -> Result<PathBuf> {
    std::fs::create_dir_all(home).context("创建 thin 数据目录失败")?;
    let p = path_in(home);
    let data = serde_json::to_string_pretty(&Protected {
        paths: list.to_vec(),
    })?;
    std::fs::write(&p, data).with_context(|| format!("写入保护名单失败: {}", p.display()))?;
    Ok(p)
}

fn soft_canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("thin-protect-{}-{tag}", std::process::id()))
    }

    #[test]
    fn add_remove_and_match_subtree() {
        let home = temp_home("a");
        let _ = std::fs::remove_dir_all(&home);
        let proj = home.join("proj");
        let target = proj.join("target");
        std::fs::create_dir_all(&target).unwrap();

        // 未保护
        assert!(!is_protected_in(&home, &target));

        // 保护项目根 → 子树 target 一并受保护
        let added = add_in(&home, &proj).unwrap();
        assert_eq!(added, proj.canonicalize().unwrap());
        assert!(is_protected_in(&home, &target));
        assert!(is_protected_in(&home, &proj));
        // 同名但不同路径不受影响
        let other = home.join("other-target");
        std::fs::create_dir_all(&other).unwrap();
        assert!(!is_protected_in(&home, &other));

        // 幂等
        add_in(&home, &proj).unwrap();
        assert_eq!(load_in(&home).len(), 1);

        // 移除后失效
        assert!(remove_in(&home, &proj).unwrap());
        assert!(!is_protected_in(&home, &target));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn matches_loaded_list_avoids_reload() {
        let home = temp_home("b");
        let _ = std::fs::remove_dir_all(&home);
        let proj = home.join("p");
        std::fs::create_dir_all(proj.join("target")).unwrap();
        add_in(&home, &proj).unwrap();
        let list = load_in(&home);
        assert!(matches(&list, &proj.join("target")));
        // 空名单永不匹配
        assert!(!matches(&[], &proj.join("target")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn rejects_root_and_home() {
        let home = temp_home("c");
        let _ = std::fs::remove_dir_all(&home);
        assert!(add_in(&home, Path::new("/")).is_err());
        if let Some(h) = std::env::var("HOME").ok().map(PathBuf::from) {
            assert!(add_in(&home, &h).is_err());
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
