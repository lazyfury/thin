//! 目录遍历：**所有遍历不变量只在这里（经 [`crate::backend`]）实现一次**。
//!
//! 不变量：
//! 1. `follow_links=false` 时用 no-follow 元数据，符号链接只上报不下钻；
//! 2. `cross_mount=false` 时遇到挂载点即剪枝；
//! 3. `same_dev_only=true` 时遇到与根不同设备的目录即剪枝；
//! 4. 根条目不回调（与既有 `dir_size` / `find_*` 语义一致，根由调用方自行处理）；
//! 5. 元数据读取失败不中断，计入 [`WalkStats::denied`]；
//! 6. 每条检查取消标志，支持中断。

use crate::backend;
use crate::kind::Entry;
use crate::progress::Control;
use std::path::PathBuf;

/// 遍历器对每个条目的处置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visit {
    /// 继续。
    Continue,
    /// 不下钻该目录（对文件无效果）。
    Skip,
    /// 立即停止整个遍历。
    Stop,
}

/// 遍历选项。
#[derive(Clone, Debug)]
pub struct WalkOptions {
    /// 最大深度（`None` = 不限；根为 0，其直接子项为 1）。
    pub max_depth: Option<usize>,
    /// 是否跟随符号链接（默认否）。
    pub follow_links: bool,
    /// 是否允许跨越挂载点（默认否）。
    pub cross_mount: bool,
    /// 是否要求同设备（默认是；APFS 各卷共享 st_dev，实际由挂载点兜底）。
    pub same_dev_only: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_depth: None,
            follow_links: false,
            cross_mount: false,
            same_dev_only: true,
        }
    }
}

/// 遍历统计（诚实核算：被拒绝的条目要看得见，而不是静默归零）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// 成功回调的条目数。
    pub entries: u64,
    /// 元数据读取失败（权限 / TCC）的条目数。
    pub denied: u64,
    /// 因卷边界 / 深度被剪枝的目录数。
    pub pruned: u64,
}

/// 一次遍历。
#[derive(Clone, Debug, Default)]
pub struct Walk {
    pub opts: WalkOptions,
}

impl Walk {
    pub fn new(opts: WalkOptions) -> Self {
        Self { opts }
    }

    /// 遍历 `roots`，对每个条目调用 `visit`。
    pub fn run<F>(&self, roots: &[PathBuf], ctl: &Control<'_>, mut visit: F) -> WalkStats
    where
        F: for<'e> FnMut(Entry<'e>) -> Visit,
    {
        backend::backend().walk(roots, &self.opts, ctl, &mut visit)
    }
}
