# thin — macOS 系统空间清理 App 设计文档

> 设计依据：本次对 suke 的 MacBook Air 的实际扫描流程与发现。
> 目标：把「逐层 du + 分类判断 + 安全删除」这套人工流程，产品化成可信赖的清理工具。

---

## 0. 核心洞察（决定了产品形态）

这次扫描暴露了 3 个关键事实，App 必须正面解决：

1. **macOS「系统数据 / 系统缓存」是兜底分类，不是真实文件夹。**
   本机它把 Parallels(26G) + 系统卷(12.6G) + Preboot(8.8G) + Library(10G) + /private(4.6G) + Homebrew(3.3G) + 安装残留(3.1G) 凑成了「≈70G」。
   → App 不能照抄这个分类，而要还原成**真实路径**。

2. **朴素 `du` 会重复计数。** firmlink、挂载点、APFS clone、外接卷都会让数字虚高（本机 `/System/Volumes/Data` 显示 195G，实际只用 118G）。
   → App 必须做**跨卷隔离 + 去重 + APFS 感知**的核算。

3. **用户真正需要的是「哪些能安全回收」，而不是「什么最大」。**
   本机 118G 里，真正可回收约 20–25G，其余是系统、虚拟机、项目数据。
   → App 的核心指标是 **Reclaimable（可回收）**，且每个条目都要能解释。

**产品一句话：一个只会删除「可再生成 / 明确归属」数据、并且能把每一 MB 都讲清楚的 macOS 清理器。**

---

## 1. 设计原则

| 原则 | 落地方式 |
|---|---|
| 诚实 | 磁盘条按真实路径分类，不用「系统数据」兜底；区分「可回收」与「占用」 |
| 可解释 | 每一项显示：这是什么 / 谁生成的 / 删了会怎样 / 能否恢复 |
| 安全优先 | 受保护路径白名单；默认进废纸篓/隔离区而非 `rm`；全量日志可回滚 |
| 只动可再生 | 绿=可再生、黄=需确认、红=不可再生（默认不勾选） |
| 快速 | 增量索引 + 并行遍历 + 规则短路，首次全盘 < 30s |

---

## 2. 系统架构

```
┌─────────────────────────────────────────────────────────┐
│                        UI (SwiftUI)                       │
│  Dashboard · CategoryList · ItemDetail · CleanConfirm     │
└───────────────▲───────────────────────────┬──────────────┘
                │ ScanReport (Observable)    │ CleanPlan
┌───────────────┴───────────────────────────▼──────────────┐
│                    Core (Swift Package)                   │
│                                                           │
│  DiskProbe ──► Scanner ──► Classifier ──► Estimator       │
│      │            │            ▲              │           │
│      │            │       RuleCatalog         │           │
│      │            │       (JSON, 可热更新)     │           │
│      ▼            ▼                           ▼           │
│  VolumeInfo   FSWalker                  ReclaimReport     │
│                                                           │
│                      SafetyGate ──► Executor              │
│                        │                │                 │
│                   ProtectedPaths    Trash / Quarantine    │
│                   SudoActions       Journal / Undo        │
└───────────────────────────────────────────────────────────┘
```

### 模块职责

- **DiskProbe** — 容量与拓扑：`df`/`diskutil apfs list`、purgeable、APFS 快照、外接卷、Time Machine。解决洞察 #2。
- **Scanner** — 两阶段：① 规则命中（已知路径）② 通用遍历（大文件/Top-N）。
- **RuleCatalog** — 所有「已知可清项」的声明式规则库（见 §4），可随版本更新，无需改代码。
- **Classifier** — 把路径映射到 `(类别, 风险, 可再生性, 回收动作, 解释)`。
- **Estimator** — 计算真正的 reclaimable：排除受保护、跨卷、clone 共享块、快照覆盖。
- **SafetyGate** — 删除前校验：白名单、是否需 sudo、目标 App 是否运行、是否有快照。
- **Executor** — 执行 + 写 Journal（可撤销），默认走「移入隔离区」而不是直接删。
- **Reporter/UI** — Dashboard、分类明细、解释面板。

---

## 3. 扫描流程（把本次人工 flow 固化）

本次我们手动做的步骤，映射为 App 的 pipeline：

```
Step 0  DiskProbe
        ├─ df / diskutil apfs list       → 总容量、已用、可清除
        ├─ listSnapshots                 → 本地 TM 快照（会虚占空间）
        └─ 枚举卷                        → 区分内部盘 / 外接盘 / Time Machine

Step 1  Rule Scan（规则命中，秒级）
        ├─ 已知缓存：~/Library/Caches, /Library/Logs, /var/log
        ├─ 开发缓存：cargo/rustup/npm/pip/homebrew/gradle/Xcode DerivedData
        ├─ 应用缓存：Chrome OptGuide、Ollama、VS Code、Electron App
        ├─ 大对象：Parallels/Docker/VM 镜像
        ├─ 系统项：/private/var/db/diagnostics, sleepimage, macOS Install Data
        └─ 回收站 / 下载

Step 2  Generic Walk（兜底，Top-N）
        └─ 并行遍历，输出 Top-N 大目录 / >1GB 大文件（-x 不跨卷）

Step 3  Classify + Estimate
        ├─ 每个命中路径 → 规则 → (类别, 风险, 可再生, 动作, 解释)
        └─ 核算 reclaimable（去重 / 跨卷隔离 / clone 感知 / 排除 protected）

Step 4  Report
        └─ 按「风险 × 类别」聚合；给出保守的「可回收总量」

Step 5  User Select → CleanPlan（dry-run 预览）

Step 6  SafetyGate → Executor
        ├─ 默认移入隔离区（保留 7 天，可一键恢复）
        ├─ 需 sudo 的走授权弹窗
        └─ 写 Journal + 复查回收空间
```

### 伪代码

```swift
func scan() async throws -> ScanReport {
    let volumes = try await DiskProbe.probe()                 // Step 0
    var hits: [RuleHit] = []

    // Step 1: 规则命中（并发，每规则独立）
    await withTaskGroup { group in
        for rule in RuleCatalog.all where rule.applies(to: volumes) {
            group.addTask { hits.append(contentsOf: await rule.evaluate()) }
        }
    }

    // Step 2: 兜底大对象
    hits += await GenericWalker.topBigItems(excluding: volumes.externalMounts)

    // Step 3: 分类 + 核算
    let items = hits.map { Classifier.classify($0) }
    let reclaimable = Estimator.reclaimable(items, volumes: volumes)

    return ScanReport(items: items, reclaimableBytes: reclaimable)  // Step 4
}
```

---

## 4. 规则目录（Rule Catalog）

规则用 JSON/YAML 声明，随 App 更新，不需要发版改代码。这是 App 能否覆盖新缓存的关键。

### Schema

```jsonc
{
  "id": "rust-target",
  "name": "Rust 编译产物 (target/)",
  "category": "dev-cache",          // system-cache | app-cache | dev-cache | vm | log | trash | leftover
  "risk": "safe",                    // safe | confirm | destructive
  "regenerable": true,
  "match": {
    "type": "glob",                  // glob | exact | dir-rule
    "paths": ["**/target/{debug,release,x86_64-*}"],
    "mustBeInside": ["Cargo.toml"]  // 约束：确认它确实是 cargo 项目产物
  },
  "size": { "method": "du", "excludeVolumes": true },
  "reclaim": { "kind": "command", "cmd": "cargo clean", "cwd": "{projectRoot}" },
  "explain": {
    "what": "Rust 编译产生的中间文件与可执行文件",
    "cost": "删除后下次编译需要重新构建，耗时变长",
    "recover": "重新编译即可，无需从备份恢复"
  }
}
```

### 本机扫描得到的初始规则集（真实数据）

| id | 类别 | 风险 | 可再生 | 本机大小 | reclaim |
|---|---|---|---|---|---|
| `rust-target` | dev-cache | safe | ✅ | 21G | `cargo clean` |
| `chrome-optguide-model` | app-cache | safe | ✅ | 4.0G | 删目录，自动重下 |
| `ollama-models` | app-cache | confirm | ❌ | 4.4G | `ollama rm <model>` |
| `wallpaper-aerials` | system-cache | safe | ✅ | 813M | 删视频缓存 |
| `macos-install-data` | leftover | safe | ❌ | 3.1G | 删升级残留 |
| `diagnostics-db` | log | safe | ✅ | 1.1G | `sudo rm`（需授权） |
| `cargo-registry` | dev-cache | safe | ✅ | 1.3G | `cargo cache -a` |
| `rustup-toolchains` | dev-cache | confirm | ✅ | 1.6G | `rustup toolchain uninstall` |
| `homebrew-cache` | dev-cache | safe | ✅ | — | `brew cleanup` |
| `sleepimage` | system | confirm | ✅ | 2.0G | 关闭休眠/删除，自动重建 |
| `parallels-vm` | vm | destructive | ❌ | 26G | 仅提示，需用户明确确认 |
| `docker-raw` | vm | destructive | ❌ | 74M | Docker 面板 |
| `trash` | trash | safe | ❌ | — | 清空回收站 |
| `android-sdk` | dev-cache | confirm | ❌ | 1.8G | 手动 |

> 规则里的 `mustBeInside` / `detection` 字段很重要：`**/target/**` 只有确认父级有 `Cargo.toml` 才算 Rust 产物，避免误删别的 target 目录。

---

## 5. 数据模型

```swift
struct VolumeInfo {
    let mountPoint: URL
    let role: VolumeRole        // system | data | preboot | external | timeMachine
    let totalBytes, usedBytes: Int64
    let purgeableBytes: Int64
    let snapshots: [SnapshotRef]
    let isExternal: Bool        // ← 外接盘 / TM 必须与内部盘分开统计
}

enum Risk: Int, Comparable { case safe, confirm, destructive }

struct CleanItem: Identifiable {
    let id: String
    let ruleID: String
    let path: URL
    let category: Category
    let risk: Risk
    let regenerable: Bool
    let sizeBytes: Int64
    let reclaimAction: ReclaimAction   // .command / .deletePath / .trash / .sudo
    let explain: Explanation
    var isSelected: Bool = false
}

struct ScanReport {
    let volumes: [VolumeInfo]
    let items: [CleanItem]
    let reclaimableBytes: Int64        // 只统计 safe+confirm 默认项
    let accountedBytes: Int64          // 用于自检：≈ 卷已用量
}
```

**自检机制**：`accountedBytes` 应约等于卷已用量。若差距过大，说明有未覆盖区域，App 主动提示「有 X GB 尚未归类」，而不是偷偷塞进「系统数据」。这是和 macOS 自带分类最大的区别。

---

## 6. 安全模型（最重要）

### 6.1 受保护白名单（永不触碰）

```
/System/**              /System/Volumes/Preboot/**
/private/var/vm/**      系统卷快照
~/Library/Keychains/**  ~/Library/Mobile Documents/**（iCloud 正在同步）
/Library/Apple/**       任何 *.app 包内部
```

### 6.2 分级删除策略

| 风险 | 默认状态 | 执行方式 |
|---|---|---|
| safe | 默认勾选 | 移入 App 隔离区（可恢复 7 天） |
| confirm | 不勾选，需手动 | 二次确认弹窗 + 说明后果 |
| destructive | 不勾选 | 输入名称确认 / 引导到官方入口（如 Docker） |

### 6.3 执行前校验（SafetyGate）

- 目标 App 正在运行 → 提示先退出，不硬删。
- 目标有 APFS 快照覆盖 → 提示「已删除但空间可能不会立即释放」。
- 目标位于外接卷 / Time Machine → 单独提示，不计入「系统缓存」。
- 需要 root 的操作 → 通过 `SMJobBless`/授权框申请，不常驻提权。

### 6.4 可撤销

所有删除写入 **Journal**（时间、规则、路径、大小、去向），隔离区保留 7 天；支持「一键恢复本次清理」。这是用户敢用的前提。

---

## 7. UI / UX

### 7.1 Dashboard

- 顶部：**诚实的磁盘条**，按真实路径分类
  `应用程序 / 文稿 / 媒体 / 开发 / 虚拟机 / 系统(真实占用) / 可清除 / 可用`
- 独立高亮一个 **「可回收 ≈X GB」** 徽标（核心 CTA）。
- 明确标注外接卷（本机 `数据 89G` / `时光机 103G`）为「外部磁盘，非系统缓存」。

### 7.2 清理列表

- 按类别分组，每项一行：图标 · 名称 · 大小 · 风险色点 · 「这是什么?」
- 点击展开解释：`what / cost / recover`（对应规则 explain 字段）。
- 顶部筛选：`仅看安全` / `仅看开发缓存` / `> 100MB`。

### 7.3 确认与执行

- 底部固定：`已选 X 项 · 将释放 ~Y GB` → `开始清理`。
- 执行页实时显示进度、成功/跳过/失败，结束后给「释放前 / 释放后」对比。

### 7.4 「为什么这么大」面板

针对用户困惑（本次的「系统缓存 70G」），提供**归因视图**：
把兜底分类拆成 Top-N 真实路径 + 一句话解释，回答「这 70G 到底是什么」。

---

## 8. 技术栈建议

**推荐：原生 macOS**

| 层 | 选型 |
|---|---|
| UI | SwiftUI（macOS 14+），Charts 画磁盘条 |
| 引擎 | Swift Package `thin`（可单测、可复用为 CLI） |
| 遍历 | `FileManager.enumerator` + `getattrlistbulk` 提速；并行 `TaskGroup` |
| 容量 | `URLResourceValues`、`statfs`；APFS 用 `diskutil`/`DiskArbitration` |
| 快照 | `tmutil`、`diskutil apfs` |
| 授权 | Full Disk Access（非沙盒）；sudo 项用 `Authorization Services` |
| 规则 | 内置 JSON + 远端可更新 |
| 分发 | Sparkle 自更新 / 公证 |

**要点**
- 必须 **Full Disk Access**，否则 `~/Library/Mail`、`.Spotlight` 等读不到，会误报。
- 不要用沙盒（沙盒下无法全盘扫描）。
- 大目录用**增量索引**：首次全扫，之后只重扫变化的规则根目录。

**备选**：Tauri（Rust 引擎 + Web UI，跨平台），代价是 macOS 权限与原生体验略差。

---

## 9. MVP 路线图

| 阶段 | 交付 | 说明 |
|---|---|---|
| **M0 · CLI 引擎** | `thin scan` / `clean --dry-run` | 直接把本次手工流程脚本化，验证规则与核算准确性 |
| **M1 · 只读 App** | SwiftUI Dashboard + 清理列表（不可删） | 先把「诚实分类 + 可解释」做对 |
| **M2 · 安全清理** | 隔离区 + Journal + 撤销 | 只开放 `safe` 类，默认进隔离区 |
| **M3 · 高级** | 大文件/重复文件、App 卸载、Xcode/Homebrew 专项、快照清理 | |
| **M4 · 智能化** | 规则热更新、异常大目录归因、清理建议 | 解决「系统数据又变大了」 |

**M0 优先**：因为本次所有结论都来自 CLI，最快验证准确性的方式就是把它们变成可跑的 `thin`，再套 UI。

> **实现进度（CLI 先行）**：扫描+核算 ✅ / 安全清理（隔离区+Journal）✅ / 大文件·重复文件·App 卸载 ✅。
> 原计划的 SwiftUI 前端顺延，待 CLI 能力稳定后再做（core 已与 UI 解耦，可直接复用）。
