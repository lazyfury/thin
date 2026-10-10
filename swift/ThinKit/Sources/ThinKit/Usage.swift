import Foundation

/// 一次调用统计整个目录的真实占用，返回 JSON（整批，不逐文件跨 FFI）。
///
/// **热路径不查询 iCloud 状态**：`isUbiquitousItemKey` / `ubiquitousItemDownloadingStatusKey`
/// 会触发逐文件 XPC，实测在 2 万文件目录上慢约 10×。云占位改用按需的
/// [`thin_dir_dataless_json`]（`thin discover --icloud`）。
///
/// 字段：
/// - `allocated`：实际分配字节（优先 `totalFileAllocatedSize`，感知压缩/稀疏）
/// - `logical`：逻辑大小（`totalFileSize` 之和）
/// - `files`：文件数（按 file resource identifier 去重，硬链接只计一次）
/// - `dataless` / `datalessCount`：恒为 0（见 `thin_dir_dataless_json`）
///
/// 不跨越卷边界（遇到其它卷的挂载根即跳过），不跟随符号链接。
/// 成功返回 `strdup` 的 JSON 字符串（调用方用 `thin_string_free` 释放），失败返回 NULL。
@_cdecl("thin_dir_usage_json")
public func thin_dir_usage_json(_ path: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let fm = FileManager.default
    let root = URL(fileURLWithPath: String(cString: path))
    var isDir: ObjCBool = false
    guard fm.fileExists(atPath: root.path, isDirectory: &isDir) else {
        return nil
    }

    var allocated: UInt64 = 0
    var logical: UInt64 = 0
    var files: UInt64 = 0
    let dataless: UInt64 = 0
    let datalessCount: UInt64 = 0

    // 单文件：直接取属性，不启动 enumerator
    if !isDir.boolValue {
        if let (alloc, logicalSize) = fileSizes(root) {
            allocated = alloc
            logical = logicalSize
            files = 1
        }
        return encode(allocated, logical, dataless, files, datalessCount)
    }

    let keys: [URLResourceKey] = [
        .isRegularFileKey,
        .isVolumeKey,
        .totalFileAllocatedSizeKey,
        .fileAllocatedSizeKey,
        .totalFileSizeKey,
        .fileSizeKey,
        .fileResourceIdentifierKey,
    ]
    // 硬链接去重：同一 resource identifier 只计一次
    var seen = Set<String>()

    guard let enumerator = fm.enumerator(
        at: root,
        includingPropertiesForKeys: keys,
        options: [],
        errorHandler: { _, _ in true }
    ) else {
        return encode(0, 0, 0, 0, 0)
    }

    for case let url as URL in enumerator {
        guard let values = try? url.resourceValues(forKeys: Set(keys)) else {
            continue
        }
        // 跳过其它卷的挂载根（APFS 各卷共享 device，不能用 st_dev 判断）
        if values.isVolume == true {
            enumerator.skipDescendants()
            continue
        }
        guard values.isRegularFile == true else {
            continue
        }

        let logicalSize = UInt64(max(0, values.totalFileSize ?? values.fileSize ?? 0))
        logical = logical.saturatingAdd(logicalSize)

        if let id = values.fileResourceIdentifier {
            let key = String(describing: id)
            if seen.insert(key).inserted {
                let alloc = UInt64(max(0, values.totalFileAllocatedSize ?? values.fileAllocatedSize ?? 0))
                allocated = allocated.saturatingAdd(alloc)
                files += 1
            }
        } else {
            let alloc = UInt64(max(0, values.totalFileAllocatedSize ?? values.fileAllocatedSize ?? 0))
            allocated = allocated.saturatingAdd(alloc)
            files += 1
        }
    }

    return encode(allocated, logical, dataless, files, datalessCount)
}

/// 按需统计目录里 iCloud 未下载占位（**逐文件查询，较慢**）。
///
/// 返回 JSON `{"dataless": <逻辑字节>, "datalessCount": <文件数>}`。
/// 与 [`thin_dir_usage_json`] 分离，避免拖慢默认的目录用量统计。
@_cdecl("thin_dir_dataless_json")
public func thin_dir_dataless_json(_ path: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let fm = FileManager.default
    let root = URL(fileURLWithPath: String(cString: path))
    var isDir: ObjCBool = false
    guard fm.fileExists(atPath: root.path, isDirectory: &isDir), isDir.boolValue else {
        return nil
    }

    let keys: [URLResourceKey] = [
        .isRegularFileKey,
        .isVolumeKey,
        .isUbiquitousItemKey,
        .ubiquitousItemDownloadingStatusKey,
        .totalFileSizeKey,
        .fileSizeKey,
    ]

    var dataless: UInt64 = 0
    var datalessCount: UInt64 = 0

    guard let enumerator = fm.enumerator(
        at: root,
        includingPropertiesForKeys: keys,
        options: [],
        errorHandler: { _, _ in true }
    ) else {
        return encodeDataless(0, 0)
    }

    for case let url as URL in enumerator {
        guard let values = try? url.resourceValues(forKeys: Set(keys)) else {
            continue
        }
        if values.isVolume == true {
            enumerator.skipDescendants()
            continue
        }
        guard values.isRegularFile == true else {
            continue
        }
        guard values.isUbiquitousItem == true,
              values.ubiquitousItemDownloadingStatus == .notDownloaded else {
            continue
        }
        let logical = UInt64(max(0, values.totalFileSize ?? values.fileSize ?? 0))
        dataless = dataless.saturatingAdd(logical)
        datalessCount += 1
    }

    return encodeDataless(dataless, datalessCount)
}

/// 单个文件：返回 (allocated, logical)
private func fileSizes(_ url: URL) -> (UInt64, UInt64)? {
    let keys: Set<URLResourceKey> = [
        .isRegularFileKey,
        .totalFileAllocatedSizeKey,
        .fileAllocatedSizeKey,
        .totalFileSizeKey,
        .fileSizeKey,
    ]
    guard let values = try? url.resourceValues(forKeys: keys), values.isRegularFile == true else {
        return nil
    }
    let alloc = UInt64(max(0, values.totalFileAllocatedSize ?? values.fileAllocatedSize ?? 0))
    let logical = UInt64(max(0, values.totalFileSize ?? values.fileSize ?? 0))
    return (alloc, logical)
}

private func encode(
    _ allocated: UInt64,
    _ logical: UInt64,
    _ dataless: UInt64,
    _ files: UInt64,
    _ datalessCount: UInt64
) -> UnsafeMutablePointer<CChar>? {
    let obj: [String: UInt64] = [
        "allocated": allocated,
        "logical": logical,
        "dataless": dataless,
        "files": files,
        "datalessCount": datalessCount,
    ]
    guard let data = try? JSONSerialization.data(withJSONObject: obj),
          let s = String(data: data, encoding: .utf8)
    else {
        return nil
    }
    return strdup(s)
}

private func encodeDataless(_ dataless: UInt64, _ count: UInt64) -> UnsafeMutablePointer<CChar>? {
    let obj: [String: UInt64] = [
        "dataless": dataless,
        "datalessCount": count,
    ]
    guard let data = try? JSONSerialization.data(withJSONObject: obj),
          let s = String(data: data, encoding: .utf8)
    else {
        return nil
    }
    return strdup(s)
}

private extension UInt64 {
    func saturatingAdd(_ other: UInt64) -> UInt64 {
        let (r, overflow) = addingReportingOverflow(other)
        return overflow ? UInt64.max : r
    }
}
