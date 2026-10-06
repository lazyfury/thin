<div align="center">

# thin

**macOS 系统空间扫描与安全清理 CLI + TUI**

> 清理 = 移入系统废纸篓（可恢复），而不是永久删除；`--quarantine` 可改用 thin 隔离区。
> 每一项都说清「这是什么 / 删了会怎样 / 能不能恢复」。

![platform](https://img.shields.io/badge/platform-macOS-000000?logo=apple&logoColor=white)
![rust](https://img.shields.io/badge/rust-1.88%2B-dea584?logo=rust&logoColor=white)
![license](https://img.shields.io/badge/license-MIT-blue)
![PRs](https://img.shields.io/badge/PRs-welcome-brightgreen)

[为什么做 thin](#为什么做-thin) · [快速开始](#快速开始) · [命令速查](#命令速查) · [安全模型](#安全模型) · [规则系统](#规则系统) · [路线图](#路线图)

设计文档 [DESIGN.md](./DESIGN.md) · Agent 提示词（用户）[docs/agent-prompt.md](./docs/agent-prompt.md) · 开发指南 [AGENTS.md](./AGENTS.md)

</div>

---

## 为什么做 thin

macOS 的「系统数据 / 系统缓存」是个兜底分类，会把虚拟机、系统卷、缓存、Homebrew 全算在一起，
动辄显示几十 GB，却说不清是什么。thin 反着来：

- 🧭 **还原真实路径** —— 每一项都注明「这是什么 / 删了会怎样 / 能否恢复」
- 🧮 **诚实核算** —— 按实际分配块统计（稀疏文件不虚高）、按 inode 去重（硬链接不重复）、排除嵌套重复路径
- 🎚️ **风险分级** —— `安全` / `需确认` / `不可再生`，默认只把前两类计入「可回收」
- ♻️ **默认可恢复** —— 清理 = 移入 `~/.thin/quarantine`，随时 `restore`；只有显式 `purge` 才永久删除
- 🛡️ **安全门** —— 受保护路径 / 裸顶层目录 / 卷隔离 / 需 sudo 项自动跳过
- 📌 **保护在研项目** —— `thin protect add .` 让正在开发的 `target/`、`node_modules/` 不再被清理
- 🤖 **Agent 友好** —— 关键命令都有 `--json`，`discover → rules add → clean` 可非交互跑通

## 快速开始

### 安装

```bash
# 从 Git 安装（需要 Rust 1.88+，edition 2024 + let-chains）
# 注意：包名是 thin-cli，安装出的二进制名是 thin；默认装到 ~/.cargo/bin/thin
cargo install --git https://github.com/lazyfury/thin.git thin-cli

# 升级到最新（覆盖已安装的同名版本）
cargo install --git https://github.com/lazyfury/thin.git thin-cli --force

# 或本地构建
git clone https://github.com/lazyfury/thin.git && cd thin
cargo build --release        # 产物 target/release/thin
```

> **可选：macOS 底层能力（Swift/ThinKit）** —— `thin probe` 的「可回收空间(purgeable) /
> 含 purgeable 可用」由 `swift/ThinKit` 经 FFI 提供（需要 Xcode 或 Command Line Tools）。
> 没有 Swift 工具链时会自动降级为纯 Rust `statfs`（该行显示「Swift 后端未启用」），
> 构建照常成功。详见 [`docs/swift-ffi.md`](docs/swift-ffi.md)。

### 30 秒上手

```bash
thin probe                     # 磁盘概览：容量 / 卷 / 快照 / 外接盘
thin scan                      # 扫描已知可清理项，按体积排序
thin clean                     # 默认 dry-run 预览，不执行任何操作
thin clean --apply             # 确认后移入系统废纸篓（可恢复）
thin clean --quarantine --apply # 改用 thin 隔离区（quarantine restore 可恢复）
thin quarantine list           # 查看隔离会话（仅 --quarantine 模式）
thin quarantine restore <会话>  # 撤回
thin                            # 无参数：进入交互式 TUI
```

> 在项目根执行 `thin protect add .`，即可保护该项目（含 `target/`、`node_modules/` 等）不被清理，
> 避免每次 `cargo install` 全量重编。

## 演示

```text
$ thin probe
磁盘容量
  APFS 容器    容量 228.3 GB  已用 57.9 GB  可用 170.4 GB  (25%)
  可回收空间(purgeable) 4.6 GB  ·  含 purgeable 可用 175.1 GB
  完全磁盘访问权限  已授权

$ thin scan
        大小  风险       类别      名称                        路径
    4.4 GB  需确认    应用缓存   Ollama 模型                 ~/.ollama/models
    1.8 GB  需确认    开发缓存   Android SDK               ~/Library/Android/sdk
    1.6 GB  需确认    开发缓存   Rust 工具链 (toolchains)     ~/.rustup/toolchains
  947.4 MB  需确认    日志      符号缓存 (uuidtext)          /private/var/db/uuidtext
  688.3 MB  安全     开发缓存   Rust 编译产物 (target/)      ~/proj/target
  ...

可回收总计: 8.7 GB （安全 688.3 MB / 需确认 8.0 GB / 需手动 2.2 GB）

$ thin clean
• Chrome 缓存    110.1 MB   →  移入系统废纸篓（可恢复）
• Homebrew 缓存   22.3 MB   →  移入系统废纸篓（可恢复）

共 2 项，预计可释放 132.4 MB
（dry-run，未执行任何操作。加 --apply 移入系统废纸篓）
```

> TUI：`thin` 进入，四个标签 —— **清理 / 应用 / 历史 / 浏览**（逐步下钻 + 用途标注，键 4）。
> 加载与扫描带进度：确定进度用进度条，不确定用 spinner。
> 底部状态栏区分反馈：信息/提示数秒后自动消失；**错误会保留到按 Esc 关闭**；
> Esc 在无提示时退出；浏览页用 `Tab`/`Shift-Tab`/数字键切换标签（`l`/`h` 留给进入/上级）。
> `q` 在任意标签页都退出整个 TUI；`Esc` 先关闭提示/模态，无提示时退出。
> 清理页默认隐藏「需 sudo / 受系统保护」与「thin protect 保护」的项：`m` 显示前者、`b` 显示后者；
> 没有可清理项时显示「✨ 您的电脑很干净！」。

## 命令速查

<details open>
<summary><b>扫描与归因</b></summary>

```bash
thin probe                        # 磁盘概览：容量、卷、快照、外接盘
thin scan                         # 已知可清理项（默认 safe + confirm）
thin scan --all                   # 额外显示不可再生项（虚拟机等）
thin scan --min 100MB             # 最小体积过滤
thin scan --json                  # 机器可读输出
thin scan --detail rust-target    # 查看某规则详细解释
thin scan --preset dev            # 只看开发缓存
thin scan --preset dev --root .   # 只看当前项目下的开发产物（target/、node_modules/ …）
thin scan --tree                  # 按文件夹合并成树形展示（只读）
thin top ~/Library --limit 20     # 某目录下最大的子项（类似 du -sh | sort -rh）
thin discover --min 1G            # 找出「未被规则覆盖」的大目录
thin discover --json              # 机器可读

# 浏览与学习：给目录标注用途（这是什么 / 能不能删）
thin ls /                         # 根目录：每个 Unix 风格目录的用途
thin ls ~/Library -l              # -l 显示说明与参考(如 man hier)
thin ls /private --depth 2        # 递归两层
thin ls /System/Volumes --json    # 机器可读
# 交互式浏览在主 TUI 的「浏览」标签页（键 5）：↑↓ 移动 · Enter 进入 · / 过滤 · s 排序 · t 占用图 · c 清理
# 主 TUI 的「清理」标签页（键 1）默认按文件夹合并的树形，按 t 切回平铺列表

thin                              # 无参数：进入交互式 TUI
thin tui                          # 显式进入 TUI（含「浏览」标签页，键 5）
thin tui --min 100MB
```

</details>

<details open>
<summary><b>开发者 / Agent 定向清理</b></summary>

两个正交的维度：**清什么**（`--preset`）× **在哪清**（`--root`）。

```bash
# 开发者：只清当前项目里的 target/、node_modules/ 等（不动全局缓存）
thin scan  --preset dev --root .   # 先看
thin clean --preset dev --root .   # dry-run
thin clean --preset dev --root . --apply

# Agent：两阶段契约 —— 先生成计划，审阅后再原样执行
thin plan --preset dev --root . > plan.json
thin apply --plan plan.json --yes
```

`plan` 输出的 `approved` 是完整清理项；`apply` 会**再次过同一安全门**，所以过期的计划也不会误删。

把完整工作流提示词交给外部 agent：`thin agents`（原样打印 [`docs/agent-prompt.md`](./docs/agent-prompt.md)，编译期内嵌、离线可用）。

</details>

```bash
thin clean                        # dry-run 预览（默认「安全」项）
thin clean --apply                # 移入系统废纸篓（二次确认；Finder 可恢复）
thin clean --apply --yes          # 跳过确认
thin clean --apply --all          # 连「需确认」项一起处理
thin clean --apply --id rust-target --id chrome-optguide-model
thin clean --preset dev --root .  # 定向清理：只清当前项目的开发产物
thin clean --tree                 # dry-run 预览按文件夹合并成树形
thin clean --quarantine --apply   # 改用 thin 隔离区（而非默认的系统废纸篓）
thin clean --json                 # 机器可读计划：approved / skipped / approvedBytes / protectedBytes
thin clean --apply --json --yes   # 执行并输出账本 JSON

thin quarantine list              # 查看所有会话
thin quarantine restore           # 恢复最近一次
thin quarantine restore --all     # 恢复全部
thin quarantine purge <会话> --dry-run   # 预览将永久删除的会话
thin quarantine purge <会话>       # 二次确认后永久删除
thin quarantine purge --older-than 7d
thin quarantine purge --all --yes  # 跳过确认（谨慎）
```

</details>

<details open>
<summary><b>保护名单</b></summary>

```bash
thin protect add .                # 保护当前项目（含所有子目录）
thin protect list                 # 查看
thin protect remove .             # 解除
```

</details>

<details>
<summary><b>大文件 / 重复文件 / App 管理</b></summary>

```bash
thin large ~/Documents --min 100MB --limit 30
thin large ~/Documents --min 100MB --json      # 机器可读（含 protected 标记）
thin dupes ~/Downloads --min 10MB              # 只读报告（受保护副本标 [已保护]，不计入可回收）
thin dupes ~/Downloads --min 10MB --json       # 机器可读报告
thin dupes ~/Downloads --min 10MB --apply      # 每组保留首个，其余移入隔离区（预览=执行）
thin apps                                      # 列出全部 App（含关联残留），带体积分级与「● 运行中」标记
thin apps --min 500MB                          # 只看大件
thin uninstall <名称>                           # 预览卸载计划
thin uninstall <名称> --deep                    # 预览时用 Spotlight 深扫补充带后缀/嵌套的关联残留
thin uninstall <名称> --apply                   # 卸载并移入隔离区
thin uninstall <名称> --kill --apply            # 先退出正在运行的 App（优雅退出 → 强制结束）再卸载
thin orphans                                   # 已卸载 App 的孤立残留（App 本体已不存在）
thin orphans --json                            # 机器可读
thin orphans --apply                           # 清理（移入废纸篓，可恢复）
```

</details>

<details>
<summary><b>规则 / 预设 / 历史 / 定时任务</b></summary>

```bash
thin rules                          # 列出所有规则（带【内置/用户】来源）
thin rules path                     # 用户规则文件路径
thin rules add --path "~/..." --risk safe --regenerable
thin rules remove <id>
thin rules export --new             # 导出未并入内置的用户规则

thin preset list                    # 内置默认 + 用户预设
thin preset add nightly             # 新建预设（默认只选 cache 类）
thin clean --preset nightly         # 按预设筛选（dry-run）

thin history --limit 20             # 清理历史
thin history --json                 # 机器可读
thin history --reconcile            # 用隔离区账本回填遗漏的历史

thin schedule install --preset nightly --weekly --hour 3 --dry-run   # 预览 plist
thin schedule install --preset nightly --weekly --hour 3             # 安装 launchd 任务
thin schedule status
thin schedule run --preset nightly --dry-run   # 预览将清理的项（不 purge/不隔离）
thin schedule run --preset nightly             # 立即按预设跑一次
thin schedule uninstall
```

</details>

## 安全模型

| 机制 | 说明 |
|---|---|
| 默认可恢复 | 清理 = 移动到 `~/.thin/quarantine/<会话>/`（同卷内为改名，**不立即释放空间**；`quarantine purge` 后才真正释放） |
| `deny delete` ACL | `~/Library/Caches`、`~/Library/Logs` 等目录无法整体删除，thin 退化为**只清内容**（等价 `rm -rf <dir>/*`），目录本身保留；废纸篓与隔离区两种模式一致 |
| Journal | 每次清理写入账本（原始路径、隔离路径、大小、规则），支持精确回滚 |
| 受保护白名单 | `/`、`/System`、`/usr`、`/etc`、`/private/var/vm`、`/private/var/db`、`/Library/Apple`、Keychains、iCloud、CloudStorage —— 永不触碰 |
| 个人目录顶层 | `~/Documents`、`~/Desktop`、`~/Library` 等**本身**不可整体清理，但其内部具体缓存/项目产物仍可清 |
| 裸顶层目录 | `/Applications`、`/Library`、`/opt`、`/Volumes`、`/Users`、`/private/var` 等目录**本身**永不被删 |
| 保护名单 | `thin protect` 标记的路径及其子目录永不清理（`scan` 标「已保护」、不计入可回收，`clean` 跳过） |
| 路径校验 | 拒绝空路径、控制字符、`..` 组件；符号链接先解析再判定 |
| 预演=执行 | `clean` / `dupes` / `uninstall` 的 dry-run 与 `--apply` 共用同一安全门，跳过项不计入可释放 |
| 卷隔离 | 目标必须与隔离区（`~/.thin`）同卷；外接盘/其他挂载被拒绝，避免跨卷复制 |
| App 保护 | 运行中的 App 拒绝卸载（探测超时视为运行中），加 `--kill` 可先退出其进程再卸载；系统关键 App 禁止卸载 |
| 超时 | `tmutil`/`plutil`/`pgrep`/`mount`/`date` 均带超时，不会挂死 |
| 需 sudo 项 | 自动跳过，提示手动处理，**不计入「可回收」** |
| 风险分级 | 默认只处理「安全」项；`--all` 含「需确认」；「不可再生」需显式 `--id` |

## 准确性说明

| 场景 | 处理 |
|---|---|
| 稀疏文件（如 `Docker.raw` 逻辑 228GB / 实占 74MB） | 按实际分配块统计 ✅ |
| 硬链接（如 Rust `.a` 文件） | 按 (dev, inode) 去重 ✅ |
| 跨挂载点 / 外接盘 | 不跨越文件系统边界，外接卷单独标注 ✅ |
| APFS 多卷 / firmlink | APFS 各卷**共享同一个 `st_dev`**，按**挂载点**(`getmntinfo`)而非设备号识别卷边界，避免 `/System` 重复计入数据卷 ✅ |
| 父子路径重复（`~/Library/Caches` 与其子目录） | 汇总时按风险分层去重，safe 子项不被 confirm 父项吞掉 ✅ |
| APFS 克隆共享块 | 已知局限，暂无法在此层面拆分 ⚠️ |

## 定时清理

纯 CLI 不需要常驻 App，用 macOS 原生 launchd（LaunchAgent），以当前用户身份运行。

<details>
<summary><b>展开：设计要点</b></summary>

- **只能执行用户自定义预设**：`thin schedule install` 要求 `--preset` 是 `thin preset add` 创建的；
  内置 `default` 预设仅供手动 `thin clean --preset default`。
- **内置默认预设只处理 cache 类**：`system-cache / app-cache / dev-cache`，且仅 `safe` + 可再生 + 非 sudo。
- **清理 + 回收两步**：隔离同卷改名**不释放空间**，所以定时任务先 `quarantine purge` 早于预设
  `purgeAfterDays` 的旧会话，再隔离本次新项。
- **历史记录**：每次实际清理写入 `~/.thin/history.jsonl`（触发方式、预设、项数、释放/跳过字节、purged 字节）。
- **Full Disk Access**：定时任务要读受保护目录，需在「系统设置 → 隐私与安全 → 完全磁盘访问权限」
  里把 thin 二进制加入；Homebrew 升级会替换二进制导致授权失效，建议装到稳定路径。

定时任务由 launchd 调起 `thin schedule run --preset <id>`，日志在 `~/.thin/schedule.log` / `schedule.err`。

</details>

## 规则系统

所有可清理项声明在 [`rules/default.json`](./crates/thin-core/rules/default.json)，编译期嵌入。
内置 **70 条**，覆盖常见 `~/Library` 缓存、大量 `~/.xx` / `~/.cache/*` 开发缓存
（pip/uv/yarn/pnpm/bun、Go、Maven、NuGet、CocoaPods/SwiftPM、Playwright、HuggingFace 等）
以及 Xcode 大件（DeviceSupport / 模拟器缓存）。

<details>
<summary><b>展开：匹配方式与用户规则</b></summary>

两种匹配方式：

```jsonc
{ "matcher": { "kind": "path", "paths": ["~/Library/Caches"] } }

{ "matcher": {
    "kind": "findDir",
    "roots": ["~/Documents"],
    "dirName": "target",
    "requireSibling": "Cargo.toml",  // 必须是 cargo 项目产物
    "maxDepth": 6
} }
```

用 `THIN_RULES=/path/to/rules.json thin scan` 可覆盖内置规则。

**用户规则（可热更新）** —— 与内置规则按 id 合并、用户优先。

- **写入点只有一个**：`~/.thin/rules.d/<id>.json`（一规则一文件，`thin rules add` 写这里）。
  避免读改写整个文件的竞态，便于 agent 逐步增删。
- `~/.thin/rules.json` 为旧格式，仍**兼容读取**（可手写批量规则），但 `thin rules add` 不再写它；
  若其中存在与 `rules.d` 同 id 的条目，会被自动清除以避免两份不一致。
- 加载顺序：内置 → `rules.json`（旧）→ `rules.d/*.json`（按文件名），同名 id 后者覆盖。

```bash
thin rules path                             # 显示两个位置
thin rules add --path "~/Library/.../SomeApp/Cache" --risk safe --regenerable
cat rule.json | thin rules add --json -     # 或用完整 JSON
thin scan --detail custom-someapp           # 验证命中
thin rules export --new                     # 导出未并入内置的规则，合进 default.json 后重新构建
```

**脚本型 matcher（动态路径）** —— 静态 `path` / `findDir` 表达不了的场景（如
「`releases/*` 里清掉除 `current-version` 之外的所有旧版本」）可用 `kind: "script"`：
脚本只负责**枚举并打印候选路径**，且必须经**风险审查**：

- `roots` 必填：输出路径必须落在其下（containment）；
- `review.hash` = 脚本内容的 blake3（`--approve-script` 自动写入）；脚本变更即失效，需重审；
- 脚本含 `rm`/`mv`/`sudo`/`curl`/写盘重定向等片段会被拒绝；运行时带超时，候选仍过清理安全门。

```bash
thin rules add --json rule.json --approve-script   # 审查并钉住脚本哈希
```

</details>

## App 残留调查

`thin apps` / `thin uninstall` 的残留探测覆盖常见写入位置：

<details>
<summary><b>展开：探测范围</b></summary>

- **App Bundle 内部**（参考 Pearcleaner）：`CFBundleExecutable`、`Contents/MacOS/*` 可执行名、
  嵌套 helper / 登录项 / XPC bundle 名（如 `Foo Helper.app`）——这些名字常被用作残留目录名；
  通用词（Helper、Electron 等）与过短名字会被过滤，避免误配；
- **用户 `~/Library/`**：Application Support（含 CrashReporter）、Caches、Logs、Containers、
  Group Containers、Application Scripts、WebKit、HTTPStorages、Preferences（含 ByHost）、
  LaunchAgents、Saved Application State、Cookies；
- **插件 / 扩展**：Internet Plug-Ins、PreferencePanes、QuickLook、Screen Savers、ColorPickers、
  Dictionaries、Automator、Spotlight、Input Methods、Widgets、Services、Safari Extensions、
  Audio Plug-Ins（AU/VST/VST3/CLAP）等，同时匹配 `<name>.<ext>` 形式；
- **主目录点目录 / XDG**：`~/.<name>`、`~/.config/<name>`、`~/.cache/<name>`、`~/.local/share|state/<name>`；
- **共享与包管理**：`/Users/Shared`、`/usr/local/*`、`/opt/homebrew/*`（后两者标记 **需 sudo**，交安全门手动处理）；
- **系统级 `/Library/`**：Application Support、Caches、Logs、Preferences、LaunchAgents、
  LaunchDaemons、PrivilegedHelperTools、Application Scripts 及各类插件目录（标记 **需 sudo**，安全门跳过并提示手动）；
- **代码签名 entitlements**（Swift 后端）：沙盒 `Containers/<bundle-id>`、entitlements 声明的
  Group Containers、iCloud 容器（`Mobile Documents`）、team id 前缀的 Group Containers，
  并通过容器元数据（`MCMMetadataIdentifier`）把 **UUID 命名**的沙盒容器归回所属 App；
- **每用户 Darwin 临时/缓存**：`/private/var/folders/<随机>/{0,T,C}`（即 `$TMPDIR` 与用户缓存），
  用 `confstr` 解析随机前缀，按 bundle id / 别名匹配其中的 App 目录（`C/`、`T/` 里常见）；
- **声明式条件表**：内置 `rules/app-leftovers.json`（编译期嵌入），用户可在
  `~/.thin/app-leftovers.d/*.json` 按 `bundleId` 覆盖。字段：`tokens`（补充目录名）、
  `aliases`（App 别名/旧名，归一化后参与残留匹配，也可作为 `thin uninstall <别名>` 的查询词）、
  `forcePaths`（精确补路径）、`require`/`exclude`（收敛泛匹配，相对 home 归一化后子串匹配）。
  用于消除歧义：Chrome 只取 `Google/Chrome` 而不整包删除共享的 `Google` 厂商目录，
  VS Code 稳定版不碰 `Code - Insiders`，Firefox 不碰 Thunderbird 等；
- **Spotlight 深扫**（`thin uninstall --deep`）：用 `mdfind` 补充按固定 token 枚举不到的
  带后缀/嵌套名字（如 `Application Scripts/com.foo.bar.FinderOpen`）。仅查询用户主目录，
  结果须落在受信任根（`~/Library`、`~/.config`、`~/.cache`、`~/.local`）之下、
  **文件名**命中强 token（App 名/bundle id/条件表 token），并过滤受保护路径与废纸篓；
  父目录优先、子项去重。候选仍走同一安全门，不改变删除语义。
- **孤立残留**（`thin orphans` / TUI 应用页 `o`）：App 本体已卸载、残留还在时，按 bundle id
  聚合 `~/Library/{Containers,Application Scripts,HTTPStorages,WebKit,Preferences,Saved Application
  State,Caches,Logs,Application Support}` 与 Darwin `C/` 下「像 bundle id」的条目；排除系统 id
  （`com.apple.*`）、group id（`TEAMID.group.*`）与已安装 App 家族（含嵌套 `.app`/`.xpc`/`.appex`
  的 bundle id）。**只覆盖高置信来源**，App 名/厂商目录等中置信来源从略，仍走同一安全门。
  注意：`~/Library/Containers` 等 App 沙盒容器受 TCC 保护，**终端需授予「完全磁盘访问权限」**
  才能移入废纸篓/隔离区；缺权限时 thin 会明确提示（而非静默「失败」）。

目录名只用强证据：完整 bundle id、bundle 末段（非通用词）、显示名/归一化名、Bundle 内可执行/
helper 名，以及精确匹配的提示表（如 VS Code→`Code`、Chrome→`Google`、Docker→`Docker`）。
**不用** bundle 中间段做泛匹配，避免误删同厂商其它 App 的数据；大小写不敏感用真实路径去重。
`pkgutil` 命中的安装包 id 会一并列出，便于 `sudo pkgutil --forget`。

</details>

## 项目结构

```
crates/
  thin-core/     核心库：磁盘探测、规则、扫描、核算、安全清理、查找（无 UI 依赖）
    src/{lib,model,fsutil,probe,rules,scan,clean,protect,discover,finder,
         apps,preset,history,schedule,status,progress,proc,fmt}.rs
    rules/default.json
  thin-cli/      前端：CLI (clap) + TUI (ratatui)
    src/{main,report,top,tui,treemap}.rs
```

分层目的：核心逻辑可被 CLI、TUI、未来的 SwiftUI GUI 或测试复用。

## 路线图

- **M0** CLI 只读扫描 + 诚实核算 + dry-run ✅
- **M1** 安全清理：隔离区 + Journal + 恢复/永久删除 + TUI 交互 ✅
- **M2** 大文件查找 / 重复文件检测 / App 卸载（均复用隔离区）✅
- **M3** 规则热更新（用户规则文件）+ 异常大目录归因 + agent 规则写入入口 ✅
- **M4** TUI 多标签页（清理 / 应用 / 历史 / 浏览，懒加载 + 进度条/spinner）✅
- **M5** 清理预设 + 历史记录 + 定时任务（launchd）+ 保护名单 + 统一安全门口径 + agent JSON ✅
- **M6（当前）** 定向清理：`--preset dev` × `--root .`，以及 agent 两阶段 `plan` / `apply` ✅
- **M7** SwiftUI 前端

## 开发

```bash
cargo build --release
cargo test
```

## 许可

MIT © [lazyfury](https://github.com/lazyfury)
