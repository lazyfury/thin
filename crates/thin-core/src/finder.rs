//! 大文件查找与重复文件检测（M2）。

use crate::progress::Progress;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 遍历时跳过的目录（避免 iCloud 等网络卷、系统索引目录）
fn excluded(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("/Library/Mobile Documents")
        || s.contains("/Library/CloudStorage")
        || s.ends_with("/.Spotlight-V100")
        || s.ends_with("/.fseventsd")
        || s.ends_with("/.DocumentRevisions-V100")
        || s.ends_with("/.Trashes")
}

struct Found {
    path: PathBuf,
    /// 逻辑大小（用于重复文件比对）
    size: u64,
    /// 实际分配块（用于“占用空间”统计，稀疏文件不虚高）
    alloc: u64,
    inode: (u64, u64),
}

/// 遍历 roots 下所有 >= min_size 的文件（不跨设备）
fn walk_files(roots: &[PathBuf], min_size: u64, progress: Option<&Progress>) -> Vec<Found> {
    if let Some(p) = progress {
        p.set_label("遍历文件");
    }
    let mut out = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        if root.is_file() {
            if let Ok(m) = std::fs::metadata(root) {
                if m.len() >= min_size {
                    out.push(Found {
                        path: root.clone(),
                        size: m.len(),
                        alloc: m.blocks().saturating_mul(512),
                        inode: (m.dev(), m.ino()),
                    });
                }
            }
            continue;
        }
        let root_dev = crate::fsutil::device_of(root);
        let mut it = WalkDir::new(root).follow_links(false).into_iter();
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
                if excluded(entry.path()) {
                    it.skip_current_dir();
                    continue;
                }
                if Some(md.dev()) != root_dev {
                    it.skip_current_dir();
                }
                continue;
            }
            if md.file_type().is_file() && md.len() >= min_size {
                out.push(Found {
                    path: entry.path().to_path_buf(),
                    size: md.len(),
                    alloc: md.blocks().saturating_mul(512),
                    inode: (md.dev(), md.ino()),
                });
                if let Some(p) = progress {
                    p.touch();
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 大文件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LargeFile {
    pub path: PathBuf,
    pub size: u64,
}

/// 查找最大的文件（按实际占用降序，取前 limit 个）
pub fn find_large(roots: &[PathBuf], min_size: u64, limit: usize) -> Vec<LargeFile> {
    find_large_progress(roots, min_size, limit, &Progress::new())
}

pub fn find_large_progress(
    roots: &[PathBuf],
    min_size: u64,
    limit: usize,
    progress: &Progress,
) -> Vec<LargeFile> {
    progress.set_label("扫描大文件");
    let mut files: Vec<LargeFile> = walk_files(roots, min_size, Some(progress))
        .into_iter()
        .filter(|f| f.alloc >= min_size)
        .map(|f| LargeFile {
            path: f.path,
            size: f.alloc,
        })
        .collect();
    files.sort_by(|a, b| b.size.cmp(&a.size));
    files.truncate(limit);
    files
}

// ---------------------------------------------------------------------------
// 重复文件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DupeGroup {
    pub size: u64,
    pub paths: Vec<PathBuf>,
}

impl DupeGroup {
    /// 保留一份后可回收的字节数
    pub fn wasted(&self) -> u64 {
        self.size
            .saturating_mul(self.paths.len().saturating_sub(1) as u64)
    }
}

/// 查找内容相同的重复文件（先按大小、再按部分哈希、最后全量哈希）
pub fn find_duplicates(roots: &[PathBuf], min_size: u64, limit: usize) -> Vec<DupeGroup> {
    find_duplicates_progress(roots, min_size, limit, &Progress::new())
}

pub fn find_duplicates_progress(
    roots: &[PathBuf],
    min_size: u64,
    limit: usize,
    progress: &Progress,
) -> Vec<DupeGroup> {
    // 1) 按大小分组，硬链接（同 inode）只算一次
    progress.set_label("按大小分组");
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for f in walk_files(roots, min_size, Some(progress)) {
        if !seen.insert(f.inode) {
            continue; // 硬链接不是真的重复占用
        }
        by_size.entry(f.size).or_default().push(f.path);
    }
    let candidates: Vec<Vec<PathBuf>> = by_size.into_values().filter(|v| v.len() > 1).collect();

    // 2) 部分哈希（前 8KB）快速排除
    progress.reset();
    progress.set_label("部分哈希");
    progress.set_total(candidates.len() as u64);
    let partial: Vec<Vec<PathBuf>> = candidates
        .par_iter()
        .map(|group| {
            let r = group_by_hash(group, Some(8192));
            progress.inc();
            r
        })
        .flatten()
        .collect();

    // 3) 全量哈希确认
    progress.reset();
    progress.set_label("全量哈希");
    progress.set_total(partial.len() as u64);
    let mut groups: Vec<DupeGroup> = partial
        .par_iter()
        .map(|group| {
            let r = group_by_hash(group, None);
            progress.inc();
            r
        })
        .flatten()
        .map(|paths| {
            let size = std::fs::metadata(&paths[0]).map(|m| m.len()).unwrap_or(0);
            DupeGroup { size, paths }
        })
        .collect();

    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()));
    groups.truncate(limit);
    groups
}

/// 按内容哈希把 paths 分组，返回其中 >1 个文件的分组
fn group_by_hash(paths: &[PathBuf], limit: Option<u64>) -> Vec<Vec<PathBuf>> {
    let mut map: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
    for p in paths {
        if let Some(h) = hash_file(p, limit) {
            map.entry(h).or_default().push(p.clone());
        }
    }
    map.into_values().filter(|v| v.len() > 1).collect()
}

fn hash_file(path: &Path, limit: Option<u64>) -> Option<[u8; 32]> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    match limit {
        Some(n) => {
            let mut buf = vec![0u8; n as usize];
            let read = f.read(&mut buf).ok()?;
            hasher.update(&buf[..read]);
        }
        None => {
            std::io::copy(&mut f, &mut hasher).ok()?;
        }
    }
    Some(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_duplicates_and_ignores_unique() {
        let base = std::env::temp_dir().join(format!("thin-dupes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let content = vec![7u8; 4096];
        std::fs::write(base.join("a.bin"), &content).unwrap();
        std::fs::write(base.join("b.bin"), &content).unwrap();
        std::fs::write(base.join("c.bin"), vec![9u8; 4096]).unwrap();

        let groups = find_duplicates(&[base.clone()], 1, 100);
        assert_eq!(groups.len(), 1, "应只有一组重复");
        assert_eq!(groups[0].paths.len(), 2);
        assert_eq!(groups[0].wasted(), 4096);

        let large = find_large(&[base.clone()], 1, 10);
        assert_eq!(large.len(), 3);

        let _ = std::fs::remove_dir_all(&base);
    }
}
