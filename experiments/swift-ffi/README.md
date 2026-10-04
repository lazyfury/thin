# Swift FFI POC

验证「Swift Package 导出 C ABI → Rust 链接调用」这条链路是否可行，用于
`thin` 接入 macOS 底层能力（详见 `docs/swift-ffi.md`）。

## 目录

- `ThinKit/` —— Swift Package，`@_cdecl` 导出 `thin_available_capacity` /
  `thin_version` / `thin_string_free`。
- `thin-sys-demo/` —— 独立 Rust crate（不属于仓库根 workspace），`build.rs`
  调 `swift build` 并链接 `libThinKit.a`，`main.rs` 对比 `NSURL` 与 `statfs`。

## 运行

```bash
cd experiments/swift-ffi/thin-sys-demo
cargo run --release
# 可选：指定路径
cargo run --release -- /System/Volumes/Data
```

依赖 Xcode / Command Line Tools（提供 `swift`）。

## 实测

```
Swift 库: ThinKit 0.1 (POC)
路径: /
NSURL  available=170.9 GB  important(含 purgeable)=175.6 GB  opportunistic=160.2 GB
statfs bavail=170.9 GB
purgeable(important-available)≈4.6 GB
```

`.build/`（约 70 MB 中间产物）与 `target/` 已被 `.gitignore` 忽略。
