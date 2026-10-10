use rayon::prelude::*;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use walkdir::WalkDir;

/// 系统挂载点集合（进程内缓存，只取一次）。
///
/// 注意：APFS 各卷共享同一个 `st_dev`，因此**不能用设备号区分卷**，必须靠挂载点，
/// 否则 `/System` 会把 `/System/Volumes/Data`（数据卷）整个算进去、与 `/Users` 重复。
fn mount_points() -> &'static HashSet<PathBuf> {
    static MOUNTS: OnceLock<HashSet<PathBuf>> = OnceLock::new();
    MOUNTS.get_or_init(|| {
        let mut set = HashSet::new();
        unsafe {
            let mut buf: *mut libc::statfs = std::ptr::null_mut();
            let n = libc::getmntinfo(&mut buf, libc::MNT_NOWAIT);
            if n > 0 && !buf.is_null() {
                for i in 0..n as isize {
                    let fs = &*buf.offset(i);
                    let mp = std::ffi::CStr::from_ptr(fs.f_mntonname.as_ptr());
                    if let Ok(s) = mp.to_str() {
                        set.insert(PathBuf::from(s));
                    }
                }
            }
        }
        set
    })
}

/// 路径是否是挂载点（另一卷的挂载根）。用于避免跨卷统计/遍历。
pub fn is_mount_point(path: &Path) -> bool {
    mount_points().contains(path)
}

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
            // 遇到别的设备或挂载点（APFS 各卷共享 st_dev，故必须查挂载点）则不下钻
            if md.dev() != root_dev || is_mount_point(entry.path()) {
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
    if let Ok(m) = std::fs::metadata(path)
        && m.is_file()
    {
        return m.len();
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
            if (root_dev.is_some() && Some(md.dev()) != root_dev) || is_mount_point(entry.path()) {
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

/// 路径用量：实际占用 + 逻辑大小 + iCloud 占位。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// 实际分配字节（`st_blocks`，感知压缩/稀疏；Swift 后端用 `totalFileAllocatedSize`）
    pub allocated: u64,
    /// 逻辑字节（所有文件大小之和）
    pub logical: u64,
    /// iCloud 未下载占位字节（本地不占空间）
    pub dataless: u64,
    /// 文件数（Swift 后端去重硬链接；回退为 0）
    pub files: u64,
}

/// 统一的路径用量查询：优先平台后端（含 iCloud 感知），缺失时回退纯 Rust 遍历。
pub fn usage(path: &Path) -> Usage {
    crate::platform::platform()
        .dir_usage(path)
        .unwrap_or_else(|| Usage {
            allocated: size_of(path),
            logical: logical_size(path),
            dataless: 0,
            files: 0,
        })
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
    let mut found = Vec::new();
    if extensions.is_empty() {
        return found;
    }
    let max = max_depth.unwrap_or(6);
    let exts: Vec<String> = extensions
        .iter()
        .map(|e| format!(".{}", e.trim().trim_start_matches('.').to_ascii_lowercase()))
        .filter(|e| e.len() > 1)
        .collect();
    if exts.is_empty() {
        return found;
    }

    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let root_dev = device_of(root);
        let mut it = WalkDir::new(root)
            .max_depth(max)
            .follow_links(false)
            .into_iter();
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
                if (root_dev.is_some() && Some(md.dev()) != root_dev)
                    || is_mount_point(entry.path())
                {
                    it.skip_current_dir();
                }
                continue;
            }
            if !md.file_type().is_file() || md.len() < min_size {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if exts.iter().any(|ext| name.ends_with(ext.as_str())) {
                found.push(entry.path().to_path_buf());
            }
        }
    }
    found
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
            // 不跨卷查找（挂载点/其他 APFS 卷）
            if is_mount_point(entry.path()) {
                it.skip_current_dir();
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
