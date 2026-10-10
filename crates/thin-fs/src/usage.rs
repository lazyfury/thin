//! 单遍用量聚合。
//!
//! 一次遍历同时算出实际分配 (`allocated`)、逻辑大小 (`logical`) 与文件数 (`files`)，
//! 替代过去 `size_of` + `logical_size` 的两次遍历。
//!
//! - `allocated` 按 inode 去重（硬链接不重复计入），与 `du` 口径一致；
//! - `logical` 不去重（与既有语义一致，用于跨卷复制前的空间预估）。
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
        };
    }

    let mut u = Usage::default();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    Walk::new(opts.clone()).run(&[root.to_path_buf()], ctl, |e| {
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
    u
}
