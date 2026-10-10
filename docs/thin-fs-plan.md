# thin-fs 抽象计划 + 模块拆分体检

> 状态：**S0–S4 已落地**（`crates/thin-fs` + `thin-core` 接入 + native 后端）；S5 会话缓存待做。
> 目标是把散落的文件系统遍历收敛成一个独立、只读、可双后端的 crate，
> 为后续「全盘搜索 / 更快的文件检查」打底；并顺带评估 `thin-core` / `thin-cli` 里其他值得拆分的模块。
>
> 相关：[`AGENTS.md`](../AGENTS.md)（安全不变量）、[`docs/swift-ffi.md`](./swift-ffi.md)（native 能力）、
> [`DESIGN.md`](../DESIGN.md)（产品愿景）。

---

## 0. TL;DR

- 新增独立 crate **`thin-fs`**：只读遍历 + 元数据 + 用量聚合 + 查找，**不认识规则与清理语义**。
- **双后端**：`RustBackend`（`walkdir` + `std::fs` + `libc`，永远可用）与 `NativeBackend`
  （macOS `getattrlistbulk(2)`，feature 可选，缺失时回退 Rust）。两后端产出必须**结果一致**。
- 安全不变量（不跨卷、不跟符号链接、只读）**只在 `thin-fs` 实现一次**；删除/移动/保护策略仍归
  `thin-core::clean`。
- 迁移**分 6 步、每步可独立合并**，全程用「新旧等价测试」兜底，不改变现有 CLI 行为。
- 暂不做全盘搜索 CLI 命令（本次非目标），但 `find` API 以「多根」设计，天然是它的基石。

---

## 1. 背景与目标

### 1.1 现状问题（来自代码审查）

所有 FS 遍历散落在 5 处，各自复刻同一批不变量，且写法已漂移：

| 位置 | 函数 | 卷边界判断 |
|---|---|---|
| `thin-core/src/fsutil.rs:69` | `dir_size` | `md.dev() != root_dev \|\| is_mount_point` |
| `thin-core/src/fsutil.rs:118` | `logical_size` | `Some(md.dev()) != root_dev \|\| is_mount_point` |
| `thin-core/src/fsutil.rs:312` | `find_files` | `Some(md.dev()) != root_dev \|\| is_mount_point` |
| `thin-core/src/fsutil.rs:374` | `find_dirs` | **只判 `is_mount_point`，漏 dev** |
| `thin-core/src/finder.rs:32` | `walk_files` | `Some(md.dev()) != root_dev \|\| is_mount_point` |
| `thin-core/src/clean.rs:1014` | `chown_recursive` | 无（全量 walk） |
| `thin-core/src/apps.rs:116` | `collect_nested_bundle_ids` | 无（手写深度递归） |

附带问题：

- **双走**：`platform::LibcPlatform::dir_usage` = `size_of()` + `logical_size()`，目录被完整走两遍。
- **逐条 `stat`**：`WalkDir::metadata()` 每 entry 一个 syscall，未利用 macOS 的 `getattrlistbulk` 批量接口。
- **规范化开销**：`rules::expand_rule_scoped` 对每个命中路径 `canonicalize`（realpath）。
- **错误静默**：`Err(_) => continue` 让「无权限」与「空目录」不可区分，总量可能虚低。
- **逻辑大小不去重**：`logical_size` 未按 inode 去重，硬链接重复计入。
- **无缓存**：一次 `scan` 内同一子树可能被多条规则重复统计。

### 1.2 目标

1. 收敛遍历不变量，消除重复实现与口径漂移。
2. 提供「单遍聚合用量」「按谓词查找」，供 `scan`/`large`/`dupes`/未来搜索复用。
3. 预留 native 批量接口，让「更快的文件检查」可以按 feature 打开。
4. 保持 `thin-core` 的规则/安全语义不被稀释，依赖方向单向：`thin-core → thin-fs`。

### 1.3 非目标（本次）

- ❌ 不新增 `thin search` 之类的全盘搜索 CLI（后续单独做）。
- ❌ 不让 `thin-fs` 执行任何删除/移动/提权。
- ❌ 不引入 Swift 依赖（native 后端用 Rust 直接 `getattrlistbulk`；Swift 能力仍在 `thin-sys`）。
- ❌ 不在同一次改动里重构 `clean.rs`（见第 9 节，另立计划）。

---

## 2. crate 边界与依赖方向

```
                ┌─────────────────────────────┐
                │           thin-cli          │
                └──────────────┬──────────────┘
                               │
                ┌──────────────▼──────────────┐
                │          thin-core          │  rules / clean / scan / recognize ...
                └───────┬──────────────┬──────┘
                        │              │
        ┌───────────────▼───┐   ┌──────▼────────┐
        │      thin-fs      │   │   thin-sys    │ （可选，swift）
        │ 只读遍历/元数据    │   └───────────────┘
        │ /用量/查找/挂载点  │
        └───────────────────┘
```

- `thin-fs` 只依赖：`walkdir`、`libc`、`rayon`（可选）、`rustix`/`nix`（native，可选）。
- `thin-fs` **不得**依赖 `thin-core`（否则成环）。挂载点、进度回调等基础类型在 `thin-fs` 内定义。
- `thin-core/src/fsutil.rs` 逐步退化为「薄 re-export + 平台用量叠加」，对外 API 不变。

---

## 3. `thin-fs` 目录结构

```
crates/thin-fs/
  Cargo.toml
  src/
    lib.rs
    error.rs        // FsError（权限/不存在/IO），不 panic
    mount.rs        // 挂载点集合（getmntinfo 缓存）、卷边界判断
    kind.rs         // Kind / Meta / Entry
    walk.rs         // WalkOptions + Walk::run（唯一的不变量实现处）
    backend/
      mod.rs        // trait Backend + 选择逻辑（OnceLock）
      rust.rs       // walkdir + std::fs 实现（默认）
      native.rs     // macOS getattrlistbulk 实现（feature = "native"）
    usage.rs        // 单遍聚合 Usage
    query.rs        // FindSpec / Predicate / find()
    cache.rs        // 会话级用量缓存（可选，feature 或默认轻量）
  tests/
    walk_invariants.rs
    backend_parity.rs
    usage_equivalence.rs
```

---

## 4. 核心 API 设计

### 4.1 基础类型

```rust
// kind.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind { Dir, File, Symlink, Mount, Volume, Other, Denied }

#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub kind: Kind,
    pub size: u64,   // 逻辑字节
    pub alloc: u64,  // 实际分配字节（st_blocks*512 / totalFileAllocatedSize）
    pub dev: u64,
    pub ino: u64,
    pub mtime: i64,
}

pub struct Entry {
    pub path: PathBuf,
    pub depth: usize,
    pub meta: Option<Meta>,   // None = 元数据不可读（如 TCC 拒绝）
}
```

### 4.2 遍历（唯一的不变量实现处）

```rust
// walk.rs
#[derive(Clone, Debug)]
pub struct WalkOptions {
    pub max_depth: Option<usize>, // None = 不限
    pub follow_links: bool,       // 默认 false
    pub cross_mount: bool,        // 默认 false：不跨卷
    pub same_dev_only: bool,      // 默认 true：与根同设备
}

pub enum Visit { Continue, Skip, Stop }

pub struct Control<'a> {
    pub progress: Option<&'a Progress>,
    pub cancel: &'a AtomicBool,
}

#[derive(Default, Clone, Copy)]
pub struct WalkStats {
    pub entries: u64,
    pub denied: u64,   // 元数据读取失败（权限/TCC）
    pub pruned: u64,   // 因卷边界/深度被跳过
}

impl Walk {
    /// 遍历 roots；所有卷边界、符号链接、深度、错误处理都在此实现一次。
    pub fn run<F>(&self, roots: &[PathBuf], ctl: &Control, visit: F) -> WalkStats
    where F: FnMut(Entry) -> Visit;
}
```

**不变量（只此一处）**：
1. `follow_links(false)` 时用 no-follow 元数据；符号链接只作为 `Symlink` 上报，不下钻。
2. `cross_mount=false` 或 `same_dev_only`：遇到卷边界/挂载点 → 上报并 `Skip`（不下钻）。
3. `depth==0` 的根自身按调用方语义处理（默认上报，调用方可 `Skip`）。
4. 元数据失败 → 计 `denied`，可选上报 `Kind::Denied`，不中断整体。
5. 每处理一条检查 `cancel`，支持取消；`progress` 统一上报。

### 4.3 单遍用量

```rust
// usage.rs
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage { pub allocated: u64, pub logical: u64, pub files: u64 }

/// 一次遍历同时算出 allocated/logical/files（硬链接按 inode 去重）。
pub fn usage(root: &Path, opts: &WalkOptions, ctl: &Control) -> Usage;
```

> iCloud「云占位」(dataless) 在纯 Rust 下无法可靠识别，**不放进 `thin-fs`**；
> 由 `thin-core` 在需要时用 `platform`/`thin-sys` 叠加（保持现有 `fsutil::usage` 行为）。

### 4.4 查找

```rust
// query.rs
pub enum Predicate {
    DirName(String),                    // 目录名精确
    Suffix(Vec<String>),                // 文件后缀（dmg/zip/tar.gz，忽略大小写）
    Sibling(String),                    // 同级存在某标志文件
    MinSize(u64),
    All(Vec<Predicate>),
}

pub struct FindSpec { pub pred: Predicate, pub opts: WalkOptions, pub once: bool }

/// 多根查找；`once` 命中目录后不下钻（对应现有 find_dirs 语义）。
pub fn find(roots: &[PathBuf], spec: &FindSpec, ctl: &Control) -> Vec<PathBuf>;
```

现有 `fsutil::find_files` / `find_dirs` / `finder::walk_files` 都映射到 `find` / `Walk::run`。

### 4.5 后端

```rust
// backend/mod.rs
pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;
    fn walk(
        &self,
        roots: &[PathBuf],
        opts: &WalkOptions,
        emit: &mut dyn FnMut(Entry) -> Visit,
        ctl: &Control,
    ) -> WalkStats;
}

pub fn backend() -> &'static dyn Backend; // OnceLock；依据 cfg/feature 选择
```

- `RustBackend`：`walkdir` 默认实现，永远可用。
- `NativeBackend`：`#[cfg(all(target_os = "macos", feature = "native"))]`，用 `getattrlistbulk`
  批量拿 `ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_DEVID |
  ATTR_CMN_FILEID | ATTR_FILE_ALLOCSIZE | ATTR_FILE_TOTALSIZE | ATTR_CMN_MODTIME |
  ATTR_CMN_ERROR`，逐 batch 回调；不跨卷、不跟链接的语义与 Rust 后端一致。
- 选择策略可加环境变量 `THIN_FS_BACKEND=rust|native` 便于对拍与压测。

---

## 5. 会话级缓存（可选，最后做）

```rust
// cache.rs
pub struct UsageCache { /* canonical path -> (dev, ino, mtime, Usage) */ }
```

- 键：规范化路径；值：`(dev, ino, mtime)` + `Usage`，`mtime` 变了即失效。
- 作用域：一次 `scan`/一次命令内；不进磁盘，不跨进程。
- 直接收益：消除 `scan` 中 `target` / `node_modules` 等规则对同一 home 根的重复遍历。
- 并发：`rayon` 下用分片 `Mutex<HashMap>` 或每线程本地缓存后合并，避免热点锁。

---

## 6. 与现有模块的接线

| 现有 | 迁移后 |
|---|---|
| `fsutil::dir_size` / `size_of` | 调 `thin_fs::usage(..).allocated`（单遍） |
| `fsutil::logical_size` | 调 `thin_fs::usage(..).logical` |
| `fsutil::is_mount_point` / `device_of` | re-export `thin_fs::mount::*` |
| `fsutil::find_files` | `thin_fs::find` + `Predicate::Suffix` |
| `fsutil::find_dirs` | `thin_fs::find` + `Predicate::{DirName,Sibling}` |
| `finder::walk_files` | `thin_fs::Walk::run` |
| `fsutil::usage` | `thin_fs::usage` + `platform` 叠加 dataless |
| `children_sizes` / `children_entries` | 复用 `Walk`（保留现有输出与排序） |
| `clean::chown_recursive` | 暂不动（写操作，非只读库职责） |

`thin-core` 对外 API 保持不变，`thin-cli` 无感。

---

## 7. 迁移路线（每步可独立合并 + 可回退）

| 步骤 | 内容 | 状态 |
|---|---|---|
| **S0** | 新建 `thin-fs` crate 骨架（mount/kind/walk/rust backend），接入 workspace | ✅ |
| **S1** | 实现 `Walk`；用新实现重写 `dir_size`/`logical_size`/`find_files`/`find_dirs`/`walk_files` | ✅（含不变量 + 等价测试） |
| **S2** | 合并为单遍 `usage`，去掉 `size_of`+`logical_size` 双走；修硬链接逻辑去重 | ✅ |
| **S3** | `fsutil` 退化为 re-export 薄层；删除重复遍历代码 | ✅（仅剩 `clean::chown_recursive` 写操作） |
| **S4** | 实现 `NativeBackend`（`getattrlistbulk`）+ `backend_parity` 对拍测试 | ✅（`--features native`） |
| **S5** | 接入会话缓存，验证 `scan` 重复遍历次数下降 | ⏳ |
| **S6** | 清理死代码与文档，更新 `AGENTS.md` 命令索引 | 部分（文档已更新） |

**S1 的等价性测试是安全网**：同一临时树（含符号链接、硬链接、嵌套、不可读目录）跑新旧两套实现，
断言结果相等；此后任何一步回归都会被测出。

---

## 8. 测试策略

- **不变量单测**：`tests/walk_invariants.rs` — 符号链接不跟、硬链接只计一次、跨卷/挂载点剪枝、
  `max_depth` 边界、`Denied` 计数、`cancel` 生效。
- **等价性测试**：旧 `fsutil` vs 新 `thin-fs`，同一合成树逐项比对。
- **性质测试**：无硬链接/无挂载时，`usage(root) == Σ usage(直接子项) + 自身`。
- **后端一致性**：`tests/backend_parity.rs`（`#[cfg(feature="native")]`）两后端结果一致。
- **回归**：现有 `thin-core` 单测全绿；`cargo build -p thin-core --no-default-features`。

---

## 9. 附：其他模块拆分体检

按「职责数 × 体积 × 改动风险」排序，`thin-fs` 是 P0，其余候选如下。

| 优先级 | 模块 | 行数 | 现状职责 | 拆分建议 | 风险 |
|---|---|---|---|---|---|
| **P1** | `thin-core/src/clean.rs` | 1548 | 保护策略 + 计划 + 执行 + 隔离区账本 + 提权 | 拆为 **crate 内子模块**：`clean/policy.rs`、`clean/plan.rs`、`clean/apply.rs`、`clean/journal.rs`、`clean/elevate.rs`，`clean.rs` 只做 facade re-export | 高（安全门核心，必须保持统一入口，逐块搬运 + 对拍） |
| **P1** | `thin-core/src/apps.rs` | 1354 | App 列表/运行态 + token 生成 + 残留匹配 + Darwin 临时目录 + 沙盒容器 | 拆出 `apps/tokens.rs`（`*_token`/`name_tokens`）、`apps/leftovers.rs`（`find_leftovers*`/`candidate_paths`/`condition_allows`）；`apps.rs` 保留列表与运行态 | 中（残留匹配是行为主体，需快照测试） |
| **P2** | `probe.rs` + `platform.rs` + `status.rs` | 248+226+385 | 磁盘探测 / 平台能力 trait / CPU·内存·电池 | 收拢为 `thin-platform`（或 `thin-core/src/sys/`）：`disk.rs`、`platform.rs`、`live.rs` | 中（`Platform` trait 被多处引用，先内部模块化再考虑独立 crate） |
| **P2** | `history.rs` + `preset.rs` + `schedule.rs` + `protect.rs` | 227+305+206+189 | 都在 `~/.thin` 下读写状态 | 收拢为 `thin-core/src/state/`（或 `thin-state` crate）：账本、预设、定时、保护名单 | 中（`preset`/`history` 依赖 `clean`，需保持单向） |
| **P2** | `thin-cli/src/main.rs` | 2865 | 全部子命令 + 参数解析 + 格式化 | 拆为 `thin-cli/src/cmd/<sub>.rs`，`main.rs` 只保留 `Cmd` 分发与公共工具 | 低（纯搬迁，编译期即可发现遗漏） |
| **P3** | `thin-cli/src/tui.rs` | 2788 | TUI 状态机 + 各标签页渲染 | 按标签拆 `tui/clean.rs`、`tui/apps.rs`、`tui/history.rs`、`tui/browse.rs` | 中（状态耦合，需谨慎） |
| **P3** | `thin-core/src/recognize.rs` + `catalog.rs` + `discover.rs` + `spotlight.rs` | 343+264+184+204 | 只读分析/归因 | 收拢为 `thin-core/src/analysis/` | 低 |
| **P3** | `proc.rs` + `fmt.rs` + `progress.rs` | 57+28+101 | 通用工具 | 可选 `thin-util`；也可并入 `thin-fs` 的进度抽象 | 低 |

**说明**：

- 只有 `thin-fs` 建议做**独立 crate**。其余优先做**crate 内子模块拆分**——不动依赖图和对外 API，
  风险最低、review 最容易。
- `clean.rs` 虽然最该拆，但它是安全不变量 #1–#10 的落点，**必须放在 `thin-fs` 稳定之后**，
  且拆完仍需保证「提权单一入口」「dry-run = apply」两条不破。
- `rules.rs`（781）职责相对内聚（装载/保存/展开/安全预检），**暂不拆**；展开逻辑在 S3 后会调用
  `thin-fs`，但规则语义仍在 `rules`。

---

## 10. 开放问题 / 待确认

1. `thin-fs` 的进度类型：复用 `thin-core::progress::Progress`（会造成 `thin-core ↔ thin-fs` 循环），
   还是在 `thin-fs` 定义 `ProgressSink` trait、由 `thin-core` 适配？**计划采用后者（trait + 适配器）**。
2. `NativeBackend` 的 `getattrlistbulk` 是纯 `libc` 还是引入 `rustix`/`nix`？**倾向纯 `libc`**，
   与现有 `probe.rs`/`fsutil.rs` 风格一致，少一个依赖。
3. 会话缓存的失效粒度：仅按 `(dev, ino, mtime)`，还是叠加目录 mtime 链？先做简单版，按需再深化。
4. 基准基线：先记录当前 `thin scan` 的用时与 `dtrace`/`fs_usage` 系统调用量，作为 S5 的对照。
