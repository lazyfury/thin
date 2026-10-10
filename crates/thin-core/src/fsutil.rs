use rayon::prelude::*;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// 遍历/挂载点能力来自 [`thin_fs`]（不变量集中实现），这里保持原有 API 形状。
pub use thin_fs::{device_of, is_mount_point};

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

/// 规范化路径用于**比较**：存在则 `canonicalize`（解析 symlink，如 macOS 上
/// `/tmp → /private/tmp`、`/var → /private/var`），不存在则回退原样。
///
/// 扫描/规则展开时统一走这里，保证 `starts_with` 嵌套去重与安全门前缀判断
/// 不会因 symlink 或路径写法不同而失效。
pub fn canonicalize_or(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// 目录遍历选项：默认不跟符号链接、不跨卷、不限深度。
fn walk_opts(max_depth: Option<usize>) -> thin_fs::WalkOptions {
    thin_fs::WalkOptions {
        max_depth,
        ..Default::default()
    }
}

fn no_ctl() -> thin_fs::Control<'static> {
    thin_fs::Control::none()
}

/// 类似 `du -sh`：统计目录实际占用，且不跨越文件系统边界。
///
/// - 按「实际分配块」(st_blocks) 计算，稀疏文件不会虚高
/// - 按 (dev, inode) 去重，硬链接不会重复计算
/// - 已知近似：APFS 克隆共享的块无法在此层面拆分
pub fn dir_size(path: &Path) -> u64 {
    thin_fs::usage(path, &walk_opts(None), &no_ctl()).allocated
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
    thin_fs::usage(path, &walk_opts(None), &no_ctl()).logical
}

/// 路径用量：实际占用 + 逻辑大小 + iCloud 占位。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// 实际分配字节（`st_blocks`，感知压缩/稀疏；Swift 后端用 `totalFileAllocatedSize`）
    pub allocated: u64,
    /// 逻辑字节（所有文件大小之和）
    pub logical: u64,
    /// iCloud 未下载占位字节（本地不占空间）
    pub dataless: u64,
    /// 文件数（硬链接去重；纯 Rust 后端也提供）
    pub files: u64,
    /// 元数据不可读的条目数（权限 / TCC）。诚实核算：总量可能因此偏低。
    /// Swift 后端不提供时记 0。
    pub denied: u64,
}

/// 纯 Rust 单遍用量（无 iCloud 占位识别）。
pub(crate) fn rust_usage(path: &Path) -> Usage {
    let u = thin_fs::usage(path, &walk_opts(None), &no_ctl());
    Usage {
        allocated: u.allocated,
        logical: u.logical,
        dataless: 0,
        files: u.files,
        denied: u.denied,
    }
}

/// 统一的路径用量查询：优先平台后端（含 iCloud 感知），缺失时回退纯 Rust 单遍遍历。
pub fn usage(path: &Path) -> Usage {
    crate::platform::platform()
        .dir_usage(path)
        .unwrap_or_else(|| rust_usage(path))
}

/// 目录里 iCloud 未下载占位 `(逻辑字节, 文件数)`；后端不支持时返回 `None`。
///
/// **按需调用**：逐文件查询 iCloud 状态较慢，不要放进默认热路径。
pub fn dir_dataless(path: &Path) -> Option<(u64, u64)> {
    crate::platform::platform().dir_dataless(path)
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
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v
}

/// 目录条目的类型（用于文件浏览/`thin ls`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// 符号链接（不跟随）
    Symlink,
    /// 挂载点（设备号与父目录不同，不跨卷统计）
    Mount,
    /// 无权限读取（如 macOS TCC 保护目录）
    Inaccessible,
}

impl EntryKind {
    pub fn label(&self) -> &'static str {
        match self {
            EntryKind::Dir => "目录",
            EntryKind::File => "文件",
            EntryKind::Symlink => "链接",
            EntryKind::Mount => "挂载",
            EntryKind::Inaccessible => "无权",
        }
    }
}

/// 一层目录里的一个子项（保留类型与符号链接目标，供浏览使用）。
#[derive(Debug, Clone)]
pub struct ChildEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    /// 递归实占；符号链接/挂载点/无权限为 0
    pub size: u64,
    pub target: Option<PathBuf>,
}

/// 列出目录的直接子项（含类型），按大小降序。与 [`children_sizes`] 不同：
/// 不过滤 0 字节、区分文件/链接/挂载/无权限，适合文件浏览。
pub fn children_entries(root: &Path) -> Vec<ChildEntry> {
    children_entries_progress(root, &crate::progress::Progress::new())
}

pub fn children_entries_progress(
    root: &Path,
    progress: &crate::progress::Progress,
) -> Vec<ChildEntry> {
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            paths.push(e.path());
        }
    }
    progress.set_label("统计目录占用");
    progress.set_total(paths.len() as u64);
    let root_dev = device_of(root);
    let mut v: Vec<ChildEntry> = paths
        .into_par_iter()
        .map(|p| {
            let entry = match std::fs::symlink_metadata(&p) {
                Ok(m) if m.file_type().is_symlink() => ChildEntry {
                    target: std::fs::read_link(&p).ok(),
                    path: p,
                    kind: EntryKind::Symlink,
                    size: 0,
                },
                Ok(m) if m.is_dir() => {
                    let kind = if Some(m.dev()) != root_dev || is_mount_point(&p) {
                        EntryKind::Mount
                    } else if std::fs::read_dir(&p).is_err() {
                        EntryKind::Inaccessible
                    } else {
                        EntryKind::Dir
                    };
                    let size = size_of(&p);
                    ChildEntry {
                        path: p,
                        kind,
                        size,
                        target: None,
                    }
                }
                Ok(_) => ChildEntry {
                    size: size_of(&p),
                    path: p,
                    kind: EntryKind::File,
                    target: None,
                },
                Err(_) => ChildEntry {
                    path: p,
                    kind: EntryKind::Inaccessible,
                    size: 0,
                    target: None,
                },
            };
            progress.inc();
            entry
        })
        .collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.size));
    v
}

/// 在 roots 下按扩展名/后缀查找文件（限定深度与最小大小），不跨卷。
///
/// `extensions` 忽略大小写与可选的前导点，支持复合后缀（如 `tar.gz`）。
pub fn find_files(
    roots: &[PathBuf],
    extensions: &[String],
    max_depth: Option<usize>,
    min_size: u64,
) -> Vec<PathBuf> {
    if extensions.is_empty() {
        return Vec::new();
    }
    let opts = walk_opts(Some(max_depth.unwrap_or(6)));
    thin_fs::query::find_files(roots, extensions, opts, min_size, &no_ctl())
}

/// 在 roots 下查找名为 dir_name 的目录（限定深度），可选要求同级存在某个文件。
pub fn find_dirs(
    roots: &[PathBuf],
    dir_name: &str,
    require_sibling: Option<&str>,
    max_depth: Option<usize>,
) -> Vec<PathBuf> {
    let opts = walk_opts(Some(max_depth.unwrap_or(6)));
    thin_fs::query::find_dir(roots, dir_name, require_sibling, opts, &no_ctl())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    fn tmp(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("thin-core-fsutil-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn size_matches_manual_and_dedupes_hardlinks() {
        let base = tmp("size");
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("a"), vec![0u8; 10_000]).unwrap();
        fs::write(base.join("sub/b"), vec![0u8; 20_000]).unwrap();
        fs::hard_link(base.join("a"), base.join("a2")).unwrap();

        // 实占：按 inode 去重（a 与 a2 同 inode，只算一次）
        let unique_alloc: u64 = [base.join("a"), base.join("sub/b")]
            .iter()
            .map(|p| fs::metadata(p).unwrap().blocks().saturating_mul(512))
            .sum();
        assert_eq!(dir_size(&base), unique_alloc);
        assert_eq!(size_of(&base), unique_alloc);

        // 逻辑大小：不去重，硬链接重复计入
        assert_eq!(logical_size(&base), 40_000);

        // 单文件：size_of 取实际块，logical_size 取 len
        assert_eq!(
            size_of(&base.join("a")),
            fs::metadata(base.join("a")).unwrap().blocks() * 512
        );
        assert_eq!(logical_size(&base.join("a")), 10_000);

        // usage：单遍聚合
        let u = rust_usage(&base);
        assert_eq!(u.allocated, unique_alloc);
        assert_eq!(u.logical, 40_000);
        assert_eq!(u.files, 2);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn find_files_respects_suffix_depth_and_size() {
        let base = tmp("find-files");
        fs::create_dir_all(base.join("nested")).unwrap();
        fs::write(base.join("App.dmg"), vec![0u8; 2 * 1024 * 1024]).unwrap();
        fs::write(base.join("nested/Archive.ZIP"), vec![0u8; 2 * 1024 * 1024]).unwrap();
        fs::write(base.join("tiny.zip"), b"x").unwrap();
        fs::write(base.join("keep.txt"), b"x").unwrap();

        let got = find_files(
            std::slice::from_ref(&base),
            &["dmg".to_string(), "zip".to_string()],
            Some(3),
            1024,
        );
        assert_eq!(got.len(), 2, "tiny.zip 因低于 min_size 被过滤");
        assert!(got.iter().any(|p| p.ends_with("App.dmg")));
        assert!(got.iter().any(|p| p.ends_with("Archive.ZIP")));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn find_dirs_requires_sibling() {
        let base = tmp("find-dirs");
        fs::create_dir_all(base.join("proj/target")).unwrap();
        fs::create_dir_all(base.join("junk/target")).unwrap();
        fs::write(base.join("proj/Cargo.toml"), b"").unwrap();

        let got = find_dirs(
            std::slice::from_ref(&base),
            "target",
            Some("Cargo.toml"),
            Some(4),
        );
        assert_eq!(got.len(), 1);
        assert!(got[0].ends_with("proj/target"));

        let _ = fs::remove_dir_all(&base);
    }
}
