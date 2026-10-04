import Foundation
import Security

/// 读取 .app 的沙盒信息（来自代码签名 entitlements），返回 JSON：
/// `{"bundleId": "...", "sandboxed": true, "groups": [...],
///   "icloudContainers": [...], "teamId": "..."}`
///
/// 用途：卸载时精确定位沙盒数据目录，而不是靠名称猜测：
/// - `sandboxed == true` → `~/Library/Containers/<bundle-id>`
/// - 每个 group id → `~/Library/Group Containers/<group-id>`
/// - 每个 iCloud container id → iCloud 容器目录
/// - `teamId` → Group Containers 常见前缀（`<team-id>.group.<...>`）
///
/// 未签名 / 无 entitlements 时 `sandboxed=false`、列表为空（不视为失败）。
/// 成功返回 `strdup` 的 JSON（调用方用 `thin_string_free` 释放），失败返回 NULL。
@_cdecl("thin_app_sandbox_info_json")
public func thin_app_sandbox_info_json(_ path: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let url = URL(fileURLWithPath: String(cString: path))
    var result: [String: Any] = [
        "sandboxed": false,
        "groups": [String](),
        "icloudContainers": [String](),
    ]
    if let id = Bundle(url: url)?.bundleIdentifier {
        result["bundleId"] = id
    }

    var code: SecStaticCode?
    if SecStaticCodeCreateWithPath(url as CFURL, [], &code) == errSecSuccess, let code {
        var info: CFDictionary?
        let flags = SecCSFlags(rawValue: kSecCSSigningInformation)
        if SecCodeCopySigningInformation(code, flags, &info) == errSecSuccess,
           let dict = info as? [String: Any]
        {
            if let team = dict[kSecCodeInfoTeamIdentifier as String] as? String, !team.isEmpty {
                result["teamId"] = team
            }
            if let ent = dict[kSecCodeInfoEntitlementsDict as String] as? [String: Any] {
                result["sandboxed"] = (ent["com.apple.security.app-sandbox"] as? Bool) ?? false
                if let groups = ent["com.apple.security.application-groups"] as? [String] {
                    result["groups"] = groups
                }
                if let icloud = ent["com.apple.developer.icloud-container-identifiers"] as? [String] {
                    result["icloudContainers"] = icloud
                }
            }
        }
    }

    guard let data = try? JSONSerialization.data(withJSONObject: result),
          let s = String(data: data, encoding: .utf8)
    else {
        return nil
    }
    return strdup(s)
}

/// 枚举 `<home>/Library/Containers` 下所有沙盒容器，返回 JSON 数组：
/// `[{"path": "/Users/x/Library/Containers/com.foo.bar", "identifier": "com.foo.bar"}, ...]`
///
/// 沙盒容器的目录名**未必是 bundle id**（可能是 UUID）。权威标识来自
/// 目录内的 `.com.apple.containermanagerd.metadata.plist` 的
/// `MCMMetadataIdentifier`；缺失时回退为目录名。
///
/// 这是「一次 FFI 调用整批返回」的批量接口：Rust 侧缓存该映射后，
/// 每个 App 按 bundle id 查表即可，避免逐个 App 反复遍历 Containers。
@_cdecl("thin_sandbox_containers_json")
public func thin_sandbox_containers_json(_ home: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let homeURL = URL(fileURLWithPath: String(cString: home), isDirectory: true)
    let containers = homeURL.appendingPathComponent("Library/Containers", isDirectory: true)

    guard let entries = try? FileManager.default.contentsOfDirectory(
        at: containers,
        includingPropertiesForKeys: nil,
        options: [.skipsHiddenFiles]
    ) else {
        return strdup("[]")
    }

    var result: [[String: String]] = []
    for dir in entries {
        let name = dir.lastPathComponent
        var identifier = name
        let meta = dir.appendingPathComponent(".com.apple.containermanagerd.metadata.plist")
        if let dict = NSDictionary(contentsOf: meta),
           let id = dict["MCMMetadataIdentifier"] as? String,
           !id.isEmpty {
            identifier = id
        }
        result.append(["path": dir.path, "identifier": identifier])
    }

    guard let data = try? JSONSerialization.data(withJSONObject: result),
          let s = String(data: data, encoding: .utf8)
    else {
        return nil
    }
    return strdup(s)
}
