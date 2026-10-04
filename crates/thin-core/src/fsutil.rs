use rayon::prelude::*;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 展开开头的 ~ 为用户主目录
pub fn expand(path: &str) -> Option<PathBuf> {
    if path == "~" {
        return std::env::var("HOME").ok().map(PathBuf::from);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").ok()?;
        return Some(PathBuf::from(home).join(rest));
    }
    Some(PathBuf::from(path))
}

/// 取路径所在设备号（用于避免跨越挂载点）
pub fn device_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.dev())
}

/// 类似 `du -sh`：统计目录实际占用，且不跨越文件系统边界。
///
/// - 按「实际分配块」(st_blocks) 计算，稀疏文件不会虚高
/// - 按 (dev, inode) 去重，硬链接不会重复计算
/// - 已知近似：APFS 克隆共享的块无法在此层面拆分
pub fn dir_size(path: &Path) -> u64 {
    let root_dev = match device_of(path) {
        Some(d) => d,
        None => return 0,
    };

    let mut total: u64 = 0;
    // 按 (设备号, inode) 去重，避免硬链接被重复计算（与 du 行为一致）
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut it = WalkDir::new(path).follow_links(false).into_iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.is_dir() {
            // 遇到别的设备（挂载点）则不下钻
            if md.dev() != root_dev {
                it.skip_current_dir();
            }
            continue;
        }
        if md.file_type().is_file() && seen.insert((md.dev(), md.ino())) {
            total = total.saturating_add(md.blocks().saturating_mul(512));
        }
    }
    total
}

/// 路径总大小：文件取实际分配块，目录递归求和
pub fn size_of(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m.blocks().saturating_mul(512),
        Ok(_) => dir_size(path),
        Err(_) => 0,
    }
}

/// 逻辑大小：所有文件 `len()` 之和（不跨越文件系统边界）。
///
/// 与 [`size_of`]（按实际分配块）不同，用于跨卷复制前的空间预估，
/// 因为复制会把稀疏文件的空洞也写成实心。
pub fn logical_size(path: &Path) -> u64 {
    if let Ok(m) = std::fs::metadata(path) {
        if m.is_file() {
            return m.len();
        }
    }
    let root_dev = device_of(path);
    let mut total: u64 = 0;
    let mut it = WalkDir::new(path).follow_links(false).into_iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.is_dir() {
            if root_dev.is_some() && Some(md.dev()) != root_dev {
                it.skip_current_dir();
            }
            continue;
        }
        if md.file_type().is_file() {
            total = total.saturating_add(md.len());
        }
    }
    total
}

/// 列出目录下各直接子项的大小（降序，并行统计）
pub fn children_sizes(root: &Path) -> Vec<(PathBuf, u64)> {
    children_sizes_progress(root, &crate::progress::Progress::new())
}

/// 带进度上报的 children_sizes
pub fn children_sizes_progress(
    root: &Path,
    progress: &crate::progress::Progress,
) -> Vec<(PathBuf, u64)> {
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            paths.push(e.path());
        }
    }
    progress.set_label("统计目录占用");
    progress.set_total(paths.len() as u64);
    let mut v: Vec<(PathBuf, u64)> = paths
        .into_par_iter()
        .map(|p| {
            let s = size_of(&p);
            progress.inc();
            (p, s)
        })
        .filter(|(_, s)| *s > 0)
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

/// 在 roots 下查找名为 dir_name 的目录（限定深度），可选要求同级存在某个文件。
pub fn find_dirs(
    roots: &[PathBuf],
    dir_name: &str,
    require_sibling: Option<&str>,
    max_depth: Option<usize>,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let max = max_depth.unwrap_or(6);

    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let mut it = WalkDir::new(root)
            .max_depth(max)
            .follow_links(false)
            .into_iter();

        while let Some(entry) = it.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.depth() == 0 || !entry.file_type().is_dir() {
                continue;
            }
            if entry.file_name() != dir_name {
                continue;
            }
            // 可选约束：同级必须存在某个标志文件（如 Cargo.toml）
            if let Some(sib) = require_sibling {
                let ok = entry
                    .path()
                    .parent()
                    .map(|p| p.join(sib).exists())
                    .unwrap_or(false);
                if !ok {
                    it.skip_current_dir();
                    continue;
                }
            }
            found.push(entry.path().to_path_buf());
            // 命中后不再下钻，避免重复统计
            it.skip_current_dir();
        }
    }
    found
}
