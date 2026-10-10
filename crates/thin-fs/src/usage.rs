//! 单遍用量聚合。
//!
//! 一次遍历同时算出实际分配 (`allocated`)、逻辑大小 (`logical`) 与文件数 (`files`)。
//!
//! - `allocated` 按 inode 去重（硬链接不重复计入），与 `du` 口径一致；
//! - `logical` 不去重（所有文件大小之和）。
//!
//! 注意：iCloud「云占位」(dataless) 纯 Rust 无法可靠识别，不在此层；由 `thin-core`
//! 用平台能力叠加。

use crate::kind::Kind;
use crate::progress::Control;
use crate::walk::{Visit, Walk, WalkOptions};
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// 路径用量。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// 实际分配字节（`st_blocks * 512`，按 inode 去重）。
    pub allocated: u64,
    /// 逻辑字节（所有文件 `st_size` 之和）。
    pub logical: u64,
    /// 去重后的普通文件数。
    pub files: u64,
    /// 元数据不可读的条目数（权限 / TCC）。**诚实核算**：让「读不到」可见，
    /// 而不是静默当作 0 字节。
    pub denied: u64,
    /// 因卷边界 / 深度被剪枝的目录数。
    pub pruned: u64,
}

/// 单遍统计 `root` 的用量。
pub fn usage(root: &Path, opts: &WalkOptions, ctl: &Control<'_>) -> Usage {
    // 单文件根：直接取属性，不启动遍历。
    if let Ok(md) = std::fs::metadata(root)
        && md.file_type().is_file()
    {
        return Usage {
            allocated: md.blocks().saturating_mul(512),
            logical: md.len(),
            files: 1,
            denied: 0,
            pruned: 0,
        };
    }

    let mut u = Usage::default();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let stats = Walk::new(opts.clone()).run(&[root.to_path_buf()], ctl, |e| {
        if let Some(m) = e.meta
            && m.kind == Kind::File
        {
            u.logical = u.logical.saturating_add(m.size);
            if seen.insert((m.dev, m.ino)) {
                u.allocated = u.allocated.saturating_add(m.alloc);
                u.files += 1;
            }
        }
        Visit::Continue
    });
    u.denied = stats.denied;
    u.pruned = stats.pruned;
    u
}
