//! 平台能力抽象：macOS 底层能力（Swift/ThinKit）与纯 Rust 回退的统一入口。
//!
//! 上层只依赖 [`Platform`] trait，不直接认识 Swift。默认选择规则：
//! - 编译时开启 `swift` feature 且运行时 ABI 匹配 → [`SwiftPlatform`]
//! - 否则 → [`LibcPlatform`]（`statfs` 等；App/权限能力返回 `None`）
//!
//! 具体能力的语义与回退见各方法注释，FFI 约定见 `docs/swift-ffi.md`。

use crate::fsutil::Usage;
use crate::probe::{Capacity, CapacitySource};
use std::path::Path;

/// macOS 底层能力抽象。所有方法都允许「无法判断」时返回 `None`。
pub trait Platform: Send + Sync {
    /// 卷容量；Swift 后端含 purgeable，回退仅 `statfs`。
    fn capacity(&self, path: &str) -> Option<Capacity>;

    /// 某个 .app 是否在运行；`None` 表示无法判断。
    fn is_app_running(&self, app: &Path) -> Option<bool>;

    /// 读取 .app 的 `CFBundleIdentifier`；读不到返回 `None`。
    fn bundle_id(&self, app: &Path) -> Option<String>;

    /// 完全磁盘访问权限自检；`None` 表示无法判断。
    fn full_disk_access(&self) -> Option<bool>;

    /// 路径用量（实际占用 / 逻辑 / iCloud 占位）；`None` 表示无法统计。
    fn dir_usage(&self, path: &Path) -> Option<Usage>;
}

/// 纯 Rust 回退实现：卷容量用 `statfs`，其余能力不可用。
pub struct LibcPlatform;

impl Platform for LibcPlatform {
    fn capacity(&self, path: &str) -> Option<Capacity> {
        crate::probe::statfs(path).map(|v| Capacity {
            total: v.total,
            available: v.avail,
            important: v.avail,
            opportunistic: v.avail,
            source: CapacitySource::Statfs,
        })
    }

    fn is_app_running(&self, _app: &Path) -> Option<bool> {
        None
    }

    fn bundle_id(&self, _app: &Path) -> Option<String> {
        None
    }

    fn full_disk_access(&self) -> Option<bool> {
        None
    }

    fn dir_usage(&self, path: &Path) -> Option<Usage> {
        Some(Usage {
            allocated: crate::fsutil::size_of(path),
            logical: crate::fsutil::logical_size(path),
            dataless: 0,
            files: 0,
        })
    }
}

/// Swift/ThinKit 后端：能力更强，单项失败时逐项回退到 [`LibcPlatform`]。
#[cfg(feature = "swift")]
pub struct SwiftPlatform;

#[cfg(feature = "swift")]
impl Platform for SwiftPlatform {
    fn capacity(&self, path: &str) -> Option<Capacity> {
        if let Some(c) = thin_sys::volume_capacity(Path::new(path)) {
            return Some(Capacity {
                total: c.total,
                available: c.available,
                important: c.important,
                opportunistic: c.opportunistic,
                source: CapacitySource::Swift,
            });
        }
        LibcPlatform.capacity(path)
    }

    fn is_app_running(&self, app: &Path) -> Option<bool> {
        thin_sys::is_app_running(app)
    }

    fn bundle_id(&self, app: &Path) -> Option<String> {
        thin_sys::bundle_id(app)
    }

    fn full_disk_access(&self) -> Option<bool> {
        thin_sys::full_disk_access()
    }

    fn dir_usage(&self, path: &Path) -> Option<Usage> {
        if let Some(u) = thin_sys::dir_usage(path) {
            return Some(Usage {
                allocated: u.allocated,
                logical: u.logical,
                dataless: u.dataless,
                files: u.files,
            });
        }
        LibcPlatform.dir_usage(path)
    }
}

// 启用 swift 时 `platform()` 返回 SWIFT，LIBC 仅在无 feature 构建中使用。
#[cfg_attr(feature = "swift", allow(dead_code))]
static LIBC: LibcPlatform = LibcPlatform;
#[cfg(feature = "swift")]
static SWIFT: SwiftPlatform = SwiftPlatform;

/// 当前平台的默认实现。
pub fn platform() -> &'static dyn Platform {
    #[cfg(feature = "swift")]
    {
        &SWIFT
    }
    #[cfg(not(feature = "swift"))]
    {
        &LIBC
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_always_available_for_root() {
        // 无论 Swift 还是 statfs，根路径都应能拿到容量
        let c = platform().capacity("/").expect("根路径容量应可用");
        assert!(c.total > 0);
    }

    #[test]
    fn libc_platform_has_no_app_capabilities() {
        let p = LibcPlatform;
        assert!(p.is_app_running(Path::new("/Applications")).is_none());
        assert!(p.bundle_id(Path::new("/Applications")).is_none());
        assert!(p.full_disk_access().is_none());
    }
}
