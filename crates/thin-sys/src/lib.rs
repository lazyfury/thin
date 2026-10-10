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
pub const ABI_VERSION: u32 = 5;

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

/// 目录用量（一次批量统计，不逐文件跨 FFI）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct DirUsage {
    /// 实际分配字节（感知 APFS 压缩/稀疏）
    pub allocated: u64,
    /// 逻辑字节（所有文件大小之和）
    pub logical: u64,
    /// iCloud 未下载占位的逻辑字节（本地不占空间）
    pub dataless: u64,
    /// 文件数（硬链接去重）
    pub files: u64,
    /// 未下载占位文件数
    #[serde(rename = "datalessCount", default)]
    pub dataless_count: u64,
}

/// 目录里 iCloud 未下载占位（按需查询，逐文件 XPC，较慢）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct DirDataless {
    /// 未下载占位的逻辑字节（本地不占空间）
    pub dataless: u64,
    /// 未下载占位的文件数
    #[serde(rename = "datalessCount", default)]
    pub dataless_count: u64,
}

/// App 沙盒信息（来自代码签名 entitlements）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSandboxInfo {
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub sandboxed: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    /// `com.apple.developer.icloud-container-identifiers` 声明的 iCloud 容器
    #[serde(default)]
    pub icloud_containers: Vec<String>,
    /// 代码签名 Team Identifier（Group Containers 常见前缀）
    #[serde(default)]
    pub team_id: Option<String>,
}

/// 一个沙盒容器目录及其权威标识。
///
/// 目录名未必等于 bundle id（可能是 UUID）；`identifier` 来自容器元数据，
/// 用于把「名字不可读」的容器归回所属 App。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct SandboxContainer {
    pub path: String,
    pub identifier: String,
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

/// 批量统计目录用量（Swift 枚举器一次走完，**不含 iCloud 云占位**）；
/// 后端不可用或路径不存在时返回 `None`。
pub fn dir_usage(path: &Path) -> Option<DirUsage> {
    imp::dir_usage(path)
}

/// 按需统计目录里 iCloud 未下载占位（逐文件查询，较慢）；后端不可用时返回 `None`。
pub fn dir_dataless(path: &Path) -> Option<DirDataless> {
    imp::dir_dataless(path)
}

/// 读取 .app 的沙盒信息（bundle id / 是否沙盒 / group id / iCloud 容器 / team id）。
pub fn app_sandbox_info(app: &Path) -> Option<AppSandboxInfo> {
    imp::app_sandbox_info(app)
}

/// 枚举 `<home>/Library/Containers` 下所有沙盒容器（一次批量返回）。
pub fn sandbox_containers(home: &Path) -> Option<Vec<SandboxContainer>> {
    imp::sandbox_containers(home)
}

/// 把路径移入系统废纸篓；`Some(true)` 成功、`Some(false)` 失败、`None` 后端不可用。
pub fn trash_item(path: &Path) -> Option<bool> {
    imp::trash_item(path)
}

#[cfg(all(target_os = "macos", thin_sys_swift))]
mod imp {
    use super::{
        ABI_VERSION, AppSandboxInfo, DirDataless, DirUsage, SandboxContainer, VolumeCapacity,
    };
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
        fn thin_dir_usage_json(path: *const c_char) -> *mut c_char;
        fn thin_dir_dataless_json(path: *const c_char) -> *mut c_char;
        fn thin_app_sandbox_info_json(path: *const c_char) -> *mut c_char;
        fn thin_sandbox_containers_json(home: *const c_char) -> *mut c_char;
        fn thin_trash_item(path: *const c_char) -> i32;
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

    pub fn dir_usage(path: &Path) -> Option<DirUsage> {
        if !backend_available() {
            return None;
        }
        let c = CString::new(path.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: 返回 `strdup` 的 JSON C 字符串或 NULL，所有权随后归还。
        unsafe {
            let p = thin_dir_usage_json(c.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            thin_string_free(p);
            serde_json::from_str(&s).ok()
        }
    }

    pub fn dir_dataless(path: &Path) -> Option<DirDataless> {
        if !backend_available() {
            return None;
        }
        let c = CString::new(path.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: 返回 `strdup` 的 JSON C 字符串或 NULL，所有权随后归还。
        unsafe {
            let p = thin_dir_dataless_json(c.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            thin_string_free(p);
            serde_json::from_str(&s).ok()
        }
    }

    pub fn app_sandbox_info(app: &Path) -> Option<AppSandboxInfo> {
        if !backend_available() {
            return None;
        }
        let c = CString::new(app.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: 返回 `strdup` 的 JSON C 字符串或 NULL，所有权随后归还。
        unsafe {
            let p = thin_app_sandbox_info_json(c.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            thin_string_free(p);
            serde_json::from_str(&s).ok()
        }
    }

    pub fn sandbox_containers(home: &Path) -> Option<Vec<SandboxContainer>> {
        if !backend_available() {
            return None;
        }
        let c = CString::new(home.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: 返回 `strdup` 的 JSON C 字符串或 NULL，所有权随后归还。
        unsafe {
            let p = thin_sandbox_containers_json(c.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            thin_string_free(p);
            serde_json::from_str(&s).ok()
        }
    }

    pub fn trash_item(path: &Path) -> Option<bool> {
        if !backend_available() {
            return None;
        }
        let c = CString::new(path.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: `c` 是有效 C 字符串。
        Some(unsafe { thin_trash_item(c.as_ptr()) } == 0)
    }
}

#[cfg(not(all(target_os = "macos", thin_sys_swift)))]
mod imp {
    use super::{AppSandboxInfo, DirDataless, DirUsage, SandboxContainer, VolumeCapacity};
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

    pub fn dir_usage(_path: &Path) -> Option<DirUsage> {
        None
    }

    pub fn dir_dataless(_path: &Path) -> Option<DirDataless> {
        None
    }

    pub fn app_sandbox_info(_app: &Path) -> Option<AppSandboxInfo> {
        None
    }

    pub fn sandbox_containers(_home: &Path) -> Option<Vec<SandboxContainer>> {
        None
    }

    pub fn trash_item(_path: &Path) -> Option<bool> {
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
    fn dir_usage_matches_small_tree() {
        if !backend_available() {
            return;
        }
        let base = std::env::temp_dir().join(format!("thin-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a")).unwrap();
        std::fs::write(base.join("a/x.bin"), vec![0u8; 4096]).unwrap();
        std::fs::write(base.join("y.bin"), vec![0u8; 8192]).unwrap();

        let u = dir_usage(&base).expect("目录用量应可用");
        assert_eq!(u.files, 2);
        assert!(u.logical >= 12288, "逻辑至少 12KB，实得 {}", u.logical);
        assert!(u.allocated > 0);
        assert_eq!(u.dataless, 0, "本地文件不应算作云占位");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dir_dataless_zero_for_local_tree() {
        if !backend_available() {
            return;
        }
        let base = std::env::temp_dir().join(format!("thin-dataless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a")).unwrap();
        std::fs::write(base.join("a/x.bin"), vec![0u8; 4096]).unwrap();

        let d = dir_dataless(&base).expect("应可查询云占位");
        assert_eq!(d.dataless, 0, "本地文件不应算作云占位");
        assert_eq!(d.dataless_count, 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reads_sandbox_entitlements() {
        if !backend_available() {
            return;
        }
        let safari = Path::new("/Applications/Safari.app");
        if safari.exists() {
            let info = app_sandbox_info(safari).expect("Safari 沙盒信息应可读");
            assert_eq!(info.bundle_id.as_deref(), Some("com.apple.Safari"));
            assert!(info.sandboxed, "Safari 应为沙盒 App");
        }
    }

    #[test]
    fn reads_icloud_entitlement() {
        if !backend_available() {
            return;
        }
        let notes = Path::new("/System/Applications/Notes.app");
        if notes.exists() {
            let info = app_sandbox_info(notes).expect("Notes 沙盒信息应可读");
            assert!(
                info.icloud_containers
                    .iter()
                    .any(|c| c == "com.apple.notes"),
                "应读出 iCloud 容器: {:?}",
                info.icloud_containers
            );
        }
    }

    #[test]
    fn enumerates_sandbox_containers() {
        if !backend_available() {
            return;
        }
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let Some(list) = sandbox_containers(Path::new(&home)) else {
            return;
        };
        for c in &list {
            assert!(!c.path.is_empty(), "容器路径不应为空");
            assert!(!c.identifier.is_empty(), "容器标识不应为空");
        }
    }

    #[test]
    fn trash_missing_path_fails_gracefully() {
        if !backend_available() {
            return;
        }
        assert_eq!(
            trash_item(Path::new("/nonexistent/thin-xyz-404")),
            Some(false)
        );
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
