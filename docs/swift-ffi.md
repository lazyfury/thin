# Swift FFI 落地计划（方向 A：Swift 提供 macOS 底层能力给 Rust CLI）

> 状态：**M0 / M1 / M2 / M3 已落地**（`swift/ThinKit` + `crates/thin-sys` + `thin-core/src/platform.rs`）。
> `thin probe` 显示 purgeable 与 FDA 状态；App 状态/bundle id 接入 NSWorkspace/Bundle；
> `thin discover` 用批量目录用量显示实际占用与 iCloud 云占位；
> `thin uninstall` 用代码签名 entitlements 精确定位沙盒/group 容器；`thin clean --trash` 走系统废纸篓。
> POC 回归样例保留在 `experiments/swift-ffi/`。

## 0. 目标

`thin` 现在是纯 Rust，绕过 macOS 框架的方式主要是 **shell-out**（`tmutil` / `plutil` /
`pgrep` / `mount`）和 `libc::statfs`。这带来三类问题：

1. **不准确**：`statfs` 不含 purgeable 空间，和 Finder「可用」对不上；`metadata.len()` 是逻辑大小，APFS 压缩/稀疏/clone 会虚高。
2. **脆弱**：解析 `mount` 文本、`pgrep -f` 猜路径都有假阳性/假阴性。
3. **能力缺失**：Full Disk Access 自检、iCloud 占位文件识别、沙盒容器残留、系统废纸篓、特权助手等根本做不到。

目标：加一层 **Swift Package（ThinKit）**，用 `@_cdecl` 导出纯 C ABI，由新的 Rust crate
`thin-sys` 链接，在 `thin-core` 里以 **capability trait** 接入，保留纯 Rust 回退。

约束：
- 全部限定 macOS（`#[cfg(target_os = "macos")]` + cargo feature），非 macOS 与 CI 仍可编可测。
- 不引入 App Sandbox（会废掉全盘扫描能力）。
- 不做 GUI（方向 B 另行讨论）。

## 1. POC 与 M0 实测结论（已完成）

- 正式结构：`swift/ThinKit`（Swift Package）+ `crates/thin-sys`（Rust 绑定，workspace 成员）。
- POC 回归样例：`experiments/swift-ffi/thin-sys-demo`（独立 crate，直接链接 `swift/ThinKit`）。
- 接线：`thin-core` 的 `probe::capacity()`（feature `swift`，默认开启）优先走 Swift，缺失时回退 `statfs`；
  `thin probe` 已打印 purgeable。

实现：`NSURL.volumeAvailableCapacityForImportantUsageKey` 等 4 个资源键 → `@_cdecl`
→ Rust `build.rs` 调 `swift build` 并链接静态库。

实测（macOS 26.7 · Swift 6.4 · arm64 · `thin-sys-demo --release`）：

```
路径: /
NSURL  available=170.9 GB  important(含 purgeable)=175.6 GB  opportunistic=160.2 GB
statfs bavail=170.9 GB
purgeable(important-available)≈4.6 GB
```

| 观察 | 数值 | 含义 |
|---|---|---|
| ThinKit 静态库 | 23 KB | 加一层 FFI 的实际体积成本很小 |
| 最终二进制 | 473 KB（demo） | 主要胖在链接 Foundation/Swift runtime |
| 运行时依赖 | `/usr/lib/swift/libswiftCore.dylib` 等（弱链接） | 系统自带，无需自带 runtime |
| 跨 cwd 运行 | ✅ | `-Wl,-rpath,/usr/lib/swift` 生效 |

**结论：链路可行，且能立刻拿到 `statfs` 给不出的 purgeable 数据。**

构建要点（已固化为 `build.rs`）：
- 用 `swift build --show-bin-path` 拿产出目录（不同 SwiftPM 版本布局不同：`.build/release` vs `.build/out/Products/Release`）。
- `cargo:rustc-link-search=native=<bin>` + `cargo:rustc-link-lib=static=ThinKit`。
- 静态 Swift 库靠 `.o` 里的 `LC_LINKER_OPTION` 自动补 `-lswiftCore`；Rust 侧补 `/usr/lib/swift` 搜索路径与 rpath 即可。
- `.build/` 中介物约 70 MB，已在 `.gitignore` 忽略。

复现：
```bash
cd experiments/swift-ffi/thin-sys-demo && cargo run --release
```

## 2. 目标架构（M0 已按此落地）

```
swift/ThinKit/                     # 已从 experiments 提升
  Package.swift                    # platforms: [.macOS(.v13)]
  Sources/ThinKit/
    FFI.swift                      # 所有 @_cdecl 入口集中于此
    Volumes.swift  Apps.swift  Perms.swift  FS.swift

crates/thin-sys/                   # 正式绑定（workspace member，feature = "swift"）
  build.rs                         # swift build + link；缺工具链时优雅降级
  src/lib.rs                       # 安全封装：extern "C" → Result<_, ThinSysError>

crates/thin-core/src/platform.rs   # trait Platform + LibcPlatform（纯 Rust 回退）
```

分层原则：

```rust
// thin-core 只认 trait，不认识 Swift
pub trait Platform: Send + Sync {
    fn volume_capacity(&self, path: &Path) -> Option<VolumeCapacity>;
    fn is_app_running(&self, bundle_id: &str) -> bool;
    fn full_disk_access(&self) -> FdaStatus;
    fn allocated_size(&self, path: &Path) -> Option<u64>;
}

pub struct LibcPlatform;   // 现有 statfs / fs 逻辑，永远可用
pub struct MacPlatform;    // thin-sys 包装的 Swift 后端（feature = "swift"）
```

`thin-cli` 在 macOS 下默认启用 `swift` feature，其余平台走 `LibcPlatform`。

## 3. FFI 契约规范

1. **命名**：`thin_<动作>`，`@_cdecl` 导出；`#[repr(C)]` 只用于扁平 struct，复杂数据用 JSON。
2. **错误**：返回 `Int32`，`0` 成功，负数为错误码（`-1` 参数非法 / `-2` 不存在 / `-3` 权限 / `-99` 未知）。
3. **字符串**：UTF-8 C 字符串，Swift `strdup` 分配，Rust 侧必须调 `thin_string_free`。
4. **列表**：跨 FFI **整批 JSON**，不要逐文件往返。例如 `thin_dir_usage_json(path, opts) -> char*`。
   热路径（每文件 allocated size）由 Swift 端内部批量完成，Rust 只收聚合结果。
5. **线程**：只读 Foundation API 允许后台线程；涉及主线程的用 `DispatchQueue.main.sync`，并保证不与 TUI 死锁（读操作优先不回落主线程）。
6. **ABI 版本**：`thin_abi_version() -> u32`，`thin-sys` 启动时校验，避免 Swift/Rust 版本错配。

## 4. 构建与分发策略

| 方案 | 做法 | 取舍 |
|---|---|---|
| **静态库 + 系统 Swift runtime**（POC 采用，推荐先做） | 链接 `libThinKit.a`，rpath `/usr/lib/swift` | 单文件、无需自带 runtime；依赖系统版本 |
| 动态库随包 | 出 `libThinKit.dylib`，`@executable_path/../lib` | 便于单独升级；分发要多带一个文件 |
| 全静态 Swift runtime | `--static-swift-stdlib` | 自包含但体积显著变大 |

- 最低系统版本 **macOS 13+**（`SMAppService`、`NSBackgroundActivityScheduler` 等较新 API 需要）。
- 签名 + 公证（不沙盒）。CI 可缓存 `.build` 与预编译 `libThinKit.a`。
- `thin-sys` 的 `build.rs` 在检测不到 `swift` 时：不链接并置 `cfg(thin_no_swift)`，让上层退回 `LibcPlatform`，保证 `cargo test` 在任何环境可跑。

## 5. 能力路线图与验收

### M0 · FFI 骨架 + 卷容量 ✅
- 动机：`probe.rs` 原用 `statfs`，看不到 purgeable。
- API：`NSURL` 卷资源键。
- 落地：`swift/ThinKit` + `crates/thin-sys`（`build.rs` 无 Swift 时自动降级）+ `thin-core`
  `probe::capacity()`（feature `swift`）；`thin probe` 实测输出：
  ```
  APFS 容器    容量 228.3 GB  已用 57.9 GB  可用 170.4 GB  (25%)
  可回收空间(purgeable) 4.6 GB  ·  含 purgeable 可用 175.1 GB
  ```
- 验收：`cargo test` 全绿（含 `thin-sys` FFI 用例）；`cargo build -p thin-core --no-default-features`
  与「假 swift」环境均验证降级可编。
- 回退：`LibcPlatform::volume_capacity`（`statfs`）。

### M1 · App 状态与权限 ✅
- ✅ `thin-core/src/platform.rs`：`Platform` trait + `LibcPlatform` 回退 + `SwiftPlatform`；
  `probe::capacity()` 收进 trait，`full_disk_access()` 对外暴露。
- ✅ `NSWorkspace.runningApplications`：新增 `thin_is_app_running`；`apps::is_running` 以
  **NSWorkspace ∪ pgrep** 判定（NSWorkspace 只认识注册 App，脚本直接 exec 的同路径进程由 pgrep 补充）。
- ✅ `Bundle(url:).bundleIdentifier`：新增 `thin_bundle_id`；`apps::bundle_id` 优先 Swift，回退 `plutil`。
- ✅ **Full Disk Access 自检**：新增 `thin_full_disk_access`（实际尝试打开 TCC.db / Safari / Messages）；
  `thin probe` 打印授权状态，`thin scan` 在未授权时给出提示与路径。
- 验收：`cargo test` 全绿（含 `bundle_id == com.apple.calculator`、Finder 运行中检测）；
  实跑确认无 FDA 时 `thin probe`/`thin scan` 均提示，而非静默扫成 0 B。
- objc2 备选：`objc2-app-kit` / `objc2-foundation` 可完全覆盖；FDA 纯 Rust 即可。
- ABI 升至 2。

### M2 · 容量核算更诚实 ✅
- ✅ `thin_dir_usage_json`：**一次 FFI 调用走完整个目录**（不逐文件跨 FFI），返回
  `{allocated, logical, dataless, files, datalessCount}`；`allocated` 取
  `totalFileAllocatedSize`（感知压缩/稀疏），硬链接按 resource identifier 去重，不跨卷、不跟符号链接。
- ✅ `thin-core`：`fsutil::Usage` + `fsutil::usage()`；`Platform::dir_usage()`（Libc 回退为
  `size_of`/`logical_size`）。
- ✅ `discover` 接入：展示每项的「云占位」与合计 iCloud 未下载占用。
- **实测校验**：Swift `allocated` 与 `du -sk` **完全一致**：
  - `target` → 2 733 776 896 B = 2 669 704 KB = `du`
  - `/System/Applications` → 726 589 440 B = 709 560 KB = `du`
- **实测 iCloud**：`~/Library/Mobile Documents` allocated=613.3 MB、logical=818.8 MB、
  dataless=235.1 MB；`discover` 正确把 `com~apple~CloudDocs` 的 197 MB 标为「云占位」且不计入合计。
- **scan 热路径不替换**（有意取舍）：`st_blocks` 已等价于 allocated 且更快；`usage()` 保留给需要
  iCloud/逻辑体积的场景（discover、未来面板）。ABI 升至 3。
- objc2 备选：`getattrlistbulk` 可直接用 Rust syscall，未必需要 Swift；iCloud 状态仍需 Foundation。

### M3 · 卸载与删除语义 ✅（NSFileCoordinator 暂缓）
- ✅ `thin_app_sandbox_info_json`（Security 框架读代码签名 entitlements）：返回
  `{bundleId, sandboxed, groups}`；`find_leftovers` 据此精确补充
  `~/Library/Containers/<bundle-id>` 与 `~/Library/Group Containers/<group-id>`，
  不再只靠名称猜测（group id 常与 bundle id 无关）。
- ✅ `thin_trash_item`（`FileManager.trashItem`）+ `clean::trash()`：`thin clean --trash`
  用系统废纸篓替代内置隔离区，与隔离区**共用同一安全门**；后端不可用时整体报错，绝不回退 `rm`。
- 实测：`iShot Pro` 预览正确列出 `Group Containers/4K6FWZU8C4.group.cn.better365`；
  `thin clean --trash --apply` 成功移动并写入历史（`manual-trash`）。ABI 升至 4。
- `NSFileCoordinator` 暂缓：缓存/残留目录极少被其他进程持续持有，收益低于复杂度；
  现有 `clean.rs` 的 `deny delete` ACL 退化路径已能处理占用场景。
- objc2 备选：`objc2-foundation` 覆盖 `trashItem`/`NSFileCoordinator`；entitlements 需 Security。

### M4（可选，大工程） · 调度与特权
- `SMAppService` 替代手写 launchd plist（`schedule.rs`）——注意需 bundle/已批准的 plist，纯 CLI 受限，可能保留 launchd。
- 特权 helper 清理 `/Library` 缓存，免反复 sudo。
- `FSEvents` 做 `thin watch` / TUI 实时体积。
- 验收：调度安装/卸载有明确状态；特权路径可一次授权复用。

## 6. objc2 备选：什么时候不用 Swift

| 能力 | 纯 Rust（objc2/syscall） | 结论 |
|---|---|---|
| 卷容量（含 purgeable） | `objc2-foundation` NSURL 资源键 | 可不用 Swift |
| 运行中 App / bundle id | `objc2-app-kit` / `objc2-foundation` | 可不用 Swift |
| 实际占用 | `getattrlistbulk` 直接 syscall | 可不用 Swift |
| FDA 自检 | 普通文件可读性判断 | 可不用 Swift |
| NSMetadataQuery / FSEvents 封装 | 绑定冗长、async 难写 | **用 Swift** |
| SwiftUI / 菜单栏（方向 B） | 不可替代 | **必须 Swift** |
| 未来 SMAppService/helper 复杂流程 | 绑定可能缺失 | **用 Swift** |

策略：**批量、复杂、易变的系统交互用 Swift；单次薄 shim 若 objc2 已足够则不必引入 Swift**。两者都在 `Platform` trait 后面，可逐项切换。

## 7. 提升步骤（POC → 正式，M0–M3 已完成）

1. ✅ `git mv experiments/swift-ffi/ThinKit swift/ThinKit`；gitignore 加 `swift/**/.build`。
2. ✅ 新建 `crates/thin-sys`（workspace member），无 Swift / 非 macOS 自动降级为空实现。
3. ✅ `thin-core/src/platform.rs`（`Platform` trait + `LibcPlatform`/`SwiftPlatform`），
   能力均收进 trait；`probe::capacity`、`fsutil::usage`、`apps`、`clean::trash` 均经此入口。
4. ✅ `thin probe` 接入 `capacity()`，实测显示 purgeable。
5. ✅ POC 保留为 `experiments/swift-ffi/thin-sys-demo` 回归样例；`thin-sys` 内另有 `#[test]`。
6. ✅ `README.md` 增加构建前置说明（Swift 可选、缺失自动降级）。

### 后续 M4（可选）预备
- `SMAppService` 替代手写 launchd plist（注意纯 CLI 的 bundle 限制）。
- 特权 helper 清理 `/Library` 缓存，免反复 sudo。
- `FSEvents` 做 `thin watch` / TUI 实时体积。

## 8. 风险与回退

| 风险 | 缓解 |
|---|---|
| Swift 运行时链接/分发差异 | POC 已用静态库 + `/usr/lib/swift` rpath 验证；保留 dylib/全静态两条备选 |
| 工具链缺失导致构建失败 | `thin-sys` feature-gate + 编译期探测 + `LibcPlatform` 回退 |
| Foundation 主线程约束 | 读操作不回落主线程；必要时同步主队列并加超时 |
| 二进制体积增长 | 当前增量小（库 23 KB）；以 feature 隔离，非 macOS 不带 |
| API 版本漂移 | `thin_abi_version` 校验 + 每项独立验收回退 |
| 过度依赖 Swift | objc2 备选表逐项兜底 |
