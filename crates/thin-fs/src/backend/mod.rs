//! 遍历后端抽象。
//!
//! - [`rust::RustBackend`]：`walkdir` + `std::fs`，永远可用（默认）。
//! - `native`（feature `native`，S4 实现）：macOS `getattrlistbulk` 批量后端，
//!   结果必须与 Rust 后端一致。
//!
//! 后端选择通过 [`backend()`] 惰性完成；若日后启用 native，可用环境变量
//! `THIN_FS_BACKEND=rust|native` 强制对拍。

mod rust;

#[cfg(all(target_os = "macos", feature = "native"))]
mod native;

use crate::kind::Entry;
use crate::progress::Control;
use crate::walk::{Visit, WalkOptions, WalkStats};
use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(all(target_os = "macos", feature = "native"))]
pub use native::NativeBackend;
pub use rust::RustBackend;

/// 遍历后端。
pub trait Backend: Send + Sync {
    /// 后端名（用于诊断 / 对拍）。
    fn name(&self) -> &'static str;

    /// 遍历 `roots`，对每个条目调用 `emit`；返回统计。
    fn walk(
        &self,
        roots: &[PathBuf],
        opts: &WalkOptions,
        ctl: &Control<'_>,
        emit: &mut (dyn for<'e> FnMut(Entry<'e>) -> Visit + '_),
    ) -> WalkStats;
}

static BACKEND: OnceLock<Box<dyn Backend>> = OnceLock::new();

/// 当前后端（进程内只选一次）。
pub fn backend() -> &'static dyn Backend {
    BACKEND.get_or_init(select).as_ref()
}

/// 当前后端名。
pub fn backend_name() -> &'static str {
    backend().name()
}

fn select() -> Box<dyn Backend> {
    #[cfg(all(target_os = "macos", feature = "native"))]
    {
        // 允许 `THIN_FS_BACKEND=rust` 强制回退，便于对拍。
        if std::env::var("THIN_FS_BACKEND").as_deref() != Ok("rust") {
            return Box::new(NativeBackend);
        }
    }
    Box::new(RustBackend)
}
