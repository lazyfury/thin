# spacekit

macOS 系统空间扫描与安全清理 CLI + TUI（M1 · 隔离区可恢复）。

> 设计文档见 [DESIGN.md](./DESIGN.md)。清理默认**不真正删除**，而是移入**隔离区**（`~/.spacekit/quarantine`），
> 写入 Journal，可随时 `restore`；只有显式 `purge` 才永久删除。

## 为什么做这个

macOS 的「系统数据 / 系统缓存」是个兜底分类，会把虚拟机、系统卷、缓存、Homebrew 等统统算在一起，
动辄显示几十 GB，却说不清是什么。spacekit 的做法相反：

- **还原真实路径**，每一项都注明「这是什么 / 删了会怎样 / 能否恢复」
- **诚实核算**：按实际分配块统计（稀疏文件不虚高）、按 inode 去重（硬链接不重复）、排除嵌套重复路径
- **风险分级**：`安全` / `需确认` / `不可再生`，默认只把前两类计入「可回收」
- **可恢复**：清理 = 移入隔离区，随时可撤回；永久删除需显式操作

## 工作区结构

```
crates/
  spacekit-core/     核心库：磁盘探测、规则、扫描、核算、安全清理（无 UI 依赖）
    src/{lib,model,fsutil,probe,rules,scan,clean,fmt}.rs
    rules/default.json
  spacekit-cli/      前端：CLI (clap) + TUI (ratatui)
    src/{main,report,top,tui}.rs
```

分层目的：核心逻辑可被 CLI、TUI、未来的 SwiftUI GUI 或测试复用。

## 构建

```bash
cargo build --release
# 产物: target/release/spacekit
```

## 命令

```bash
# 磁盘概览：容量、卷、快照、外接盘
spacekit probe

# 扫描已知可清理项并估算可回收空间
spacekit scan                 # 默认显示 safe + confirm
spacekit scan --all           # 额外显示不可再生项（虚拟机等）
spacekit scan --min 100MB     # 最小体积过滤
spacekit scan --json          # 机器可读输出
spacekit scan --detail rust-target   # 查看某规则详细解释

# 列出某目录下最大的子项（类似 du -sh PATH/* | sort -rh）
spacekit top ~/Library --limit 20

# 交互式 TUI：浏览、勾选、按 c 移入隔离区
spacekit tui
spacekit tui --min 100MB

# 列出内置规则目录
spacekit rules

# 清理：默认只预览；--apply 才真正移入隔离区
spacekit clean                          # dry-run 预览（默认「安全」项）
spacekit clean --apply                  # 移入隔离区（会二次确认）
spacekit clean --apply --yes            # 跳过确认
spacekit clean --apply --all            # 连「需确认」项一起处理
spacekit clean --apply --id rust-target --id chrome-optguide-model

# 隔离区管理
spacekit quarantine list                # 查看所有会话
spacekit quarantine restore             # 恢复最近一次
spacekit quarantine restore --all       # 恢复全部
spacekit quarantine purge <session>     # 永久删除
spacekit quarantine purge --older-than 7d
```

## 安全模型

| 机制 | 说明 |
|---|---|
| 默认可恢复 | 清理 = 移动到 `~/.spacekit/quarantine/<会话>/`，原位置立刻释放空间 |
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

用 `SPACEKIT_RULES=/path/to/rules.json spacekit scan` 可覆盖内置规则。

## 路线图

- **M0** CLI 只读扫描 + 诚实核算 + dry-run ✅
- **M1（当前）** 安全清理：隔离区 + Journal + 恢复/永久删除 + TUI 交互 ✅
- M2 大文件 / 重复文件 / App 卸载
- M3 规则热更新、异常大目录归因

## 测试

```bash
cargo test
```
