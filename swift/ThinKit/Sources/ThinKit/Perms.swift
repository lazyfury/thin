import Foundation

/// 自检「完全磁盘访问权限（Full Disk Access, FDA）」。
///
/// 返回：`1` 已授权 / `0` 已受限 / `-1` 无法判断（探测路径均不存在）。
///
/// 原理：TCC 受保护路径（如用户的 `TCC.db`、`~/Library/Safari`）只有在授予
/// FDA 后才能读取。逐个尝试真正打开文件，能打开即视为已授权。
@_cdecl("thin_full_disk_access")
public func thin_full_disk_access() -> Int32 {
    let fm = FileManager.default
    let home = fm.homeDirectoryForCurrentUser
    let probes: [URL] = [
        home.appendingPathComponent("Library/Application Support/com.apple.TCC/TCC.db"),
        URL(fileURLWithPath: "/Library/Application Support/com.apple.TCC/TCC.db"),
        home.appendingPathComponent("Library/Safari"),
        home.appendingPathComponent("Library/Messages"),
    ]
    var sawExisting = false
    for url in probes where fm.fileExists(atPath: url.path) {
        sawExisting = true
        if let handle = try? FileHandle(forReadingFrom: url) {
            try? handle.close()
            return 1
        }
        // 目录形式（Safari/Messages）：列出内容同样会触发 TCC 校验
        if let items = try? fm.contentsOfDirectory(atPath: url.path), !items.isEmpty {
            return 1
        }
    }
    return sawExisting ? 0 : -1
}
