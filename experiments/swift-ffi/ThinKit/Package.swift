// swift-tools-version:5.9
import PackageDescription

// ThinKit：给 Rust CLI 提供 macOS 底层能力的 Swift 库。
// 通过 @_cdecl 导出纯 C ABI，由 crates/thin-sys 链接。
let package = Package(
    name: "ThinKit",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "ThinKit", type: .static, targets: ["ThinKit"])
    ],
    targets: [
        .target(name: "ThinKit")
    ]
)
