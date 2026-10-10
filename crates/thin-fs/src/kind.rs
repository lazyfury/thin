//! 遍历条目的类型与元数据快照。

use std::path::Path;

/// 条目的类型（大小写不敏感的 POSIX 分类，外加 thin 关心的卷边界）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 普通目录。
    Dir,
    /// 普通文件。
    File,
    /// 符号链接（不跟随）。
    Symlink,
    /// 挂载点（另一卷的挂载根，`cross_mount=false` 时不下钻）。
    Mount,
    /// 与根目录不在同一设备（`same_dev_only=true` 时不下钻）。
    Volume,
    /// 其他类型（FIFO / socket / 设备节点）。
    Other,
    /// 元数据不可读（权限 / TCC）。
    Denied,
}

/// 一次遍历中拿到的元数据快照。
///
/// `alloc` 按 macOS 的 512 字节块换算（`st_blocks * 512`），与 `du` 口径一致，
/// 稀疏文件不会虚高。`size` 是逻辑字节（`st_size`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    /// 逻辑字节。
    pub size: u64,
    /// 实际分配字节（`st_blocks * 512`）。
    pub alloc: u64,
    pub dev: u64,
    pub ino: u64,
    /// 修改时间（Unix 秒）。
    pub mtime: i64,
}

/// 遍历回调看到的条目。`path` 借用自遍历器，避免逐条分配。
pub struct Entry<'a> {
    pub path: &'a Path,
    /// 相对遍历根的深度（根的深度为 0，但默认不回调根）。
    pub depth: usize,
    /// 元数据；`None` 表示不可读（此时 `kind` 视角为 [`Kind::Denied`]）。
    pub meta: Option<Meta>,
}

impl Entry<'_> {
    /// 元数据不可读时按 `Denied` 处理。
    pub fn kind(&self) -> Kind {
        self.meta.map(|m| m.kind).unwrap_or(Kind::Denied)
    }
}
