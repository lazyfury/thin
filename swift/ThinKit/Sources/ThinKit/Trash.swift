import Foundation

/// 把路径移入系统废纸篓（Finder 可恢复），替代 thin 自带隔离区的可选模式。
///
/// 返回：`0` 成功 / `1` 失败（路径不存在、无权限、跨卷等）。
@_cdecl("thin_trash_item")
public func thin_trash_item(_ path: UnsafePointer<CChar>) -> Int32 {
    let url = URL(fileURLWithPath: String(cString: path))
    var resulting: NSURL?
    do {
        try FileManager.default.trashItem(at: url, resultingItemURL: &resulting)
        return 0
    } catch {
        return 1
    }
}
