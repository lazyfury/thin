# thin

macOS 系统空间扫描与安全清理 CLI + TUI（M3 · 规则可扩展）。

> 设计文档见 [DESIGN.md](./DESIGN.md)。清理默认**不真正删除**，而是移入**隔离区**（`~/.thin/quarantine`），
> 写入 Journal，可随时 `restore`；只有显式 `purge` 才永久删除。
> Agent 工作流见 [AGENTS.md](./AGENTS.md)。

## 为什么做这个

macOS 的「系统数据 / 系统缓存」是个兜底分类，会把虚拟机、系统卷、缓存、Homebrew 等统统算在一起，
动辄显示几十 GB，却说不清是什么。thin 的做法相反：

- **还原真实路径**，每一项都注明「这是什么 / 删了会怎样 / 能否恢复」
- **诚实核算**：按实际分配块统计（稀疏文件不虚高）、按 inode 去重（硬链接不重复）、排除嵌套重复路径
- **风险分级**：`安全` / `需确认` / `不可再生`，默认只把前两类计入「可回收」
- **可恢复**：清理 = 移入隔离区，随时可撤回；永久删除需显式操作

## 工作区结构

```
crates/
  thin-core/     核心库：磁盘探测、规则、扫描、核算、安全清理、查找（无 UI 依赖）
    src/{lib,model,fsutil,probe,rules,scan,clean,finder,apps,discover,fmt}.rs
    rules/default.json
  thin-cli/      前端：CLI (clap) + TUI (ratatui)
    src/{main,report,top,tui,treemap}.rs
```

分层目的：核心逻辑可被 CLI、TUI、未来的 SwiftUI GUI 或测试复用。

## 构建

```bash
cargo build --release
# 产物: target/release/thin
```

## 命令

```bash
# 磁盘概览：容量、卷、快照、外接盘
thin probe

# 扫描已知可清理项并估算可回收空间
thin scan                 # 默认显示 safe + confirm
thin scan --all           # 额外显示不可再生项（虚拟机等）
thin scan --min 100MB     # 最小体积过滤
thin scan --json          # 机器可读输出
thin scan --detail rust-target   # 查看某规则详细解释

# 列出某目录下最大的子项（类似 du -sh PATH/* | sort -rh）
thin top ~/Library --limit 20

# 交互式 TUI（首页=清理；1-5 / Tab 切换标签）
thin tui
thin tui --min 100MB
# 标签: 清理 / 概览(硬盘占用图) / 大文件 / 重复 / 应用

# 规则 / 归因 / agent 入口
thin rules                          # 列出所有规则
thin rules path                     # 用户规则文件路径
thin rules add --path "~/..." --risk safe --regenerable
thin rules remove <id>
thin discover --min 1G              # 找出未被规则覆盖的大目录
thin discover --json                # 机器可读

# 清理：默认只预览；--apply 才真正移入隔离区
thin clean                          # dry-run 预览（默认「安全」项）
thin clean --apply                  # 移入隔离区（会二次确认）
thin clean --apply --yes            # 跳过确认
thin clean --apply --all            # 连「需确认」项一起处理
thin clean --apply --id rust-target --id chrome-optguide-model

# 隔离区管理
thin quarantine list                # 查看所有会话
thin quarantine restore             # 恢复最近一次
thin quarantine restore --all       # 恢复全部
thin quarantine purge <session>     # 永久删除
thin quarantine purge --older-than 7d

# 大文件 / 重复文件 / App 管理
thin large ~/Documents --min 100MB --limit 30
thin dupes ~/Downloads --min 10MB              # 只读报告
thin dupes ~/Downloads --min 10MB --apply      # 每组保留首个，其余移入隔离区
thin apps --min 500MB                          # 列出 App（含关联残留）
thin uninstall <名称>                           # 预览卸载计划
thin uninstall <名称> --apply                   # 卸载并移入隔离区
```

## 安全模型

| 机制 | 说明 |
|---|---|
| 默认可恢复 | 清理 = 移动到 `~/.thin/quarantine/<会话>/`，原位置立刻释放空间 |
| Journal | 每次清理写入账本（原始路径、隔离路径、大小、规则），支持精确回滚 |
| 受保护白名单 | `/`、`/System`、`/private/var/vm`、Keychains、iCloud、挂载点 —— 永不触碰 |
| 需 sudo 项 | M1 自动跳过，提示手动处理 |
| 风险分级 | 默认只处理「安全」项；`--all` 含「需确认」；「不可再生」需显式 `--id` |

## 准确性说明

| 场景 | 处理 |
|---|---|
| 稀疏文件（如 `Docker.raw` 逻辑 228GB / 实占 74MB） | 按实际分配块统计 ✅ |
| 硬链接（如 Rust `.a` 文件） | 按 (dev, inode) 去重 ✅ |
| 跨挂载点 / 外接盘 | 不跨越文件系统边界，外接卷单独标注 ✅ |
| 父子路径重复（`~/Library/Caches` 与其子目录） | 汇总时排除嵌套项 ✅ |
| APFS 克隆共享块 | 已知局限，暂无法在此层面拆分 ⚠️ |

## 规则目录

所有可清理项声明在 [`rules/default.json`](./rules/default.json)，编译期嵌入，可随版本更新。

支持两种匹配方式：

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

### 用户规则（可热更新）

内置规则之外，可把自己的发现写入**用户规则文件**（`thin rules path`，默认 `~/.thin/rules.json`），
与内置规则按 id 合并、用户优先：

```bash
thin rules add --path "~/Library/Application Support/SomeApp/Cache" --risk safe --regenerable
cat rule.json | thin rules add --json -     # 或用完整 JSON
thin rules remove custom-someapp
```

`thin discover` 会标注每个大目录的归因（已归类 / 部分 / 未归类）并直接给出补规则的命令。
完整 agent 流程见 [AGENTS.md](./AGENTS.md)。

## 路线图

- **M0** CLI 只读扫描 + 诚实核算 + dry-run ✅
- **M1** 安全清理：隔离区 + Journal + 恢复/永久删除 + TUI 交互 ✅
- **M2** 大文件查找 / 重复文件检测 / App 卸载（均复用隔离区）✅
- **M3** 规则热更新（用户规则文件）+ 异常大目录归因 + **agent 规则写入入口** ✅
- **M4（当前）** TUI 多标签页（清理/概览占用图/大文件/重复/应用，懒加载）✅
- M5 SwiftUI 前端

## 测试

```bash
cargo test
```
