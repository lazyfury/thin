//! 平台能力抽象：macOS 底层能力（Swift/ThinKit）与纯 Rust 回退的统一入口。
//!
//! 上层只依赖 [`Platform`] trait，不直接认识 Swift。默认选择规则：
//! - 编译时开启 `swift` feature 且运行时 ABI 匹配 → [`SwiftPlatform`]
//! - 否则 → [`LibcPlatform`]（`statfs` 等；App/权限能力返回 `None`）
//!
//! 具体能力的语义与回退见各方法注释，FFI 约定见 `docs/swift-ffi.md`。
//!
//! **排查开关**：设置环境变量 `THIN_BACKEND=libc`（或 `THIN_NO_SWIFT=1`）可在同一
//! 二进制内强制走 [`LibcPlatform`]，用于对比 Swift 与纯 Rust 路径的耗时/结果。

use crate::fsutil::Usage;
use crate::probe::{Capacity, CapacitySource};
use std::path::Path;
use std::sync::OnceLock;

/// App 沙盒信息（来自代码签名 entitlements）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxInfo {
    pub bundle_id: Option<String>,
    pub sandboxed: bool,
    /// `com.apple.security.application-groups` 声明的 group id
    pub groups: Vec<String>,
    /// `com.apple.developer.icloud-container-identifiers` 声明的 iCloud 容器
    pub icloud_containers: Vec<String>,
    /// 代码签名 Team Identifier（Group Containers 常见前缀）
    pub team_id: Option<String>,
}

/// 一个沙盒容器目录及其权威标识（目录名可能是 UUID）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxContainer {
    pub path: std::path::PathBuf,
    pub identifier: String,
}

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

    /// 读取 .app 的沙盒信息；`None` 表示无法读取。
    fn app_sandbox_info(&self, app: &Path) -> Option<SandboxInfo>;

    /// 枚举用户沙盒容器（一次批量返回）；`None` 表示后端不可用。
    fn sandbox_containers(&self, home: &Path) -> Option<Vec<SandboxContainer>>;

    /// 移入系统废纸篓；`None` 表示后端不可用。
    fn trash_item(&self, path: &Path) -> Option<bool>;

    /// 是否支持系统废纸篓（Swift 后端可用）。
    fn trash_available(&self) -> bool;
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
        Some(crate::fsutil::rust_usage(path))
    }

    fn app_sandbox_info(&self, _app: &Path) -> Option<SandboxInfo> {
        None
    }

    fn sandbox_containers(&self, _home: &Path) -> Option<Vec<SandboxContainer>> {
        None
    }

    fn trash_item(&self, _path: &Path) -> Option<bool> {
        None
    }

    fn trash_available(&self) -> bool {
        false
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

    fn app_sandbox_info(&self, app: &Path) -> Option<SandboxInfo> {
        let info = thin_sys::app_sandbox_info(app)?;
        Some(SandboxInfo {
            bundle_id: info.bundle_id,
            sandboxed: info.sandboxed,
            groups: info.groups,
            icloud_containers: info.icloud_containers,
            team_id: info.team_id,
        })
    }

    fn sandbox_containers(&self, home: &Path) -> Option<Vec<SandboxContainer>> {
        let list = thin_sys::sandbox_containers(home)?;
        Some(
            list.into_iter()
                .map(|c| SandboxContainer {
                    path: std::path::PathBuf::from(c.path),
                    identifier: c.identifier,
                })
                .collect(),
        )
    }

    fn trash_item(&self, path: &Path) -> Option<bool> {
        thin_sys::trash_item(path)
    }

    fn trash_available(&self) -> bool {
        thin_sys::backend_available()
    }
}

// 启用 swift 时 `platform()` 返回 SWIFT，LIBC 仅在无 feature 构建中使用。
#[cfg_attr(feature = "swift", allow(dead_code))]
static LIBC: LibcPlatform = LibcPlatform;
#[cfg(feature = "swift")]
static SWIFT: SwiftPlatform = SwiftPlatform;

/// 是否被环境变量强制走纯 Rust（`THIN_BACKEND=libc|rust` 或 `THIN_NO_SWIFT!=0`）。
fn force_libc() -> bool {
    static FORCE: OnceLock<bool> = OnceLock::new();
    *FORCE.get_or_init(|| {
        matches!(
            std::env::var("THIN_BACKEND").as_deref(),
            Ok("libc") | Ok("rust")
        ) || std::env::var("THIN_NO_SWIFT").is_ok_and(|v| v != "0")
    })
}

/// 当前平台的默认实现。
pub fn platform() -> &'static dyn Platform {
    #[cfg(feature = "swift")]
    {
        if force_libc() {
            return &LIBC;
        }
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
