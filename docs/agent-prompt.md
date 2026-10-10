# thin agent prompt — 面向使用者的 agent 工作流

> 本文件是**面向使用 thin 的自动化 agent** 的提示词：如何**探索磁盘**、把发现**写成规则**、
> 并**非交互地**完成清理。它由 `thin agents` 子命令编译期内嵌并原样打印；
> 面向**开发 thin 本身**的说明见仓库根的 `AGENTS.md`。

核心原则：探索 → 归因 → 写规则 → 验证。所有写操作默认进入**系统废纸篓**（可恢复），
加 `--quarantine` 则进入 thin 隔离区（`quarantine restore` 可恢复）；绝不永久删除。

---

## 0. 可用命令一览

```bash
thin probe                     # 磁盘概览（容量/卷/快照/外接盘）
thin scan [--all] [--json] [--preset P] [--root DIR]  # 扫描已知可清理项（可限定预设/目录）
thin discover [ROOT] [--json]  # 找出「未被规则覆盖」的大目录
thin top [ROOT]                # 某目录下最大子项
thin ls [PATH] [--depth N] [--long] [--all] [--json]  # 浏览并标注用途（学习向，只读）
thin rules [list|path]         # 规则列表 / 用户规则文件路径
thin rules add ...             # 新增规则（agent 入口，统一写入 rules.d/<id>.json）
thin rules remove <id>         # 删除用户规则
thin rules export [--new]      # 导出用户规则，便于并入内置 default.json
thin clean [--apply] [--json] [--preset P] [--root DIR] [--quarantine]  # 清理（默认 dry-run；默认入系统废纸篓，--quarantine 入隔离区）
thin plan [--preset P] [--root DIR]  # 只读生成计划 JSON（agent 两阶段契约的第一阶段）
thin apply --plan <file|-> [--yes]   # 执行已审阅的计划（再过一次安全门）
thin preset list|add|remove    # 清理预设（内置 default / dev；定时任务只执行用户预设）
thin protect add|list|remove   # 保护名单：路径及其子目录永不清理（保护在研项目的 target/ 等）
thin history [--limit N] [--json] [--reconcile]  # 清理历史（--reconcile 回填隔离区遗漏记录）
thin schedule install|status|run|uninstall   # 定时任务（launchd）
thin schedule run --preset <id> --dry-run    # 预览一次预设清理，不 purge/不隔离/不写历史
thin quarantine list|restore|purge
thin large [ROOT] --min --limit [--json]
thin dupes [ROOT] --min [--apply] [--json]   # 受保护副本标 [已保护] 且不计入可回收
thin apps --min ; thin uninstall <名称> [--apply] [--deep] [--sudo]
thin orphans [--json] [--apply] [--sudo]  # 已卸载 App 的孤立残留（无 App 本体，仅剩缓存/容器/偏好）
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

`discover` 文本输出末尾会直接给出下一步可执行的 `thin rules add ...` 命令，并附**覆盖率自检**
（直接子项合计 / 已归类 / 部分覆盖 / 未归类）。受保护/个人目录（`~/Documents`、`~/Library`、
iCloud、`~/.thin` 等）只会被归因，**不会**出现在建议里（会以「已跳过 N 个受保护/个人目录」提示）。
`--json` 输出对象：`{ root, total, covered, partial, uncovered, coverageRatio, findings: [...] }`。
注意：`rules add` 会在写盘前用与清理相同的安全门预检，个人目录顶层会被直接拒绝。

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
  "sudo": false,                // 是否需 root：默认自动跳过；`clean --sudo` 会弹系统授权框，提权后仍入隔离区
  "matcher": { "kind": "path", "paths": ["~/..."] }
  // 或 { "kind": "findDir", "roots": ["~/Documents"], "dirName": "target",
  //      "requireSibling": "Cargo.toml", "maxDepth": 6 }
  // 或 { "kind": "findFile", "roots": ["~/Downloads"],
  //      "extensions": ["dmg", "pkg", "zip", "tar.gz"], "maxDepth": 3, "minSize": 10485760 }
  // 或脚本型（动态产出路径，静态表达不了的场景，如“清 releases/* 但保留 current-version”）：
  // { "kind": "script", "roots": ["~/.pi/agent/install/releases"],
  //   "script": "...只枚举并 printf '%s\\0' 路径...", "timeoutSecs": 10,
  //   "review": { "hash": "<blake3(script)>", "note": "..." } }
  ,
  "reclaim": "给人类看的清理方式/命令",
  "explain": { "what": "...", "cost": "...", "recover": "..." }
}
```

**脚本型 matcher 的风险审查**（三层，缺一不可）：
1. 写规则时：脚本必须带 `review.hash`（脚本内容的 blake3，用 `--approve-script` 自动写入），
   脚本含 `rm`/`mv`/`sudo`/`curl`/重定向写盘等片段会被直接拒绝；`roots` 必填且不得为个人目录顶层/根。
2. 运行时：脚本以 `/bin/sh -c` 执行（超时 1–120s），输出必须落在 `roots` 之下（containment），
   非零退出/超时/超量输出一律丢弃。
3. 清理时：候选仍过 `clean::plan` 统一安全门。
脚本内容变更 → hash 不匹配 → 规则自动失效，需重新审查。

```bash
# 写入脚本型规则（--approve-script 会计算并钉住脚本哈希）
thin rules add --json rule.json --approve-script
```

写入位置（**统一**：`thin rules add` 只写 `rules.d/<id>.json`，一规则一文件，避免读改写竞态）：
- `~/.thin/rules.d/<id>.json` —— 唯一写入点
- `~/.thin/rules.json` —— 旧格式，仅兼容读取；若存在与 `rules.d` 同 id 的条目会被自动清除

加载顺序：内置 → `rules.json`（旧）→ `rules.d/*.json` 按文件名，同名 id 后者覆盖。

提升到内置：`thin rules export --new` 输出未进内置的规则 JSON，合入 `crates/thin-core/rules/default.json`
后重新构建即可；用户规则按 id 覆盖内置，提升后可从 `~/.thin` 删除。

---

### 保护在研项目（可选）

正在开发的项目，其 `target/`、`node_modules/` 等会被规则命中；每次清理后都要重新编译。
在项目根执行一次即可将其加入保护名单，`scan` 会标为「已保护」且不计入可回收，`clean` 直接跳过：

```bash
thin protect add .        # 保护当前项目（含所有子目录）
thin protect list         # 查看
thin protect remove .     # 解除
```

保护是运行期名单（`~/.thin/protected.json`），不会修改或删除规则；显式 `clean --id` 也无法绕过。

---

### 定向清理：开发者与 agent

两个正交维度：**清什么**（`--preset`）× **在哪清**（`--root`）。

```bash
# 开发者：只清当前项目里的 target/、node_modules/ 等，不动全局缓存
thin scan  --preset dev --root .
thin clean --preset dev --root .            # dry-run
thin clean --preset dev --root . --apply

# agent：两阶段契约，把「扫描 + 决策 + 执行」拆开
thin plan --preset dev --root . > plan.json   # 只读，输出 approved/skipped/approvedBytes
thin apply --plan plan.json --yes             # 原样执行；会再次过同一安全门
```

- `--root` 会让 `findDir` 规则改到该目录下查找，所以任意位置的项目都能命中，
  且 path 规则只保留位于该目录下的项。`--root` 必须是真实存在的目录。
- `--preset dev` 内置：只清 `dev-cache`、`safe`+`confirm`、且仅可再生。
- agent 不要用 `--apply --yes` 裸跑；用 `plan` → 审阅 → `apply --plan`，过期计划也不会误删。

## 3. 验证与清理

```bash
# 验证规则确实命中
thin scan --detail custom-someapp

# 清理（默认 dry-run；--apply 才执行，移入系统废纸篓）
thin clean --apply --id custom-someapp --yes
# 需 thin 隔离区（Journal/恢复）时：thin clean --quarantine --apply --id custom-someapp --yes
# 需 root 的系统项（如 /var/log）：加 --sudo 弹系统授权框（仅限内置 sudo 规则，非交互 --json 不支持）
# thin clean --apply --sudo --yes

# 兜底（仅 --quarantine 模式会产生会话）：列出 / 恢复 / 永久删除
thin quarantine list
thin quarantine restore <session>
thin quarantine purge --older-than 7d --dry-run   # 先预览
thin quarantine purge --older-than 7d             # 二次确认后永久删除
```

---

## 4. Agent 必须遵守的安全约束

1. **先只读**：`scan`/`discover`/`large`/`dupes` 都是只读，先用它们摸清情况。
2. **永不直接 `rm`**：清理一律走 `thin clean --apply`（默认进系统废纸篓）/ `--quarantine`（隔离区）。
3. **风险分级**：
   - `safe`：可自动处理（可再生成）
   - `confirm`：需人工确认（`--all` 才纳入）
   - `destructive`：只能 `--id` 显式指定
4. **受保护路径**（永不可动）：`/`、`/System`、`/usr`、`/bin`、`/sbin`、`/etc`、`/private/var/vm`、`/private/var/db`（`diagnostics`/`uuidtext` 除外）、`/Library/Apple`、`/Library/Extensions`、Keychains、iCloud、`~/Library/CloudStorage`、挂载点、整个主目录。
5. **裸顶层目录**（即使子项可清理，也绝不删目录本身）：`/Applications`、`/Library`、`/opt`、`/Volumes`、`/Users`、`/private/var` 等。
6. **卷隔离**：目标必须与隔离区（`~/.thin`）同卷；外接盘/其他挂载会被安全门拒绝，避免跨卷复制。
7. **预演=执行**：`thin clean` 的 dry-run 与 `--apply` 使用**同一安全门**（`clean::plan`），被跳过项不会计入「可释放」。
8. **运行中的 App / 系统 App**：`uninstall --apply` 前检测运行状态（超时视为运行中）；系统关键 App（列表见 `apps.rs`）禁止卸载。`--deep` 仅用 Spotlight 补充候选（限定 `~/Library`、`~/.config` 等受信任根，文件名命中 App 名/bundle id 等强 token），候选仍走同一安全门，不改变删除语义。
9. **外部命令超时**：`tmutil`/`plutil`/`pgrep`/`mount`/`date` 均有超时，避免挂死。
10. **写规则前先确认路径真实存在且可清理**；`regenerable=false` 的项风险应设为 `confirm` 或 `destructive`。
11. 不确定时，宁可 `risk=confirm` 且 `--dry-run` 先看结果。
12. **永久删除要确认**：`thin quarantine purge` 默认二次确认；agent 显式传 `--yes` 才跳过，并应先用 `--dry-run` 预览。
13. **App 沙盒容器需要「完全磁盘访问权限」**：`~/Library/Containers`、`Group Containers` 受 TCC 保护，终端未授权时 `thin orphans` / `uninstall` 的容器项会失败（thin 会明确提示，不再只报「权限问题」）；应先授权再清理。
14. **绝不 `sudo thin`**：整个进程提权会让 `~/.thin`（用户规则 / 保护名单 / 隔离区）和废纸篓全部落到 root 名下，静默绕过保护。需要 root 的项用 `thin clean --apply --sudo`：提权子进程会**重新过同一安全门**，仍移入用户隔离区（可恢复），绝不 `rm`。

---

## 5. 最小示例：发现并收纳一个未归类缓存

```bash
export THIN_HOME=$PWD/.thin-sandbox

thin discover ~/Library --min 200MB --json          # 发现未归类目录
thin rules add --path "~/Library/Application Support/RetroArch" \
  --risk safe --category app-cache --regenerable    # 写成规则
thin scan --detail custom-retroarch                 # 验证命中
thin clean --apply --id custom-retroarch --yes      # 移入系统废纸篓（可恢复）
# thin clean --quarantine --apply --id custom-retroarch --yes  # 或进隔离区
# thin quarantine restore <session>                  # 隔离区模式需要时还原
```
