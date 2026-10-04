import Foundation

/// POC：C ABI 入口。
///
/// 约定（后续 thin-sys 沿用）：
/// - 标量用 out-param，返回 `Int32` 错误码（0 = 成功）；
/// - 字符串为 UTF-8 C 字符串，由 Swift `strdup` 分配，Rust 侧必须调
///   `thin_string_free` 释放；
/// - 路径一律按 UTF-8 C 字符串传入。

/// 返回路径所在卷的容量信息。
///
/// 与 `libc::statfs` 的关键差异：`important` / `opportunistic` 是 Apple
/// 推荐的「可用容量」，**包含可被系统回收的 purgeable 空间**，因此通常
/// 大于 `statfs` 的 `f_bavail`——这也是 Finder 与 `df` 显示不一致的原因。
@_cdecl("thin_available_capacity")
public func thin_available_capacity(
    _ path: UnsafePointer<CChar>,
    _ total: UnsafeMutablePointer<UInt64>,
    _ available: UnsafeMutablePointer<UInt64>,
    _ important: UnsafeMutablePointer<UInt64>,
    _ opportunistic: UnsafeMutablePointer<UInt64>
) -> Int32 {
    let url = URL(fileURLWithPath: String(cString: path))
    do {
        let values = try url.resourceValues(forKeys: [
            .volumeTotalCapacityKey,
            .volumeAvailableCapacityKey,
            .volumeAvailableCapacityForImportantUsageKey,
            .volumeAvailableCapacityForOpportunisticUsageKey,
        ])
        total.pointee = UInt64(max(0, values.volumeTotalCapacity ?? 0))
        available.pointee = UInt64(max(0, values.volumeAvailableCapacity ?? 0))
        // `ForImportantUsage` / `ForOpportunisticUsage` 是 Int64（字节），偶有 -1 表示未知
        important.pointee = UInt64(max(0, values.volumeAvailableCapacityForImportantUsage ?? 0))
        opportunistic.pointee = UInt64(max(0, values.volumeAvailableCapacityForOpportunisticUsage ?? 0))
        return 0
    } catch {
        return 1
    }
}

/// FFI ABI 版本；Rust 侧 `thin-sys` 启动时校验，避免 Swift/Rust 版本错配。
@_cdecl("thin_abi_version")
public func thin_abi_version() -> UInt32 {
    5
}

/// 库版本字符串，用于验证「Swift 分配内存 → Rust 读取 → Rust 释放」这条链路。
@_cdecl("thin_version")
public func thin_version() -> UnsafeMutablePointer<CChar>? {
    strdup("ThinKit 0.1 (POC)")
}

/// 释放由 ThinKit 分配的 C 字符串。
@_cdecl("thin_string_free")
public func thin_string_free(_ ptr: UnsafeMutablePointer<CChar>?) {
    free(ptr)
}
