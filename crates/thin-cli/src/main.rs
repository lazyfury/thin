mod browse;
mod cmd;
mod ls;
mod report;
mod text;
mod toast;
mod top;
mod treemap;
mod tui;

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser, Subcommand};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use thin_core::fmt::human;
use thin_core::model::{Category, Risk};
use thin_core::{
    CleanItem, Rule, apps, clean, discover, finder, fsutil, history, orphans, preset, probe,
    protect, rules, scan, schedule, tree,
};

use cmd::apps::{cmd_apps, cmd_orphans, cmd_uninstall};
use cmd::clean::{cmd_clean, cmd_protect, cmd_quarantine};
use cmd::rules::cmd_rules;
use cmd::scan::{cmd_apply, cmd_discover, cmd_dupes, cmd_large, cmd_plan, cmd_scan};
use cmd::state::{cmd_history, cmd_preset, cmd_schedule};
use cmd::update::cmd_update;

#[derive(Parser)]
#[command(
    name = "thin",
    about = "macOS 系统空间扫描与安全清理 (M5 · 预设/历史/定时)",
    after_help = "Agent 工作流: thin agents（探索 → 写规则 → 可恢复清理；含安全约束与命令速查）",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // CLI 参数枚举只解析一次，无需为体积 boxing
enum Cmd {
    /// 磁盘概览：容量、卷、快照、外接盘
    Probe,

    /// 扫描已知可清理项并估算可回收空间
    Scan(ScanArgs),

    /// 列出某目录下最大的子项（类似 du -sh PATH/* | sort -rh）
    Top(TopArgs),

    /// 浏览目录：列出子项并标注用途（学习向，只读）
    Ls(ls::LsArgs),

    /// 交互式 TUI（浏览、勾选、移入隔离区）
    Tui(TuiArgs),

    /// 规则：列出 / 新增（agent 入口）/ 删除
    Rules(RulesArgs),

    /// 归因：找出未被规则覆盖的大目录
    Discover(DiscoverArgs),

    /// 清理：默认 dry-run 预览；--apply 移入隔离区（可恢复）
    Clean(CleanArgs),

    /// 生成清理计划（只读 JSON，供 agent / 审阅后 apply）
    Plan(PlanArgs),

    /// 执行已审阅的清理计划（--plan 文件或 stdin）
    Apply(ApplyArgs),

    /// 管理隔离区：列出 / 恢复 / 永久删除
    Quarantine(QuarantineArgs),

    /// 查找大文件
    Large(LargeArgs),

    /// 查找重复文件
    Dupes(DupesArgs),

    /// 列出已安装 App（含关联残留）
    Apps(AppsArgs),

    /// 卸载 App（App 及其残留一并移入隔离区）
    Uninstall(UninstallArgs),

    /// 已卸载 App 的孤立残留（App 本体已不存在，仅剩缓存/容器/偏好）
    Orphans(OrphansArgs),

    /// 清理预设：列出 / 查看 / 新增 / 删除（定时任务只执行用户预设）
    Preset(PresetArgs),

    /// 清理历史记录
    History(HistoryArgs),

    /// 定时任务：安装 / 卸载 / 状态 / 立即运行（launchd）
    Schedule(ScheduleArgs),

    /// 保护名单：被标记的路径及其子目录永不清理
    Protect(ProtectArgs),

    /// Agent 工作流：打印内嵌的用户提示词（探索 → 写规则 → 可恢复清理）
    Agents,

    /// 通过 cargo 从 git 安装/更新 thin 自身
    Update(UpdateArgs),

    /// 内部：提权子进程入口，供 `clean --sudo` 通过系统授权框调用（不面向用户）
    #[command(hide = true, name = "__elevated-move")]
    ElevatedMove(ElevatedMoveArgs),
}

#[derive(clap::Args)]
struct UpdateArgs {
    /// 从指定 git 仓库安装（默认官方仓库）
    #[arg(long, default_value = "https://github.com/lazyfury/thin.git")]
    git: String,
    /// 只打印将执行的命令，不实际安装
    #[arg(long)]
    dry_run: bool,
}

#[derive(clap::Args)]
struct ElevatedMoveArgs {
    /// 提权清单 JSON（由 `thin clean --sudo` 生成）
    #[arg(long)]
    manifest: String,

    /// 调用者主目录：隔离区落在这里并 chown 回用户
    #[arg(long)]
    user_home: String,

    /// 输出账本 JSON
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct ScanArgs {
    /// 包含不可再生项（虚拟机等）
    #[arg(long)]
    all: bool,

    /// 输出 JSON
    #[arg(long)]
    json: bool,

    /// 最小体积过滤，如 100MB / 1G
    #[arg(long, default_value = "1MB")]
    min: String,

    /// 按预设筛选（见 thin preset list；如 dev）
    #[arg(long)]
    preset: Option<String>,

    /// 只统计指定根目录之下的项（定向清理，如 . 表示当前项目）
    #[arg(long)]
    root: Option<String>,

    /// 显示指定规则的详细说明（可多次）
    #[arg(long = "detail")]
    detail: Vec<String>,

    /// 同时列出需 sudo / 受系统保护、thin 不会处理的项（默认隐藏）
    #[arg(long)]
    manual: bool,

    /// 以「按文件夹合并」的树形展示（只读）
    #[arg(long)]
    tree: bool,
}

#[derive(clap::Args)]
struct TopArgs {
    /// 要分析的目录
    #[arg(default_value = "~")]
    path: String,

    /// 显示条数
    #[arg(short, long, default_value_t = 20)]
    limit: usize,
}

#[derive(clap::Args)]
struct TuiArgs {
    /// 最小体积过滤
    #[arg(long, default_value = "1MB")]
    min: String,
}

#[derive(clap::Args)]
struct LargeArgs {
    /// 搜索根目录
    #[arg(default_value = "~")]
    root: String,
    /// 最小体积
    #[arg(long, default_value = "100MB")]
    min: String,
    /// 显示条数
    #[arg(short, long, default_value_t = 30)]
    limit: usize,
    /// 输出 JSON
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct DupesArgs {
    /// 搜索根目录
    #[arg(default_value = "~")]
    root: String,
    /// 最小体积
    #[arg(long, default_value = "1MB")]
    min: String,
    /// 显示组数
    #[arg(short, long, default_value_t = 50)]
    limit: usize,
    /// 把每组除首个外的副本移入系统废纸篓（`--quarantine` 改回隔离区）
    #[arg(long)]
    apply: bool,
    /// 跳过确认
    #[arg(long)]
    yes: bool,
    /// 输出 JSON（只读报告；与 --apply 互斥）
    #[arg(long)]
    json: bool,
    /// 改用 thin 隔离区（默认系统废纸篓）
    #[arg(long)]
    quarantine: bool,
}

#[derive(clap::Args)]
struct AppsArgs {
    /// 最小总占用过滤（默认不过滤，列出全部）
    #[arg(long, default_value = "0")]
    min: String,
}

#[derive(clap::Args)]
struct UninstallArgs {
    /// App 名称（模糊匹配）
    query: String,
    /// 实际执行（默认只预览）
    #[arg(long)]
    apply: bool,
    /// 跳过确认
    #[arg(long)]
    yes: bool,
    /// 改用 thin 隔离区（默认 App 及残留移入系统废纸篓）
    #[arg(long)]
    quarantine: bool,
    /// 启用 Spotlight 深扫，补充带后缀/嵌套的关联残留（较慢）
    #[arg(long)]
    deep: bool,
    /// 卸载前先退出正在运行的 App（优雅退出 → 强制结束），仍移入废纸篓/隔离区
    #[arg(long)]
    kill: bool,
    /// 对需要 root 的系统级残留弹出系统授权框，提权后移入隔离区
    #[arg(long)]
    sudo: bool,
}

#[derive(clap::Args)]
struct OrphansArgs {
    /// 输出 JSON
    #[arg(long)]
    json: bool,
    /// 实际执行（默认只预览）
    #[arg(long)]
    apply: bool,
    /// 跳过确认
    #[arg(long)]
    yes: bool,
    /// 改用 thin 隔离区（默认移入系统废纸篓）
    #[arg(long)]
    quarantine: bool,
    /// 对需要 root 的系统级残留弹出系统授权框，提权后移入隔离区
    #[arg(long)]
    sudo: bool,
}

#[derive(clap::Args)]
struct RulesArgs {
    #[command(subcommand)]
    cmd: Option<RulesCmd>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // CLI 参数枚举只解析一次，无需为体积 boxing
enum RulesCmd {
    /// 列出所有规则（默认）
    List,
    /// 显示用户规则文件路径
    Path,
    /// 新增/覆盖一条规则（agent 入口）
    Add(RuleAddArgs),
    /// 删除一条用户规则
    Remove(RuleRemoveArgs),
    /// 导出用户规则（便于并入内置 default.json）
    Export(ExportArgs),
}

#[derive(clap::Args)]
struct RuleAddArgs {
    /// 要清理的路径（构造 path 规则）
    #[arg(long)]
    path: Option<String>,
    /// 直接给出完整规则 JSON，或 "-" 从 stdin 读取，或文件路径
    #[arg(long)]
    json: Option<String>,
    #[arg(long)]
    id: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value = "other")]
    category: String,
    #[arg(long, default_value = "confirm")]
    risk: String,
    #[arg(long)]
    regenerable: bool,
    #[arg(long, default_value = "")]
    reclaim: String,
    #[arg(long)]
    what: Option<String>,
    #[arg(long)]
    cost: Option<String>,
    #[arg(long)]
    recover: Option<String>,
    /// 审查并批准规则中的脚本型 matcher（写入脚本内容哈希）
    #[arg(long)]
    approve_script: bool,
}

#[derive(clap::Args)]
struct ExportArgs {
    /// 只导出内置里不存在的规则 id
    #[arg(long = "new")]
    new_only: bool,
    /// 只导出指定 id
    #[arg(long)]
    id: Option<String>,
}

#[derive(clap::Args)]
struct RuleRemoveArgs {
    id: String,
}

#[derive(clap::Args)]
struct DiscoverArgs {
    /// 分析根目录
    #[arg(default_value = "~")]
    root: String,
    /// 最小体积过滤
    #[arg(long, default_value = "500MB")]
    min: String,
    /// 输出 JSON
    #[arg(long)]
    json: bool,
    /// 额外统计 iCloud 云占位（逐文件查询，较慢）
    #[arg(long)]
    icloud: bool,
}

#[derive(clap::Args)]
struct ProtectArgs {
    #[command(subcommand)]
    cmd: ProtectCmd,
}

#[derive(Subcommand)]
enum ProtectCmd {
    /// 列出保护名单
    List,
    /// 加入保护名单（默认保护该目录及其所有子目录）
    Add {
        /// 路径，支持 ~；在项目根用 . 即可保护整个项目
        #[arg(default_value = ".")]
        path: String,
    },
    /// 从保护名单移除
    Remove {
        #[arg(default_value = ".")]
        path: String,
    },
}

#[derive(clap::Args)]
struct CleanArgs {
    /// 实际执行：移入隔离区（默认为 dry-run 预览）
    #[arg(long)]
    apply: bool,

    /// 仅预览，不执行（默认行为，显式指定更清晰）
    #[arg(long)]
    dry_run: bool,

    /// 跳过确认提示
    #[arg(long)]
    yes: bool,

    /// 连同「需确认」项一起处理（默认只处理「安全」项）
    #[arg(long)]
    all: bool,

    /// 只处理指定规则 id（可多次；可用于不可再生项）
    #[arg(long = "id")]
    ids: Vec<String>,

    /// 按预设筛选清理项（见 thin preset list；如 dev）
    #[arg(long)]
    preset: Option<String>,

    /// 只处理指定根目录之下的项（定向清理，如 . 表示当前项目）
    #[arg(long)]
    root: Option<String>,

    /// 输出 JSON：dry-run 输出清理计划；--apply 输出账本（需配合 --yes）
    #[arg(long)]
    json: bool,

    /// 同时预览需 sudo / 受系统保护、thin 不会处理的项（默认隐藏）
    #[arg(long)]
    manual: bool,

    /// 对需要 root 的项弹出系统授权框，提权后移入隔离区（仅限内置 sudo 规则）
    #[arg(long)]
    sudo: bool,

    /// 以「按文件夹合并」的树形预览清理项
    #[arg(long)]
    tree: bool,

    /// （默认）以系统废纸篓作为清理方式（Finder 可恢复；需 Swift 后端）
    #[arg(long, conflicts_with = "quarantine")]
    trash: bool,

    /// 改用 thin 隔离区（`quarantine restore` 可恢复），而非默认的系统废纸篓
    #[arg(long)]
    quarantine: bool,
}

#[derive(clap::Args)]
struct PlanArgs {
    /// 按预设筛选（见 thin preset list；如 dev）
    #[arg(long)]
    preset: Option<String>,

    /// 只处理指定根目录之下的项（定向清理，如 . 表示当前项目）
    #[arg(long)]
    root: Option<String>,

    /// 连同「需确认」项一起纳入计划（默认只含「安全」项）
    #[arg(long)]
    all: bool,

    /// 只纳入指定规则 id（可多次）
    #[arg(long = "id")]
    ids: Vec<String>,
}

#[derive(clap::Args)]
struct ApplyArgs {
    /// 要执行的计划 JSON：文件路径，或 `-` 从 stdin 读取（由 `thin plan` 生成）
    #[arg(long)]
    plan: String,

    /// 跳过确认（计划会再次过安全门；仍建议先看 thin plan 输出）
    #[arg(long)]
    yes: bool,

    /// 改用 thin 隔离区（默认移入系统废纸篓）
    #[arg(long)]
    quarantine: bool,
}

#[derive(clap::Args)]
struct PresetArgs {
    #[command(subcommand)]
    cmd: Option<PresetCmd>,
}

#[derive(Subcommand)]
enum PresetCmd {
    /// 列出预设（内置默认 + 用户自定义）
    List,
    /// 查看某预设详情
    Show { id: String },
    /// 新增/覆盖一条用户预设（默认只选 cache 类）
    Add(PresetAddArgs),
    /// 删除一条用户预设
    Remove { id: String },
}

#[derive(clap::Args)]
struct PresetAddArgs {
    id: String,
    #[arg(long)]
    name: Option<String>,
    /// 类别，可多次；不指定则默认 system-cache/app-cache/dev-cache
    #[arg(long = "category")]
    categories: Vec<String>,
    /// 允许的风险等级，可多次（默认 safe）
    #[arg(long = "risk")]
    risks: Vec<String>,
    /// 只处理可再生项
    #[arg(long)]
    regenerable_only: bool,
    /// 仅处理这些规则 id，可多次
    #[arg(long = "include")]
    include: Vec<String>,
    /// 排除这些规则 id，可多次
    #[arg(long = "exclude")]
    exclude: Vec<String>,
    /// 隔离保留天数（定时运行先 purge 早于该天数的会话）
    #[arg(long)]
    purge_after_days: Option<u64>,
}

#[derive(clap::Args)]
struct HistoryArgs {
    /// 显示条数
    #[arg(short, long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
    /// 用隔离区账本回填缺失的历史记录
    #[arg(long)]
    reconcile: bool,
}

#[derive(clap::Args)]
struct ScheduleArgs {
    #[command(subcommand)]
    cmd: ScheduleCmd,
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// 安装/更新定时任务（仅限用户自定义预设）
    Install(ScheduleInstallArgs),
    /// 卸载定时任务
    Uninstall,
    /// 查看定时任务状态
    Status,
    /// 立即按预设运行一次（launchd 也调用这个）
    Run {
        #[arg(long)]
        preset: String,
        /// 只预览将清理的项，不 purge / 不隔离 / 不写历史
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Args)]
struct ScheduleInstallArgs {
    /// 要执行的预设 id（必须先用 thin preset add 创建）
    #[arg(long)]
    preset: String,
    /// 每天
    #[arg(long)]
    daily: bool,
    /// 每周
    #[arg(long)]
    weekly: bool,
    /// 小时 0-23
    #[arg(long, default_value_t = 10)]
    hour: u32,
    /// 分钟 0-59
    #[arg(long, default_value_t = 0)]
    minute: u32,
    /// 周几（0/7=周日），仅 --weekly 有效
    #[arg(long, default_value_t = 0)]
    weekday: u32,
    /// 固定间隔秒数（与 daily/weekly 二选一）
    #[arg(long)]
    interval: Option<u64>,
    /// 只生成并打印 plist，不加载（安全预览）
    #[arg(long)]
    dry_run: bool,
}

#[derive(clap::Args)]
struct QuarantineArgs {
    #[command(subcommand)]
    cmd: QuarantineCmd,
}

#[derive(Subcommand)]
enum QuarantineCmd {
    /// 列出所有隔离会话
    List,
    /// 恢复（移回原位置）
    Restore(RestoreArgs),
    /// 永久删除
    Purge(PurgeArgs),
}

#[derive(clap::Args)]
struct RestoreArgs {
    /// 会话 id；省略则恢复最近一次
    session: Option<String>,
    /// 恢复全部会话
    #[arg(long)]
    all: bool,
}

#[derive(clap::Args)]
struct PurgeArgs {
    /// 会话 id
    session: Option<String>,
    /// 永久删除全部会话
    #[arg(long)]
    all: bool,
    /// 永久删除早于 N 天的会话，如 7d
    #[arg(long, default_value = "")]
    older_than: String,
    /// 只预览将永久删除的会话，不执行
    #[arg(long)]
    dry_run: bool,
    /// 跳过确认（永久删除不可恢复，务必确认）
    #[arg(long)]
    yes: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // 拒绝 `sudo thin`：整个进程提权会让 thin_home / protect 名单 / 废纸篓全部
    // 落到 root 名下，静默绕过用户保护；提权只允许走内置的 __elevated-move 子进程。
    if clean::is_root() && !matches!(&cli.cmd, Some(Cmd::ElevatedMove(_))) {
        return reject_root();
    }
    // 无子命令：默认进入 TUI（非交互环境下退化为打印帮助）
    let Some(cmd) = cli.cmd else {
        return default_entry();
    };
    match cmd {
        Cmd::Probe => probe::probe_summary()?,
        Cmd::Scan(args) => cmd_scan(args)?,
        Cmd::Top(args) => {
            let path = fsutil::expand(&args.path).unwrap_or_else(|| PathBuf::from(&args.path));
            top::run(path, args.limit);
        }
        Cmd::Tui(args) => cmd_tui(args)?,
        Cmd::Ls(args) => ls::run(args)?,
        Cmd::Rules(args) => cmd_rules(args)?,
        Cmd::Discover(args) => cmd_discover(args)?,
        Cmd::Clean(args) => cmd_clean(args)?,
        Cmd::Plan(args) => cmd_plan(args)?,
        Cmd::Apply(args) => cmd_apply(args)?,
        Cmd::Quarantine(args) => cmd_quarantine(args)?,
        Cmd::Large(args) => cmd_large(args)?,
        Cmd::Dupes(args) => cmd_dupes(args)?,
        Cmd::Apps(args) => cmd_apps(args)?,
        Cmd::Uninstall(args) => cmd_uninstall(args)?,
        Cmd::Orphans(args) => cmd_orphans(args)?,
        Cmd::Preset(args) => cmd_preset(args)?,
        Cmd::History(args) => cmd_history(args)?,
        Cmd::Schedule(args) => cmd_schedule(args)?,
        Cmd::Protect(args) => cmd_protect(args)?,
        Cmd::Agents => print_agents(),
        Cmd::Update(args) => cmd_update(args)?,
        Cmd::ElevatedMove(args) => cmd_elevated_move(args)?,
    }
    Ok(())
}

/// `sudo thin` 的拒绝提示：说明会破坏哪些安全不变量，并给出正确做法。
fn reject_root() -> Result<()> {
    Err(anyhow!(
        "拒绝以 root 运行 thin：\n  \
         · 会使用 root 的 ~/.thin，用户规则 / 保护名单 / 隔离区全部失效\n  \
         · 废纸篓会落到 root 名下，Finder 看不到、无法恢复\n  \
         · script 型规则与 ~/.thin/rules.d 会以 root 执行，存在提权风险\n\
         请以普通用户运行；需要 root 的项用 `thin clean --apply --sudo`（会弹系统授权框）。"
    ))
}

/// 内部提权子进程：复核清单并移入调用者隔离区，stdout 输出账本 JSON。
fn cmd_elevated_move(args: ElevatedMoveArgs) -> Result<()> {
    let home = PathBuf::from(&args.user_home);
    let journal = clean::elevated_move(Path::new(&args.manifest), &home)?;
    if args.json {
        println!("{}", serde_json::to_string(&journal)?);
    } else {
        for e in &journal.entries {
            println!("已移入隔离区 {}", e.original.display());
        }
    }
    Ok(())
}

/// 面向使用者的 agent 提示词在编译期嵌入：安装出来的二进制旁边没有仓库文件，
/// 内嵌才能保证 `thin agents` 离线可读、永不失效。
/// 注意：这是用户提示词，不是仓库开发指南（仓库根 AGENTS.md）。
const AGENT_PROMPT: &str = include_str!("../../../docs/agent-prompt.md");

/// `thin agents`：把 agent 工作流原样打到 stdout，便于 `thin agents | ...` 或直接喂给 agent。
fn print_agents() {
    print!("{AGENT_PROMPT}");
    if !AGENT_PROMPT.ends_with('\n') {
        println!();
    }
}

/// 无子命令时的默认入口：TTY 下直接进 TUI，否则打印帮助（便于脚本/管道不会卡住）
fn default_entry() -> Result<()> {
    if std::io::stdout().is_terminal() {
        cmd_tui(TuiArgs {
            min: "1MB".to_string(),
        })
    } else {
        Cli::command().print_help()?;
        println!();
        Ok(())
    }
}

fn cmd_tui(args: TuiArgs) -> Result<()> {
    let min = parse_size_arg(&args.min)?;
    let root = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    tui::run(root, min)?;
    Ok(())
}
/// 按参数筛选清理项
fn select_items(args: &CleanArgs) -> Result<Vec<thin_core::CleanItem>> {
    let catalog = rules::load()?;
    let root = args.root.as_deref().map(expand_root);
    let items = scan::scan_scoped(&catalog, true, 1_048_576, root.as_deref());
    select_scoped(
        items,
        args.preset.as_deref(),
        &args.ids,
        args.all,
        args.root.as_deref(),
    )
}

/// 统一的清理项筛选：预设 / 规则 id / 风险 / 根目录作用域。
///
/// `root` 是「定向清理」的关键：只保留指定目录（含子目录）下的项。
fn select_scoped(
    items: Vec<thin_core::CleanItem>,
    preset_id: Option<&str>,
    ids: &[String],
    all: bool,
    root: Option<&str>,
) -> Result<Vec<thin_core::CleanItem>> {
    // 预设优先：只处理预设命中的项
    let selected = if let Some(pid) = preset_id {
        let p = preset::get(pid).ok_or_else(|| anyhow!("未找到预设 {pid}（thin preset list）"))?;
        preset::select(&p, &items)
    } else {
        let selected: Vec<thin_core::CleanItem> = items
            .into_iter()
            .filter(|it| {
                if !ids.is_empty() {
                    return ids.contains(&it.rule_id);
                }
                match it.risk {
                    Risk::Safe => true,
                    Risk::Confirm => all,
                    Risk::Destructive => false,
                }
            })
            .collect();
        // 只处理最顶层项：避免父目录与其子目录重复计数、重复移动
        scan::top_level(&selected)
    };

    if let Some(root) = root {
        return Ok(scan::scope_items(selected, Some(&expand_root(root))));
    }
    Ok(selected)
}

fn print_plan(plan: &clean::Plan, tree: bool, mode: clean::Mode) {
    if tree {
        report::print_tree(&tree::build_forest(&plan.approved, home_dir().as_deref()));
        let total = plan.approved_bytes();
        println!(
            "\n共 {} 项，预计可释放 \x1b[1m{}\x1b[0m",
            plan.approved.len(),
            human(total)
        );
        if !plan.skipped.is_empty() {
            println!(
                "\n\x1b[33m安全门跳过 {} 项（不计入可释放）:\x1b[0m",
                plan.skipped.len()
            );
            for s in &plan.skipped {
                println!("  - {}：{}", shorten(&s.path), s.reason);
            }
        }
        return;
    }
    println!("\x1b[1m清理计划\x1b[0m");
    println!(
        "\x1b[90m以下为官方推荐的清理方式；thin 将目标移入{}（可恢复），不会执行这些命令。\x1b[0m\n",
        mode.label()
    );
    for it in &plan.approved {
        println!("• {:<28} {:>10}", it.name, human(it.size));
        println!("  {:<28} {}", "官方方式:", it.reclaim);
        println!("  {:<28} 移入{}（可恢复）", "thin 动作:", mode.label());
        println!("  {:<28} {}", "路径:", it.path.display());
    }
    let total = plan.approved_bytes();
    println!(
        "\n共 {} 项，预计可释放 \x1b[1m{}\x1b[0m",
        plan.approved.len(),
        human(total)
    );
    // 与真实执行使用同一安全门：被跳过项也如实展示
    if !plan.skipped.is_empty() {
        println!(
            "\n\x1b[33m安全门跳过 {} 项（不计入可释放）:\x1b[0m",
            plan.skipped.len()
        );
        for s in &plan.skipped {
            println!("  - {}：{}", shorten(&s.path), s.reason);
        }
    }
}
fn expand_root(s: &str) -> PathBuf {
    fsutil::expand(s).unwrap_or_else(|| PathBuf::from(s))
}

/// 规范化的家目录（解析 `/var` → `/private/var` 等符号链接），供树形视图裁剪前缀。
fn home_dir() -> Option<PathBuf> {
    let raw = std::env::var("HOME").ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(fsutil::canonicalize_or(Path::new(&raw)))
}

/// 把家目录前缀显示为 ~
fn shorten(path: &Path) -> String {
    let s = path.display().to_string();
    let raw = std::env::var("HOME").unwrap_or_default();
    let canon = std::fs::canonicalize(&raw)
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    for h in [raw.as_str(), canon.as_deref().unwrap_or("")] {
        if !h.is_empty() && s.starts_with(h) {
            return s.replacen(h, "~", 1);
        }
    }
    s
}

fn truncate(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    let mut out: String = chars[..width.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

/// 体积分级对应的 ANSI 颜色
fn tier_ansi(t: apps::Tier) -> &'static str {
    match t {
        apps::Tier::Large => "\x1b[31m",
        apps::Tier::Medium => "\x1b[33m",
        apps::Tier::Small => "\x1b[32m",
    }
}
#[allow(clippy::too_many_arguments)]
fn record_history(
    trigger: &str,
    preset: Option<&str>,
    scanned: usize,
    approved: usize,
    purged: (usize, u64),
    journal: Option<&clean::Journal>,
) {
    let mut r = history::Record::new(trigger);
    r.preset = preset.map(str::to_string);
    r.scanned = scanned;
    r.approved = approved;
    r.purged_sessions = purged.0;
    r.purged_bytes = purged.1;
    if let Some(j) = journal {
        r.session = Some(j.session.clone());
        r.moved = j.entries.len();
        r.moved_bytes = j.total_size();
        r.skipped = j.skipped.len();
    }
    if let Err(e) = history::append(&r) {
        eprintln!("\x1b[33m写入历史失败: {e:#}\x1b[0m");
    }
}

/// 打印一次「移入系统废纸篓」的结果（含仅移入内容的退化情形）。
fn print_trash_report(report: &clean::TrashReport) {
    print!(
        "\x1b[1m已移入系统废纸篓\x1b[0m {} 项 · {}",
        report.moved_count(),
        human(report.trashed_bytes)
    );
    if report.contents_count() > 0 {
        print!(
            "\x1b[90m（其中 {} 项原目录受 ACL 保护，仅移入内容）\x1b[0m",
            report.contents_count()
        );
    }
    println!();
    for (p, why) in &report.failed {
        println!("  \x1b[33m失败\x1b[0m {}：{why}", shorten(p));
    }
}

/// 记录一次「移入系统废纸篓」到历史（无隔离会话）。
fn record_history_trash(
    preset: Option<&str>,
    scanned: usize,
    approved: usize,
    report: &clean::TrashReport,
) {
    let mut r = history::Record::new("manual-trash");
    r.preset = preset.map(str::to_string);
    r.scanned = scanned;
    r.approved = approved;
    r.moved = report.moved_count();
    r.moved_bytes = report.trashed_bytes;
    r.skipped = report.skipped.len() + report.failed.len();
    if let Err(e) = history::append(&r) {
        eprintln!("\x1b[33m写入历史失败: {e:#}\x1b[0m");
    }
}

fn print_journal(journal: &clean::Journal) {
    if journal.entries.is_empty() {
        println!("\n没有可执行项（全部被安全门跳过）:");
        for s in &journal.skipped {
            println!("  \x1b[33m跳过\x1b[0m {}：{}", s.path.display(), s.reason);
        }
        return;
    }
    println!(
        "\n\x1b[1m已移入隔离区\x1b[0m  会话 {}  共 {} 项，{}",
        journal.session,
        journal.entries.len(),
        human(journal.total_size())
    );
    for s in &journal.skipped {
        println!("  \x1b[33m跳过\x1b[0m {}：{}", s.path.display(), s.reason);
    }
    println!("\n恢复:       thin quarantine restore {}", journal.session);
    println!("永久删除:   thin quarantine purge {}", journal.session);
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

/// 解析 1MB / 500KB / 2G 之类的大小
/// 解析体积；非法输入直接报错，避免静默按默认值猜（如 --min abc）
fn parse_size_arg(s: &str) -> Result<u64> {
    parse_size(s).ok_or_else(|| anyhow!("无法解析体积 {s:?}（示例: 100MB / 1G / 500KB）"))
}

/// 解析天数；非法输入直接报错
fn parse_days_arg(s: &str) -> Result<u64> {
    parse_days(s).ok_or_else(|| anyhow!("无法解析天数 {s:?}（示例: 7d / 30）"))
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let upper = s.to_uppercase();
    let (num, mult) = if let Some(n) = upper.strip_suffix("PB").or_else(|| upper.strip_suffix('P'))
    {
        (n, 1024u64.pow(5))
    } else if let Some(n) = upper.strip_suffix("TB").or_else(|| upper.strip_suffix('T')) {
        (n, 1024u64.pow(4))
    } else if let Some(n) = upper.strip_suffix("GB").or_else(|| upper.strip_suffix('G')) {
        (n, 1024u64.pow(3))
    } else if let Some(n) = upper.strip_suffix("MB").or_else(|| upper.strip_suffix('M')) {
        (n, 1024u64.pow(2))
    } else if let Some(n) = upper.strip_suffix("KB").or_else(|| upper.strip_suffix('K')) {
        (n, 1024)
    } else if let Some(n) = upper.strip_suffix('B') {
        (n, 1)
    } else {
        (upper.as_str(), 1)
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * mult as f64) as u64)
}

/// 解析 7d / 7 天 之类的天数
fn parse_days(s: &str) -> Option<u64> {
    s.trim()
        .trim_end_matches(['d', 'D', '天'])
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_works() {
        assert_eq!(parse_size("1MB"), Some(1_048_576));
        assert_eq!(parse_size("500KB"), Some(512_000));
        assert_eq!(parse_size("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1TB"), Some(1024u64.pow(4)));
        assert_eq!(parse_size("1.5T"), Some((1.5 * 1024f64.powi(4)) as u64));
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("bad"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn parse_days_works() {
        assert_eq!(parse_days("7d"), Some(7));
        assert_eq!(parse_days("7 天"), Some(7));
        assert_eq!(parse_days("30"), Some(30));
        assert_eq!(parse_days("x"), None);
    }
}
