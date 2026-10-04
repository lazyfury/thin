# AGENTS.md — thin 仓库开发指南

> 本文件面向**在 thin 仓库里改代码**的 agent / 开发者：项目结构、构建测试、架构约定与安全不变量。
> 面向**使用 thin 完成磁盘清理**的 agent 提示词是另一份文档：[`docs/agent-prompt.md`](docs/agent-prompt.md)，
> 由 `thin agents` 子命令编译期内嵌并原样打印。两者受众不同，请勿混用。

---

## 1. 项目是什么

thin 是 macOS 系统空间扫描与安全清理的 **Rust CLI + TUI**。核心原则：

**清理 = 移入系统废纸篓（可恢复），绝不 `rm`；每一项都能解释「这是什么 / 删了会怎样 / 能否恢复」。**

- 诚实核算：按实际分配块统计，按 inode 去重，排除嵌套重复路径；
- 风险分级：`safe` / `confirm` / `destructive`，默认只把前两类计入可回收；
- 统一安全门：dry-run 与 `--apply` 走同一条路径，被跳过项不计入「可释放」；
- Agent 友好：关键命令都有 `--json`，`plan → apply` 两阶段可非交互跑通。

用户文档、命令速查与规则 schema 以 [`README.md`](README.md) 为准；产品设计愿景见 [`DESIGN.md`](DESIGN.md)。

---

## 2. 仓库布局

```
crates/
  thin-core/   核心库（无 UI 依赖）
    src/{lib,model,fsutil,probe,rules,scan,clean,protect,discover,finder,
         apps,preset,history,schedule,status,progress,proc,recognize,
         catalog,script,platform,tree}.rs
    rules/default.json        内置清理规则
  thin-cli/    前端：clap CLI (main.rs) + ratatui TUI (tui.rs) + report/browse/ls/top/treemap
  thin-sys/    macOS 底层能力 FFI（Swift/ThinKit）；无 Swift 工具链时降级为纯 Rust
docs/
  agent-prompt.md   ★ 面向使用者的 agent 提示词（`thin agents` 内嵌）
  swift-ffi.md      Swift FFI 落地计划
swift/         ThinKit Swift 源码（FFI 后端）
experiments/   FFI 等实验代码
DESIGN.md      产品设计愿景（以 README + 代码现状为准）
README.md      用户文档
```

**分层目的**：核心逻辑可被 CLI、TUI、未来的 SwiftUI GUI 或测试复用。`thin-core` 不得依赖任何 UI 库。

---

## 3. 构建 / 测试 / 检查

```bash
cargo build --release              # 产物 target/release/thin
cargo test                         # 全量测试
cargo clippy --all-targets         # 保持无 warning
cargo fmt

# 无 Swift 工具链时验证纯 Rust 降级路径
cargo build -p thin-core --no-default-features
```

工具链：stable（edition 2024 + let-chains，见 `rust-toolchain.toml`）。Swift 后端缺失时构建仍须成功。

---

## 4. 架构约定

- **逻辑分层**：`thin-core` 不依赖 UI；CLI/TUI/未来 SwiftUI 都复用其 API。
- **规则优先**：新增可清理项优先写成声明式规则（`rules/default.json` + 用户 `~/.thin/rules.d/*.json`），
  而不是在代码里硬编码路径。
- **匹配器三类**：`path`、`findDir`（目录名 + `requireSibling` 标记 + `maxDepth`）、
  `script`（动态枚举，必须带 `review.hash` 脚本风险审查，见 `script.rs`）。
- **外部命令必须带超时**：`tmutil`/`plutil`/`pgrep`/`mount`/`date`/`swift` 等一律加超时，避免挂死。
- **核算诚实**：体积统计要排除嵌套重复、跨卷、受保护项，不能虚高。

---

## 5. 安全不变量（改代码时不得破坏）

1. **永不直接删除**：清理只经系统废纸篓或 `--quarantine` 隔离区；不得引入绕过统一入口的 `rm`。
2. **预演 = 执行**：dry-run 与 `--apply` 共用同一安全门（`clean::plan`）；被跳过项不计入「可释放」。
3. **受保护路径**永不可清理：`/`、`/System`、`/usr`、`/bin`、`/sbin`、`/etc`、`/private/var/vm`、
   `/private/var/db`（`diagnostics`/`uuidtext` 除外）、`/Library/Apple`、`/Library/Extensions`、
   Keychains、iCloud、`~/Library/CloudStorage`、挂载点、整个主目录。
4. **裸顶层目录**只删内容、不删目录本身：`/Applications`、`/Library`、`/opt`、`/Volumes`、`/Users`、`/private/var` 等。
5. **卷隔离**：目标必须与隔离区（`~/.thin`）/废纸篓同卷，跨界一律拒绝。
6. 受保护/个人目录（`~/Documents`、iCloud、`~/.thin` 等）不得建成清理规则。
7. `regenerable=false` 的项风险必须是 `confirm` 或 `destructive`。
8. 运行中的 App 禁止卸载；系统关键 App 在 `apps.rs` 的白名单内。
9. **`deny delete` ACL**：`~/Library/Caches`、`~/Library/Logs` 等目录带 `group:everyone deny delete`，
   无法整体移动。**废纸篓与隔离区两条清理路径**都必须退化为「只搬内容」（等价 `rm -rf <dir>/*`），
   不得让整项失败——只改其中一条会重现「扫描得到却清不掉」的回归。

---

## 6. 命令 → 代码索引

| 命令 | 主要实现 |
|---|---|
| `thin probe` | `thin-core/src/probe.rs`、`platform.rs` |
| `thin scan` / `plan` / `apply` | `scan.rs`、`clean.rs` |
| `thin discover` / `top` | `discover.rs`、`thin-cli/src/top.rs` |
| `thin rules ...` | `rules.rs` + `thin-cli/src/main.rs` `cmd_rules` |
| `thin clean` / quarantine | `clean.rs` |
| `thin protect` | `protect.rs` |
| `thin preset` / `history` / `schedule` | `preset.rs` / `history.rs` / `schedule.rs` |
| `thin large` / `dupes` | `finder.rs` |
| `thin apps` / `uninstall` | `apps.rs`、`recognize.rs` |
| `thin ls` / `tui` | `thin-cli/src/ls.rs` / `tui.rs`、`browse.rs`、`treemap.rs` |
| `thin agents` | `thin-cli/src/main.rs`（内嵌 `docs/agent-prompt.md`） |

---

## 7. 修改 `thin agents` 的输出

`thin agents` 打印的是**用户提示词**，不是本文件。要更新其内容，改
[`docs/agent-prompt.md`](docs/agent-prompt.md)；它通过 `include_str!` 在编译期内嵌，
改完重新构建即可，无需运行时读取仓库文件（保证安装后的二进制离线可读）。

---

## 8. 提交流程

- 一个提交只做一件事，提交信息用中文，格式 `type(scope): 摘要`（`feat`/`fix`/`refactor`/`docs`/`test`/`chore`）。
- 改动规则 schema / 安全门 / 用户提示词时，同步更新 README、`docs/agent-prompt.md` 或本文件。
- 提交前确保 `cargo test` 与 `cargo clippy --all-targets` 通过。
