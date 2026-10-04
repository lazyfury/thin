import Foundation
import Security

/// 读取 .app 的沙盒信息（来自代码签名 entitlements），返回 JSON：
/// `{"bundleId": "...", "sandboxed": true, "groups": ["group....", ...]}`
///
/// 用途：卸载时精确定位沙盒数据目录，而不是靠名称猜测：
/// - `sandboxed == true` → `~/Library/Containers/<bundle-id>`
/// - 每个 group id → `~/Library/Group Containers/<group-id>`
///
/// 未签名 / 无 entitlements 时 `sandboxed=false`、`groups=[]`（不视为失败）。
/// 成功返回 `strdup` 的 JSON（调用方用 `thin_string_free` 释放），失败返回 NULL。
@_cdecl("thin_app_sandbox_info_json")
public func thin_app_sandbox_info_json(_ path: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let url = URL(fileURLWithPath: String(cString: path))
    var result: [String: Any] = [
        "sandboxed": false,
        "groups": [String](),
    ]
    if let id = Bundle(url: url)?.bundleIdentifier {
        result["bundleId"] = id
    }

    var code: SecStaticCode?
    if SecStaticCodeCreateWithPath(url as CFURL, [], &code) == errSecSuccess, let code {
        var info: CFDictionary?
        let flags = SecCSFlags(rawValue: kSecCSSigningInformation)
        if SecCodeCopySigningInformation(code, flags, &info) == errSecSuccess,
           let dict = info as? [String: Any],
           let ent = dict[kSecCodeInfoEntitlementsDict as String] as? [String: Any]
        {
            result["sandboxed"] = (ent["com.apple.security.app-sandbox"] as? Bool) ?? false
            if let groups = ent["com.apple.security.application-groups"] as? [String] {
                result["groups"] = groups
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
