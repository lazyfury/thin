//! thin 的 macOS 底层能力绑定（Swift/ThinKit）。
//!
//! 当目标不是 macOS、或构建机没有 Swift 工具链时，本 crate 自动退化为
//! 「全部返回 `None`」的空实现；调用方据此回退到纯 Rust（`libc`）逻辑。
//! 是否真正连上 Swift 后端用 [`backend_available`] 判断。
//!
//! FFI 约定见 `docs/swift-ffi.md`：标量 out-param + `Int32` 错误码，
//! 字符串由 Swift `strdup` 分配、Rust 调 `thin_string_free` 释放。

use std::path::Path;

/// FFI ABI 版本；与 Swift 端 `thin_abi_version` 对齐，不一致即视为后端不可用。
pub const ABI_VERSION: u32 = 2;

/// 卷容量（含 purgeable 信息）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeCapacity {
    /// 卷总容量（字节）
    pub total: u64,
    /// `statfs` 口径的可用容量
    pub available: u64,
    /// Apple 推荐口径的可用容量，**包含可回收的 purgeable 空间**
    pub important: u64,
    /// 机会性可用容量（更激进，含可被自愿回收的空间）
    pub opportunistic: u64,
}

/// 是否已连上可用的 Swift 后端（目标为 macOS 且 ABI 匹配）。
pub fn backend_available() -> bool {
    imp::backend_available()
}

/// ThinKit 版本字符串，用于诊断。
pub fn version() -> Option<String> {
    imp::version()
}

/// 查询路径所在卷的容量；后端不可用时返回 `None`。
pub fn volume_capacity(path: &Path) -> Option<VolumeCapacity> {
    imp::volume_capacity(path)
}

/// 某个 .app 是否在运行；后端不可用或无法判断时返回 `None`。
pub fn is_app_running(app: &Path) -> Option<bool> {
    imp::is_app_running(app)
}

/// 读取 .app 的 `CFBundleIdentifier`；后端不可用或读不到时返回 `None`。
pub fn bundle_id(app: &Path) -> Option<String> {
    imp::bundle_id(app)
}

/// 自检完全磁盘访问权限；`None` 表示无法判断（后端不可用或探测路径缺失）。
pub fn full_disk_access() -> Option<bool> {
    imp::full_disk_access()
}

#[cfg(all(target_os = "macos", thin_sys_swift))]
mod imp {
    use super::{ABI_VERSION, VolumeCapacity};
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::path::Path;

    unsafe extern "C" {
        fn thin_abi_version() -> u32;
        fn thin_version() -> *mut c_char;
        fn thin_string_free(ptr: *mut c_char);
        fn thin_available_capacity(
            path: *const c_char,
            total: *mut u64,
            available: *mut u64,
            important: *mut u64,
            opportunistic: *mut u64,
        ) -> i32;
        fn thin_is_app_running(path: *const c_char) -> i32;
        fn thin_bundle_id(path: *const c_char) -> *mut c_char;
        fn thin_full_disk_access() -> i32;
    }

    pub fn backend_available() -> bool {
        // SAFETY: 无参数、返回 u32 的纯函数式入口。
        unsafe { thin_abi_version() == ABI_VERSION }
    }

    pub fn version() -> Option<String> {
        // SAFETY: 约定返回 `strdup` 的 UTF-8 C 字符串或 NULL，所有权随后归还。
        unsafe {
            let p = thin_version();
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            thin_string_free(p);
            Some(s)
        }
    }

    pub fn volume_capacity(path: &Path) -> Option<VolumeCapacity> {
        if !backend_available() {
            return None;
        }
        let c = CString::new(path.to_string_lossy().as_bytes()).ok()?;
        let (mut total, mut available, mut important, mut opportunistic) = (0u64, 0u64, 0u64, 0u64);
        // SAFETY: `c` 是有效 C 字符串；输出的四个指针均指向本栈帧内有效变量。
        let rc = unsafe {
            thin_available_capacity(
                c.as_ptr(),
                &mut total,
                &mut available,
                &mut important,
                &mut opportunistic,
            )
        };
        (rc == 0).then_some(VolumeCapacity {
            total,
            available,
            important,
            opportunistic,
        })
    }

    pub fn is_app_running(app: &Path) -> Option<bool> {
        if !backend_available() {
            return None;
        }
        let c = CString::new(app.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: `c` 是有效 C 字符串。
        match unsafe { thin_is_app_running(c.as_ptr()) } {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }

    pub fn bundle_id(app: &Path) -> Option<String> {
        if !backend_available() {
            return None;
        }
        let c = CString::new(app.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: 返回 `strdup` 的 UTF-8 C 字符串或 NULL，所有权随后归还。
        unsafe {
            let p = thin_bundle_id(c.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            thin_string_free(p);
            (!s.is_empty()).then_some(s)
        }
    }

    pub fn full_disk_access() -> Option<bool> {
        if !backend_available() {
            return None;
        }
        // SAFETY: 无参纯函数式入口。
        match unsafe { thin_full_disk_access() } {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }
}

#[cfg(not(all(target_os = "macos", thin_sys_swift)))]
mod imp {
    use super::VolumeCapacity;
    use std::path::Path;

    pub fn backend_available() -> bool {
        false
    }

    pub fn version() -> Option<String> {
        None
    }

    pub fn volume_capacity(_path: &Path) -> Option<VolumeCapacity> {
        None
    }

    pub fn is_app_running(_app: &Path) -> Option<bool> {
        None
    }

    pub fn bundle_id(_app: &Path) -> Option<String> {
        None
    }

    pub fn full_disk_access() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_consistent_when_available() {
        let Some(c) = volume_capacity(Path::new("/")) else {
            // 无 Swift 后端：空实现是合法状态
            assert!(!backend_available());
            return;
        };
        assert!(c.total > 0, "总量应大于 0");
        assert!(c.available <= c.total);
        // important 是「含 purgeable」口径，通常 >= statfs 可用
        assert!(c.important >= c.available.saturating_sub(0));
    }

    #[test]
    fn abi_guard_reports_bool() {
        // 只是确保入口可安全调用
        let _ = backend_available();
    }

    #[test]
    fn fda_probe_is_safe() {
        // 有后端时应能给出布尔结论；无后端为 None。不假设具体权限。
        if backend_available() {
            assert!(full_disk_access().is_some(), "有后端时 FDA 应可判断");
        }
    }

    #[test]
    fn bundle_id_matches_known_app() {
        if !backend_available() {
            return;
        }
        let calc = Path::new("/System/Applications/Calculator.app");
        if calc.exists() {
            assert_eq!(bundle_id(calc).as_deref(), Some("com.apple.calculator"));
        }
    }

    #[test]
    fn detects_running_finder() {
        if !backend_available() {
            return;
        }
        let finder = Path::new("/System/Library/CoreServices/Finder.app");
        if finder.exists() {
            assert_eq!(is_app_running(finder), Some(true), "Finder 应始终在运行");
        }
    }
}
