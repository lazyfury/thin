# AGENTS.md — thin 的 agent 工作流

本文件面向自动化 agent：如何**探索磁盘**、把发现**写成规则**、并**非交互地**完成清理。

核心原则：探索 → 归因 → 写规则 → 验证。所有写操作都进入**隔离区**，可恢复。

---

## 0. 可用命令一览

```bash
thin probe                     # 磁盘概览（容量/卷/快照/外接盘）
thin scan [--all] [--json]     # 扫描已知可清理项
thin discover [ROOT] [--json]  # 找出「未被规则覆盖」的大目录
thin top [ROOT]                # 某目录下最大子项
thin rules [list|path]         # 规则列表 / 用户规则文件路径
thin rules add ...             # 新增规则（agent 入口）
thin rules remove <id>         # 删除用户规则
thin clean [--apply]           # 清理（默认 dry-run）
thin quarantine list|restore|purge
thin large [ROOT] --min --limit
thin dupes [ROOT] --min [--apply]
thin apps --min ; thin uninstall <名称> [--apply]
```

所有命令都可用 `THIN_HOME` 指向隔离的数据目录（便于在沙箱/测试中运行），
用 `THIN_RULES=/path/rules.json` 覆盖内置规则。

---

## 1. 探索循环

```bash
# ① 看全局
thin probe

# ② 看已知可清理项
thin scan --json

# ③ 看「哪些大目录还没有规则」← 关键
thin discover --min 1G --json

# ④ 对「部分覆盖/未归类」的目录继续下钻
thin discover ~/Library --min 500MB
thin top ~/Library/Application\ Support

# ⑤ 直接看大文件
thin large ~/Library --min 500MB --limit 30
```

`discover` 会把每个子项标注为：
- `已归类` —— 已被某规则完整覆盖
- `部分(<规则id>)` —— 目录内有规则，但本身还有未归类数据
- `未归类` —— 完全没有规则

`discover` 文本输出末尾会直接给出下一步可执行的 `thin rule add ...` 命令，并附**覆盖率自检**
（直接子项合计 / 已归类 / 部分覆盖 / 未归类）。`--json` 输出对象：
`{ root, total, covered, partial, uncovered, coverageRatio, findings: [...] }`。

`scan --json` 额外返回 `accountedBytes`（规则命中总量，含嵌套去重）与 `volume`（主卷 total/used/avail），
用于说明「已知可清理项」占已用的比例（**不等于「未归类」**，后者用 `discover`）。

---

## 2. 把探索结果写成规则

### 方式 A：命令行参数（推荐，自动补默认值）

```bash
thin rules add \
  --path "~/Library/Application Support/SomeApp/Cache" \
  --id custom-someapp \
  --name "SomeApp 缓存" \
  --category app-cache \
  --risk safe \
  --regenerable \
  --reclaim "rm -rf ~/Library/Application Support/SomeApp/Cache/*" \
  --what "SomeApp 的本地缓存" \
  --cost "首次启动略慢" \
  --recover "由 App 自动重建"
```

省略 `--id` 时按路径自动生成（如 `custom-cache`）；省略 `--name` 取路径末段。

### 方式 B：完整 JSON（管道给 agent 最方便）

```bash
echo '{
  "id": "custom-someapp",
  "name": "SomeApp 缓存",
  "category": "app-cache",
  "risk": "safe",
  "regenerable": true,
  "sudo": false,
  "matcher": { "kind": "path", "paths": ["~/Library/Application Support/SomeApp/Cache"] },
  "reclaim": "rm -rf ...",
  "explain": { "what": "...", "cost": "...", "recover": "..." }
}' | thin rules add --json -
```

`--json` 也可直接接 JSON 字符串或文件路径。

### 规则模型

```jsonc
{
  "id": "唯一标识",
  "name": "显示名",
  "category": "system-cache|app-cache|dev-cache|vm|log|trash|leftover|other",
  "risk": "safe|confirm|destructive",
  "regenerable": true,          // 删除后能否自动重建
  "sudo": false,                // 是否需 root（M1 起自动跳过）
  "matcher": { "kind": "path", "paths": ["~/..."] }
  // 或 { "kind": "findDir", "roots": ["~/Documents"], "dirName": "target",
  //      "requireSibling": "Cargo.toml", "maxDepth": 6 }
  ,
  "reclaim": "给人类看的清理方式/命令",
  "explain": { "what": "...", "cost": "...", "recover": "..." }
}
```

写入位置：`thin rules path`（默认 `~/.thin/rules.json`），与内置规则**按 id 合并**（用户规则优先）。

---

## 3. 验证与清理

```bash
# 验证规则确实命中
thin scan --detail custom-someapp

# 清理（默认 dry-run；--apply 才执行，进隔离区）
thin clean --apply --id custom-someapp --yes

# 兜底：列出 / 恢复 / 永久删除
thin quarantine list
thin quarantine restore <session>
thin quarantine purge --older-than 7d
```

---

## 4. Agent 必须遵守的安全约束

1. **先只读**：`scan`/`discover`/`large`/`dupes` 都是只读，先用它们摸清情况。
2. **永不直接 `rm`**：清理一律走 `thin clean --apply` / `quarantine`，默认进隔离区。
3. **风险分级**：
   - `safe`：可自动处理（可再生成）
   - `confirm`：需人工确认（`--all` 才纳入）
   - `destructive`：只能 `--id` 显式指定
4. **受保护路径**（永不可动）：`/`、`/System`、`/private/var/vm`、`/Library/Apple`、Keychains、iCloud、`~/Library/CloudStorage`、挂载点、整个主目录。
5. **卷隔离**：目标必须与隔离区（`~/.thin`）同卷；外接盘/其他挂载会被安全门拒绝，避免跨卷复制。
6. **运行中的 App**：`thin uninstall --apply` 会先检测，App 正在运行则拒绝卸载。
5. **写规则前先确认路径真实存在且可清理**；`regenerable=false` 的项风险应设为 `confirm` 或 `destructive`。
6. 不确定时，宁可 `risk=confirm` 且 `--dry-run` 先看结果。

---

## 5. 最小示例：发现并收纳一个未归类缓存

```bash
export THIN_HOME=$PWD/.thin-sandbox

thin discover ~/Library --min 200MB --json          # 发现未归类目录
thin rules add --path "~/Library/Application Support/RetroArch" \
  --risk safe --category app-cache --regenerable    # 写成规则
thin scan --detail custom-retroarch                 # 验证命中
thin clean --apply --id custom-retroarch --yes      # 移入隔离区（可恢复）
thin quarantine restore <session>                   # 需要时还原
```
