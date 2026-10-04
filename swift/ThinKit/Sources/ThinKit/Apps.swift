import AppKit
import Foundation

/// 某个 .app 当前是否在运行（NSWorkspace 权威判断）。
///
/// 返回：`1` 运行中 / `0` 未运行 / `-1` 无法判断。
///
/// 与 Rust 侧 `pgrep -f` 的关系：NSWorkspace 只认识「注册为 App」的进程；
/// 由脚本直接 exec 的同路径进程不在其中，因此 Rust 侧在 `0` 时仍会回退
/// 进程表再确认一次（保守地视为运行中）。
///
/// 注意：`NSWorkspace` 官方建议主线程使用；CLI 无主 runloop，这里直接调用
/// 只读属性 `runningApplications`（实测可用，且不做 `DispatchQueue.main.sync`
/// 以免与 Rust 主线程互相等待导致死锁）。
@_cdecl("thin_is_app_running")
public func thin_is_app_running(_ path: UnsafePointer<CChar>) -> Int32 {
    let target = URL(fileURLWithPath: String(cString: path)).standardizedFileURL.path
    let macosPrefix = target + "/Contents/MacOS/"
    for app in NSWorkspace.shared.runningApplications {
        if let bundle = app.bundleURL?.standardizedFileURL.path, bundle == target {
            return 1
        }
        if let exe = app.executableURL?.standardizedFileURL.path, exe.hasPrefix(macosPrefix) {
            return 1
        }
    }
    return 0
}

/// 读取 .app 的 `CFBundleIdentifier`（替代 `plutil` 子进程）。
///
/// 找不到时返回 NULL；成功返回的 C 字符串由调用方用 `thin_string_free` 释放。
@_cdecl("thin_bundle_id")
public func thin_bundle_id(_ path: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let url = URL(fileURLWithPath: String(cString: path))
    guard let id = Bundle(url: url)?.bundleIdentifier, !id.isEmpty else {
        return nil
    }
    return strdup(id)
}
