mod browse;
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
    CleanItem, Rule, apps, clean, discover, finder, fsutil, history, preset, probe, protect, rules,
    scan, schedule, tree,
};

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
        Cmd::Preset(args) => cmd_preset(args)?,
        Cmd::History(args) => cmd_history(args)?,
        Cmd::Schedule(args) => cmd_schedule(args)?,
        Cmd::Protect(args) => cmd_protect(args)?,
        Cmd::Agents => print_agents(),
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

fn cmd_scan(args: ScanArgs) -> Result<()> {
    let min = parse_size_arg(&args.min)?;
    let catalog = rules::load()?;
    let root = args.root.as_deref().map(expand_root);
    let mut items = scan::scan_scoped(&catalog, args.all, min, root.as_deref());
    if let Some(pid) = &args.preset {
        let p = preset::get(pid).ok_or_else(|| anyhow!("未找到预设 {pid}（thin preset list）"))?;
        items.retain(|it| preset::matches(&p, it));
    }
    let accounted = scan::accounted_bytes(&items);
    let volume = probe::statfs(&std::env::var("HOME").unwrap_or_else(|_| "/".into()));

    if args.json {
        let vol = volume.as_ref().map(|v| {
            serde_json::json!({
                "total": v.total,
                "used": v.used,
                "avail": v.avail,
            })
        });
        let out = serde_json::json!({
            "items": items,
            "summary": scan::summarize(&items),
            "accountedBytes": accounted,
            "volume": vol,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // 缺 FDA 时部分受保护目录会被系统隐藏（扫成 0 B），先明确提醒
    if probe::full_disk_access() == Some(false) {
        println!(
            "\x1b[33m提示：未授予「完全磁盘访问权限」，部分目录可能被系统隐藏而显示为 0 B。\x1b[0m"
        );
        println!(
            "\x1b[90m      系统设置 → 隐私与安全性 → 完全磁盘访问权限，勾选终端后重试。\x1b[0m\n"
        );
    }

    // 默认把「需 sudo / 受系统保护」的项整个藏起来：thin 本来就不会动它们，
    // 显示出来只会让人以为能清。--manual 才列出；JSON 与 --detail 仍基于全量 items。
    let visible: Vec<thin_core::CleanItem> = if args.manual {
        items.clone()
    } else {
        items
            .iter()
            .filter(|it| !scan::is_manual(it))
            .cloned()
            .collect()
    };

    if args.tree {
        report::print_tree(&tree::build_forest(&visible, home_dir().as_deref()));
    } else {
        report::print_table(&visible);
    }
    report::print_summary(&visible);
    if let Some(v) = &volume {
        println!(
            "规则覆盖（含嵌套去重）: {} ｜ 主卷已用 {}（{:.0}%）",
            human(accounted),
            human(v.used),
            accounted as f64 / v.used.max(1) as f64 * 100.0
        );
        println!(
            "\x1b[90m其余为系统/应用/用户数据，不属于可清理项；局部「未归类」请用 thin discover。\x1b[0m"
        );
    }

    if !args.detail.is_empty() {
        println!();
        for id in &args.detail {
            for it in items.iter().filter(|i| &i.rule_id == id) {
                report::print_detail(it);
                println!();
            }
        }
    }
    Ok(())
}

fn cmd_tui(args: TuiArgs) -> Result<()> {
    let min = parse_size_arg(&args.min)?;
    let root = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    tui::run(root, min)?;
    Ok(())
}

fn cmd_rules(args: RulesArgs) -> Result<()> {
    match args.cmd {
        None | Some(RulesCmd::List) => list_rules()?,
        Some(RulesCmd::Path) => {
            println!(
                "用户规则目录（写入点）: {}",
                rules::user_rules_dir().display()
            );
            println!(
                "旧格式规则文件（仅兼容读取）: {}",
                rules::user_rules_path().display()
            );
        }
        Some(RulesCmd::Add(a)) => cmd_rule_add(a)?,
        Some(RulesCmd::Export(e)) => cmd_rules_export(e)?,
        Some(RulesCmd::Remove(r)) => {
            if rules::remove_user_rule(&r.id)? {
                println!("已删除用户规则 {}", r.id);
            } else {
                println!(
                    "未找到用户规则 {}（内置规则不可删除，可用 THIN_RULES 覆盖）",
                    r.id
                );
            }
        }
    }
    Ok(())
}

fn list_rules() -> Result<()> {
    let catalog = rules::load()?;
    let user_ids: std::collections::HashSet<String> = rules::load_all_user_rules()?
        .into_iter()
        .map(|r| r.id)
        .collect();
    println!(
        "{:>8}  {:<10} {:<12} {:<6} 名称 / 规则 id",
        "风险", "类别", "可再生", "来源"
    );
    println!("{}", "-".repeat(88));
    for r in &catalog {
        let src = if user_ids.contains(&r.id) {
            "用户"
        } else {
            "内置"
        };
        println!(
            "  {:<6} {:<10} {:<12} {:<6} {}  \x1b[90m({})\x1b[0m",
            r.risk.label(),
            r.category.label(),
            if r.regenerable { "是" } else { "否" },
            src,
            r.name,
            r.id
        );
    }
    println!(
        "\n共 {} 条规则（内置 {}）。\n用户规则文件: {}\n用户规则目录: {}",
        catalog.len(),
        rules::builtin()?.len(),
        rules::user_rules_path().display(),
        rules::user_rules_dir().display()
    );
    Ok(())
}

/// 导出用户来源的规则为 JSON（可直接并入内置 default.json）
fn cmd_rules_export(args: ExportArgs) -> Result<()> {
    let user = rules::load_all_user_rules()?;
    let builtin_ids: std::collections::HashSet<String> =
        rules::builtin()?.into_iter().map(|r| r.id).collect();
    let selected: Vec<Rule> = user
        .into_iter()
        .filter(|r| {
            if let Some(id) = &args.id {
                return &r.id == id;
            }
            if args.new_only {
                !builtin_ids.contains(&r.id)
            } else {
                true
            }
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&selected)?);
    eprintln!(
        "# 共 {} 条用户规则（内置 {} 条）。\n# 并入内置: 将上面内容合进 crates/thin-core/rules/default.json 后重新构建；\n# 用户规则会按 id 覆盖内置，所以已提升的规则可从 ~/.thin 删除。",
        selected.len(),
        builtin_ids.len()
    );
    Ok(())
}

fn cmd_rule_add(args: RuleAddArgs) -> Result<()> {
    let mut rule = if let Some(spec) = &args.json {
        let raw = if spec == "-" {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        } else if std::path::Path::new(spec).exists() {
            std::fs::read_to_string(spec)?
        } else {
            spec.clone()
        };
        serde_json::from_str::<thin_core::Rule>(&raw)
            .map_err(|e| anyhow!("规则 JSON 解析失败: {e}"))?
    } else {
        let path = args
            .path
            .ok_or_else(|| anyhow!("需要 --path <路径> 或 --json <规则JSON>"))?;
        let id = args.id.unwrap_or_else(|| rules::slug_for(&path));
        let name = args.name.unwrap_or_else(|| {
            path.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("规则")
                .to_string()
        });
        let category = rules::parse_category(&args.category).unwrap_or(Category::Other);
        let risk = rules::parse_risk(&args.risk).unwrap_or(Risk::Confirm);
        let mode = clean::default_mode();
        let reclaim = if args.reclaim.is_empty() {
            format!("移入{}", mode.label())
        } else {
            args.reclaim
        };
        rules::make_path_rule(
            id,
            name,
            path,
            category,
            risk,
            args.regenerable,
            reclaim,
            args.what.unwrap_or_else(|| "自定义清理项".into()),
            args.cost
                .unwrap_or_else(|| format!("移入{}，可恢复", mode.label())),
            args.recover.unwrap_or_else(|| match mode {
                clean::Mode::Trash => "从 Finder 废纸篓恢复".into(),
                clean::Mode::Quarantine => "thin quarantine restore".into(),
            }),
        )
    };

    // 脚本型 matcher：显式批准时写入脚本内容的审查哈希（脚本变更即失效）
    if args.approve_script {
        if let thin_core::Matcher::Script { script, review, .. } = &mut rule.matcher {
            *review = Some(thin_core::ScriptReview {
                hash: thin_core::script::hash(script),
                note: Some("approved via `thin rules add --approve-script`".into()),
            });
        } else {
            anyhow::bail!("--approve-script 仅适用于 script 型 matcher");
        }
    }

    // 预检：受保护 / 个人目录顶层不得建成清理规则（与 clean 安全门共用同一判断）；
    // findDir 规则不得在无 requireSibling 约束下按名字查找敏感目录。
    // 这样 `discover` 之类的建议即使被直接照做，也会在写盘前被拦下，
    // 而不是等到 clean 才发现「整份 ~/Documents 会被搬走」。
    if let Err(reason) = rules::check_rule_safety(&rule) {
        anyhow::bail!(
            "拒绝写入规则 {}：{reason}\n提示：目标不得是受保护 / 过于宽泛的路径；findDir 需具体 dirName（必要时 requireSibling）；script 型需 roots 且脚本只应枚举路径、经 --approve-script 审查。",
            rule.id
        );
    }

    // 统一写入 `rules.d/<id>.json`（一规则一文件）
    let saved = rules::upsert_user_rule(rule.clone())?;
    println!(
        "已写入规则 \x1b[1m{}\x1b[0m  →  {}",
        rule.id,
        saved.display()
    );
    println!("验证: thin scan --detail {}", rule.id);
    Ok(())
}

fn cmd_discover(args: DiscoverArgs) -> Result<()> {
    let root = expand_root(&args.root);
    let min = parse_size_arg(&args.min)?;
    let catalog = rules::load()?;
    eprintln!("分析 {} …", root.display());
    let report = discover::analyze(&root, min, &catalog);

    if args.json {
        let findings: Vec<_> = report
            .findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "size": f.size,
                    "dataless": f.dataless,
                    "coverage": f.coverage.label(),
                    "uncovered": f.coverage.is_uncovered(),
                })
            })
            .collect();
        let out = serde_json::json!({
            "root": root,
            "total": report.total,
            "covered": report.covered,
            "partial": report.partial,
            "uncovered": report.uncovered,
            "dataless": report.dataless,
            "coverageRatio": report.coverage_ratio(),
            "findings": findings,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if report.findings.is_empty() {
        println!("未发现 >= {} 的子项。", human(min));
        return Ok(());
    }

    println!("{:>10}  {:<20} 路径", "大小", "归因");
    println!("{}", "-".repeat(90));
    for f in &report.findings {
        let tag = if f.coverage.is_uncovered() {
            format!("\x1b[33m{}\x1b[0m", f.coverage.label())
        } else {
            f.coverage.label()
        };
        let cloud = if f.dataless > 0 {
            format!("  \x1b[90m云占位 {}\x1b[0m", human(f.dataless))
        } else {
            String::new()
        };
        println!(
            "{:>10}  {:<20} {}{}",
            human(f.size),
            tag,
            shorten(&f.path),
            cloud
        );
    }

    let none: Vec<_> = report
        .findings
        .iter()
        .filter(|f| matches!(f.coverage, discover::Coverage::None))
        .collect();
    // 绝不建议把受保护 / 个人目录（如 ~/Documents、iCloud、~/.thin）或
    // 已加入保护名单的目录建成清理规则
    let protect_list = protect::load();
    let (suggestible, protected): (Vec<_>, Vec<_>) = none.into_iter().partition(|f| {
        clean::static_protection_reason(&f.path).is_none()
            && !protect::matches(&protect_list, &f.path)
    });
    let partial: Vec<_> = report
        .findings
        .iter()
        .filter(|f| matches!(f.coverage, discover::Coverage::Partial(_)))
        .collect();
    if !suggestible.is_empty() {
        println!("\n\x1b[1m未归类大目录 —— 可用 thin rules add 补规则:\x1b[0m");
        for f in &suggestible {
            let p = f.path.display().to_string();
            println!(
                "  thin rules add --path \"{}\" --id {}",
                p,
                rules::slug_for(&p)
            );
        }
    }
    if !protected.is_empty() {
        let list: Vec<String> = protected
            .iter()
            .map(|f| format!("{}（{}）", shorten(&f.path), human(f.size)))
            .collect();
        println!(
            "\n\x1b[90m已跳过 {} 个受保护目录的建议（个人目录 / thin protect）: {}\x1b[0m",
            protected.len(),
            list.join("、")
        );
    }
    if !partial.is_empty() {
        println!("\n\x1b[1m部分覆盖 —— 可继续下钻:\x1b[0m");
        for f in partial {
            println!("  thin discover \"{}\" --min 500MB", f.path.display());
        }
    }

    // 覆盖率自检：直接子项合计 vs 已归类/未归类，诚实回答「还有多少没归类」
    println!(
        "\n\x1b[1m覆盖率自检\x1b[0m（{} 的直接子项）",
        shorten(&root)
    );
    println!(
        "  合计 {} ｜ 已归类 {} ｜ 部分覆盖 {} ｜ 未归类 {} ｜ 覆盖 {:.0}%",
        human(report.total),
        human(report.covered),
        human(report.partial),
        human(report.uncovered),
        report.coverage_ratio() * 100.0
    );
    if report.dataless > 0 {
        println!(
            "  \x1b[90miCloud 未下载占位 {}（仅云端，本地不占空间，不计入合计）\x1b[0m",
            human(report.dataless)
        );
    }
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

/// `thin plan`：生成清理计划（只读 JSON）。
///
/// 这是给 agent / 脚本的两阶段契约的「第一阶段」：agent 先拿到计划，审阅/决策后
/// 再用 `thin apply --plan` 原样执行。计划中的项会带在 `approved` 里，
/// 执行时会再次过同一安全门，所以计划过期也不会误删。
fn cmd_plan(args: PlanArgs) -> Result<()> {
    let catalog = rules::load()?;
    let root = args.root.as_deref().map(expand_root);
    let items = scan::scan_scoped(&catalog, true, 1_048_576, root.as_deref());
    let selected = select_scoped(
        items,
        args.preset.as_deref(),
        &args.ids,
        args.all,
        args.root.as_deref(),
    )?;
    let plan = clean::plan(&selected);
    let out = serde_json::json!({
        "version": 1,
        "preset": args.preset,
        "root": args.root,
        "approvedBytes": plan.approved_bytes(),
        "approved": &plan.approved,
        "skipped": &plan.skipped,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// `thin apply --plan <file|->`：执行已审阅的计划。
fn cmd_apply(args: ApplyArgs) -> Result<()> {
    let raw = if args.plan == "-" {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        std::fs::read_to_string(&args.plan)
            .with_context(|| format!("读取计划失败: {}", args.plan))?
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).context("计划不是合法 JSON（应由 thin plan 生成）")?;
    let approved = parsed
        .get("approved")
        .cloned()
        .ok_or_else(|| anyhow!("计划缺少 approved 字段"))?;
    let items: Vec<CleanItem> =
        serde_json::from_value(approved).context("计划 approved 字段解析失败")?;
    if items.is_empty() {
        println!("计划中没有可执行项。");
        return Ok(());
    }

    // 计划可能已过期：用当前安全门重新校验，只执行仍能通过的项
    let plan = clean::plan(&items);
    if plan.approved.is_empty() {
        println!("计划中的项已全部无法通过当前安全门：");
        for s in &plan.skipped {
            println!("  - {}：{}", shorten(&s.path), s.reason);
        }
        return Ok(());
    }

    let mode = if args.quarantine {
        clean::Mode::Quarantine
    } else {
        clean::default_mode()
    };
    if !args.yes
        && !confirm(&format!(
            "执行计划：将 {} 项移入{}？",
            plan.approved.len(),
            mode.label()
        ))?
    {
        println!("已取消。");
        return Ok(());
    }

    let applied = clean::apply(&items, mode)?;
    match &applied {
        clean::Applied::Trash(report) => {
            print_trash_report(report);
            record_history_trash(None, items.len(), plan.approved.len(), report);
        }
        clean::Applied::Quarantine(journal) => {
            print_journal(journal);
            record_history(
                "apply",
                None,
                items.len(),
                plan.approved.len(),
                (0, 0),
                Some(journal),
            );
        }
    }
    warn_snapshots();
    Ok(())
}

fn cmd_protect(args: ProtectArgs) -> Result<()> {
    match args.cmd {
        ProtectCmd::List => {
            let list = protect::load();
            if list.is_empty() {
                println!("保护名单为空。");
                println!(
                    "提示: 在项目根执行 `thin protect add .`，可保护该项目（含 target/）不被 clean 清理。"
                );
                return Ok(());
            }
            println!("保护名单（共 {} 项，含各自子目录）:\n", list.len());
            for p in &list {
                println!("  {}", p.display());
            }
            println!("\n这些路径及其子目录不会被 clean 清理，也不计入可回收。");
        }
        ProtectCmd::Add { path } => {
            let p = fsutil::expand(&path).ok_or_else(|| anyhow!("无法展开路径 {path}"))?;
            let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
            let existing = protect::load();
            if let Some(cover) = protect::covering(&existing, &p) {
                if *cover == canon {
                    println!("\x1b[1m{}\x1b[0m 已在保护名单中。", canon.display());
                } else {
                    println!(
                        "{} 已被父目录 {} 覆盖，无需重复添加。",
                        canon.display(),
                        cover.display()
                    );
                }
                return Ok(());
            }
            let canon = protect::add(&p)?;
            println!("已保护 \x1b[1m{}\x1b[0m（含其所有子目录）", canon.display());
            println!(
                "查看: thin protect list    移除: thin protect remove \"{}\"",
                canon.display()
            );
        }
        ProtectCmd::Remove { path } => {
            let p = fsutil::expand(&path).ok_or_else(|| anyhow!("无法展开路径 {path}"))?;
            if protect::remove(&p)? {
                let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                println!("已移除保护 {}", canon.display());
                if let Some(cover) = protect::covering_in(&clean::thin_home(), &canon) {
                    println!(
                        "\x1b[33m注意\x1b[0m：{} 仍受 {} 覆盖，实际仍不会被清理。",
                        canon.display(),
                        cover.display()
                    );
                }
            } else {
                println!("未在保护名单中找到 {}", p.display());
            }
        }
    }
    Ok(())
}

/// 解析清理方式：`--quarantine` 显式选隔离区；`--trash` 显式选废纸篓；
/// 都不给时用默认（废纸篓可用则废纸篓，否则回退隔离区）。
fn resolve_mode(quarantine: bool, explicit_trash: bool) -> clean::Mode {
    if quarantine {
        clean::Mode::Quarantine
    } else if explicit_trash {
        clean::Mode::Trash
    } else {
        clean::default_mode()
    }
}

fn cmd_clean(args: CleanArgs) -> Result<()> {
    let selected = select_items(&args)?;
    // 预演与执行共用同一安全门，保证「预览即所得」
    let plan = clean::plan(&selected);
    let apply = args.apply && !args.dry_run;
    let mode = resolve_mode(args.quarantine, args.trash);

    // JSON 模式：供 agent 直接消费；dry-run 输出计划，--apply 输出账本
    if args.json {
        if apply {
            if !args.yes {
                return Err(anyhow!("--json --apply 会直接执行，请同时加 --yes"));
            }
            if plan.approved.is_empty() {
                let out = serde_json::json!({
                    "session": serde_json::Value::Null,
                    "approvedBytes": 0,
                    "approved": &plan.approved,
                    "skipped": &plan.skipped,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            let applied = clean::apply(&selected, mode)?;
            match &applied {
                clean::Applied::Trash(report) => {
                    record_history_trash(
                        args.preset.as_deref(),
                        selected.len(),
                        plan.approved.len(),
                        report,
                    );
                    let out = serde_json::json!({
                        "mode": "trash",
                        "trashed": report.trashed,
                        "contentsOnly": report.contents_only,
                        "trashedBytes": report.trashed_bytes,
                        "failed": report.failed,
                        "skipped": report.skipped,
                    });
                    println!("{}", serde_json::to_string_pretty(&out)?);
                }
                clean::Applied::Quarantine(journal) => {
                    record_history(
                        "manual",
                        args.preset.as_deref(),
                        selected.len(),
                        plan.approved.len(),
                        (0, 0),
                        Some(journal),
                    );
                    println!("{}", serde_json::to_string_pretty(journal)?);
                }
            }
            return Ok(());
        }
        let protected_items: Vec<_> = selected.iter().filter(|i| i.protected).cloned().collect();
        let protected_bytes: u64 = scan::top_level(&protected_items)
            .iter()
            .map(|i| i.size)
            .sum();
        let out = serde_json::json!({
            "selectedCount": selected.len(),
            "approvedBytes": plan.approved_bytes(),
            "protectedBytes": protected_bytes,
            "approved": &plan.approved,
            "skipped": &plan.skipped,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if selected.is_empty() {
        println!("没有符合条件的清理项。");
        return Ok(());
    }

    // 默认把「需 sudo / 受系统保护」的项从预览里整个藏起来：它们本就不会被执行。
    // --manual 才展示；真正执行仍走完整 selected（安全门逐项把关）。
    let shown_selected: Vec<thin_core::CleanItem> = if args.manual {
        selected.clone()
    } else {
        selected
            .iter()
            .filter(|it| !scan::is_manual(it))
            .cloned()
            .collect()
    };
    let shown_plan = clean::plan(&shown_selected);

    if shown_plan.approved.is_empty() {
        if !shown_plan.skipped.is_empty() {
            println!("没有可通过安全门的清理项：");
            for s in &shown_plan.skipped {
                println!("  - {}：{}", shorten(&s.path), s.reason);
            }
        } else {
            println!("没有可通过安全门的清理项。");
        }
        return Ok(());
    }

    if !apply {
        print_plan(&shown_plan, args.tree, mode);
        match mode {
            clean::Mode::Trash => {
                println!("\n（dry-run，未执行任何操作。加 --apply 移入系统废纸篓）")
            }
            clean::Mode::Quarantine => {
                println!("\n（dry-run，未执行任何操作。加 --apply 移入隔离区，可恢复）")
            }
        }
        return Ok(());
    }

    if !args.yes
        && !confirm(&format!(
            "将 {} 项移入{}？",
            plan.approved.len(),
            mode.label()
        ))?
    {
        println!("已取消。");
        return Ok(());
    }

    // 传原始候选项：apply 内部复用同一安全门，并如实记录被跳过项
    let applied = clean::apply(&selected, mode)?;
    match &applied {
        clean::Applied::Trash(report) => {
            print_trash_report(report);
            if !report.skipped.is_empty() {
                println!("  安全门跳过 {} 项", report.skipped.len());
            }
            println!("\x1b[90m可在 Finder 废纸篓中恢复。\x1b[0m");
            record_history_trash(
                args.preset.as_deref(),
                selected.len(),
                plan.approved.len(),
                report,
            );
        }
        clean::Applied::Quarantine(journal) => {
            print_journal(journal);
            record_history(
                "manual",
                args.preset.as_deref(),
                selected.len(),
                plan.approved.len(),
                (0, 0),
                Some(journal),
            );
        }
    }
    warn_snapshots();
    Ok(())
}

/// 存在本地 APFS 快照时提醒：即使 purge，空间也可能不会立即释放
fn warn_snapshots() {
    if !probe::local_snapshots().is_empty() {
        println!("\x1b[33m注意: 存在本地 APFS 快照，purge 后空间也可能不会立即释放。\x1b[0m");
    }
}

fn cmd_quarantine(args: QuarantineArgs) -> Result<()> {
    match args.cmd {
        QuarantineCmd::List => {
            let list = clean::list_journals()?;
            if list.is_empty() {
                println!("隔离区为空。");
                return Ok(());
            }
            println!("数据目录: {}\n", clean::thin_home().display());
            for j in list {
                println!(
                    "\x1b[1m会话 {}\x1b[0m  共 {} 项  {}",
                    j.session,
                    j.entries.len(),
                    human(j.total_size())
                );
                for e in &j.entries {
                    println!("   - {:>10}  {}", human(e.size), e.original.display());
                }
                for s in &j.skipped {
                    println!(
                        "   \x1b[33m!\x1b[0m 跳过 {}：{}",
                        s.path.display(),
                        s.reason
                    );
                }
                println!();
            }
        }
        QuarantineCmd::Restore(r) => {
            let sessions: Vec<String> = if r.all {
                clean::list_journals()?
                    .into_iter()
                    .map(|j| j.session)
                    .collect()
            } else if let Some(s) = r.session {
                vec![s]
            } else {
                clean::list_journals()?
                    .into_iter()
                    .take(1)
                    .map(|j| j.session)
                    .collect()
            };
            if sessions.is_empty() {
                println!("没有可恢复的会话。");
                return Ok(());
            }
            for s in sessions {
                let rep = clean::restore_session(&s)?;
                println!("恢复 {}：{} 项", s, rep.restored);
                for m in rep.missing {
                    println!("  \x1b[33m缺失\x1b[0m: {}", m.display());
                }
                for c in rep.conflicts {
                    println!(
                        "  \x1b[33m冲突\x1b[0m（原位置已存在，仍留在隔离区）: {}",
                        c.display()
                    );
                }
            }
        }
        QuarantineCmd::Purge(p) => {
            let targets: Vec<clean::Journal> = if !p.older_than.is_empty() {
                clean::sessions_older_than(parse_days_arg(&p.older_than)?)?
            } else if p.all {
                clean::list_journals()?
            } else if let Some(s) = &p.session {
                clean::list_journals()?
                    .into_iter()
                    .filter(|j| &j.session == s)
                    .collect()
            } else {
                println!("请指定会话、--all 或 --older-than 7d。");
                return Ok(());
            };

            if targets.is_empty() {
                println!("没有匹配的隔离会话。");
                return Ok(());
            }

            let total: u64 = targets.iter().map(|j| j.total_size()).sum();
            println!(
                "将永久删除 {} 个会话，释放 {}（不可恢复）:",
                targets.len(),
                human(total)
            );
            for j in &targets {
                println!(
                    "  - {}  {} 项  {}",
                    j.session,
                    j.entries.len(),
                    human(j.total_size())
                );
            }

            if p.dry_run {
                println!("\n（dry-run，未执行任何删除）");
                return Ok(());
            }
            if !p.yes
                && !confirm(&format!(
                    "永久删除以上 {} 个会话？此操作不可恢复",
                    targets.len()
                ))?
            {
                println!("已取消。");
                return Ok(());
            }

            let (mut n, mut freed) = (0usize, 0u64);
            for j in targets {
                freed += clean::purge_session(&j.session)?;
                n += 1;
            }
            println!("永久删除 {n} 个会话，释放 {}", human(freed));
        }
    }
    Ok(())
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

fn cmd_large(args: LargeArgs) -> Result<()> {
    let root = expand_root(&args.root);
    let min = parse_size_arg(&args.min)?;
    eprintln!("扫描大文件…");
    let files = finder::find_large(&[root], min, args.limit);
    let protect_list = protect::load();
    let is_protected = |p: &PathBuf| protect::matches(&protect_list, p);
    if args.json {
        let out: Vec<_> = files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "size": f.size,
                    "protected": is_protected(&f.path),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "min": min, "files": out }))?
        );
        return Ok(());
    }
    if files.is_empty() {
        println!("未发现 >= {} 的文件。", human(min));
        return Ok(());
    }
    println!("{:>10}  文件", "大小");
    println!("{}", "-".repeat(80));
    for f in &files {
        let tag = if is_protected(&f.path) {
            "  \x1b[90m[已保护]\x1b[0m"
        } else {
            ""
        };
        println!("{:>10}  {}{}", human(f.size), shorten(&f.path), tag);
    }
    Ok(())
}

fn cmd_dupes(args: DupesArgs) -> Result<()> {
    let root = expand_root(&args.root);
    let min = parse_size_arg(&args.min)?;
    eprintln!("扫描重复文件（需读取内容，可能较慢）…");
    let groups = finder::find_duplicates(&[root], min, args.limit);
    if groups.is_empty() {
        println!("未发现重复文件。");
        return Ok(());
    }
    // 构建「将被移走」的候选（每组保留首个，其余为副本），并与 clean 共用安全门，
    // 避免出现「报告可省 X，实际一个都移不走」的口径矛盾。
    let protect_list = protect::load();
    let mut items: Vec<CleanItem> = Vec::new();
    for g in &groups {
        for p in g.paths.iter().skip(1) {
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(g.size);
            let mut it =
                CleanItem::synthetic(p.clone(), size, "dupes", "重复文件副本", Risk::Confirm);
            it.protected = protect::matches(&protect_list, p);
            items.push(it);
        }
    }
    let plan = clean::plan(&items);

    if args.json {
        if args.apply {
            return Err(anyhow!(
                "--json 只用于只读报告；请先用 --json 预览，再用 --apply 执行"
            ));
        }
        let groups_json: Vec<_> = groups
            .iter()
            .map(|g| {
                let removable = g
                    .paths
                    .iter()
                    .skip(1)
                    .filter(|p| plan.approved.iter().any(|it| &it.path == *p))
                    .count() as u64;
                let protected: Vec<&PathBuf> = g
                    .paths
                    .iter()
                    .skip(1)
                    .filter(|p| protect::matches(&protect_list, p))
                    .collect();
                serde_json::json!({
                    "size": g.size,
                    "paths": g.paths,
                    "reclaimableBytes": g.size.saturating_mul(removable),
                    "protected": protected,
                })
            })
            .collect();
        let out = serde_json::json!({
            "groups": groups_json,
            "reclaimableBytes": plan.approved_bytes(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let mut total = 0u64;
    for (i, g) in groups.iter().enumerate() {
        let removable = g
            .paths
            .iter()
            .skip(1)
            .filter(|p| plan.approved.iter().any(|it| &it.path == *p))
            .count() as u64;
        let wasted = g.size.saturating_mul(removable);
        total += wasted;
        println!(
            "\x1b[1m组 {} · {} × {}  （可省 {}）\x1b[0m",
            i + 1,
            g.paths.len(),
            human(g.size),
            human(wasted)
        );
        for (j, p) in g.paths.iter().enumerate() {
            let tag = if j > 0 && protect::matches(&protect_list, p) {
                "  \x1b[90m[已保护]\x1b[0m"
            } else {
                ""
            };
            println!("   {}{}", shorten(p), tag);
        }
    }
    println!("\n共 {} 组，可回收 {}", groups.len(), human(total));

    let mode = resolve_mode(args.quarantine, false);
    if !args.apply {
        println!(
            "（只读报告；加 --apply 可把每组除首个外的副本移入{}）",
            mode.label()
        );
        return Ok(());
    }

    if plan.approved.is_empty() {
        println!("没有可通过安全门的清理项：");
        for s in &plan.skipped {
            println!("  - {}：{}", shorten(&s.path), s.reason);
        }
        return Ok(());
    }
    if !args.yes
        && !confirm(&format!(
            "将 {} 个重复副本移入{}？",
            plan.approved.len(),
            mode.label()
        ))?
    {
        println!("已取消。");
        return Ok(());
    }
    let applied = clean::apply(&items, mode)?;
    match &applied {
        clean::Applied::Trash(report) => {
            print_trash_report(report);
            record_history_trash(None, items.len(), plan.approved.len(), report);
        }
        clean::Applied::Quarantine(journal) => {
            print_journal(journal);
            record_history(
                "dupes",
                None,
                items.len(),
                plan.approved.len(),
                (0, 0),
                Some(journal),
            );
        }
    }
    warn_snapshots();
    Ok(())
}

fn cmd_apps(args: AppsArgs) -> Result<()> {
    let min = parse_size_arg(&args.min)?;
    let apps = apps::list_apps();
    println!(
        "{:<6} {:>10}  {:<12} {:<8} {:<28} Bundle ID",
        "等级", "总占用", "关联残留", "状态", "App"
    );
    println!("{}", "-".repeat(104));
    for a in apps.into_iter().filter(|a| a.total() >= min) {
        let t = apps::tier(a.total());
        println!(
            "  {}{}\x1b[0m   {:>10}  {:<12} {:<8} {:<28} {}",
            tier_ansi(t),
            t.label(),
            human(a.total()),
            human(a.leftovers_size()),
            if a.running { "● 运行中" } else { "" },
            truncate(&a.name, 28),
            a.bundle_id.unwrap_or_default()
        );
    }
    Ok(())
}

fn cmd_uninstall(args: UninstallArgs) -> Result<()> {
    let matched = apps::find_app(&args.query);
    if matched.is_empty() {
        println!("未找到匹配的 App: {}", args.query);
        return Ok(());
    }
    if matched.len() > 1 {
        println!("匹配到多个 App，请输入更精确的名称：");
        for a in &matched {
            println!("  {}  ({})", a.name, human(a.total()));
        }
        return Ok(());
    }
    let app = &matched[0];

    // 系统关键 App 保护（读不到 bundle id 时也拒绝）
    if apps::is_system_protected(app.bundle_id.as_deref()) {
        println!(
            "\x1b[31m已取消：{} 是系统关键 App，禁止卸载。\x1b[0m",
            app.name
        );
        return Ok(());
    }

    let mut items: Vec<CleanItem> = vec![CleanItem::synthetic(
        app.path.clone(),
        app.size,
        "app",
        &app.name,
        Risk::Confirm,
    )];
    for l in &app.leftovers {
        let mut it = CleanItem::synthetic(
            l.path.clone(),
            l.size,
            "app-leftover",
            &format!("{} 残留", app.name),
            Risk::Confirm,
        );
        // 系统级残留（/Library 等）需 root，交给安全门跳过并提示手动处理
        it.sudo = l.sudo;
        items.push(it);
    }

    println!("\n\x1b[1m卸载计划: {}\x1b[0m\n", app.name);
    for it in &items {
        let tag = if it.sudo {
            "  \x1b[33m需 sudo\x1b[0m"
        } else {
            ""
        };
        println!("• {:<44} {:>10}{}", shorten(&it.path), human(it.size), tag);
    }
    // 与 clean 共用安全门：预览/计数即实际能移动的项
    let plan = clean::plan(&items);
    println!(
        "\n共 {} 项，可自动释放 \x1b[1m{}\x1b[0m{}",
        items.len(),
        human(plan.approved_bytes()),
        if plan.skipped.is_empty() {
            String::new()
        } else {
            format!("（另有 {} 项无法自动处理）", plan.skipped.len())
        }
    );
    for s in &plan.skipped {
        println!("  \x1b[33m跳过\x1b[0m {}：{}", shorten(&s.path), s.reason);
    }

    // 安装包记录（informational）
    let pkgs = apps::pkg_receipt_ids(app.bundle_id.as_deref(), &app.name);
    if !pkgs.is_empty() {
        println!("\n\x1b[1m安装包记录\x1b[0m（如需彻底清除: sudo pkgutil --forget <id>）");
        for p in &pkgs {
            println!("  {p}");
        }
    }

    let mode = if args.quarantine {
        clean::Mode::Quarantine
    } else {
        clean::default_mode()
    };
    if !args.apply {
        println!("（预览；加 --apply 移入{}，可恢复）", mode.label());
        return Ok(());
    }

    // SafetyGate：运行中的 App 不硬删
    if apps::is_running(&app.path) {
        println!(
            "\x1b[31m已取消：{} 正在运行，请先退出后再卸载。\x1b[0m",
            app.name
        );
        return Ok(());
    }
    if plan.approved.is_empty() {
        println!("没有可通过安全门的项。");
        return Ok(());
    }
    if !args.yes && !confirm(&format!("卸载 {} 并移入{}？", app.name, mode.label()))? {
        println!("已取消。");
        return Ok(());
    }
    let applied = clean::apply(&items, mode)?;
    match &applied {
        clean::Applied::Trash(report) => {
            print_trash_report(report);
            record_history_trash(None, items.len(), plan.approved.len(), report);
        }
        clean::Applied::Quarantine(journal) => {
            print_journal(journal);
            record_history(
                "uninstall",
                None,
                items.len(),
                plan.approved.len(),
                (0, 0),
                Some(journal),
            );
        }
    }
    warn_snapshots();
    Ok(())
}

// ---------------------------------------------------------------------------
// 预设 / 历史 / 定时任务
// ---------------------------------------------------------------------------

fn print_preset_row(p: &preset::Preset, source: &str) {
    let cats = if p.categories.is_empty() {
        "全部".to_string()
    } else {
        p.categories
            .iter()
            .map(|c| c.label())
            .collect::<Vec<_>>()
            .join("/")
    };
    let risks = p
        .risks
        .iter()
        .map(|r| r.label())
        .collect::<Vec<_>>()
        .join("/");
    println!(
        "{:<18} {:<10} {:<6} {:<6} {} [{}]{}",
        p.id,
        truncate(&p.name, 10),
        source,
        format!("{}d", p.purge_after_days),
        cats,
        risks,
        if p.regenerable_only { " 可再生" } else { "" }
    );
}

fn print_preset_detail(p: &preset::Preset) {
    println!("\x1b[1m{}\x1b[0m  ({})", p.name, p.id);
    println!(
        "  类别:     {}",
        if p.categories.is_empty() {
            "全部".to_string()
        } else {
            p.categories
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(" / ")
        }
    );
    println!(
        "  风险:     {}",
        p.risks
            .iter()
            .map(|r| r.label())
            .collect::<Vec<_>>()
            .join(" / ")
    );
    println!(
        "  仅可再生: {}",
        if p.regenerable_only { "是" } else { "否" }
    );
    println!("  保留天数: {}d", p.purge_after_days);
    if !p.include_ids.is_empty() {
        println!("  仅包含:   {}", p.include_ids.join(", "));
    }
    if !p.exclude_ids.is_empty() {
        println!("  排除:     {}", p.exclude_ids.join(", "));
    }
}

fn build_preset(a: PresetAddArgs) -> Result<preset::Preset> {
    let name = a.name.unwrap_or_else(|| a.id.clone());

    // 未指定类别/包含列表 → 默认只处理 cache 类
    if a.categories.is_empty() && a.include.is_empty() {
        let mut p = preset::Preset::new_cache_only(a.id, name);
        p.exclude_ids = a.exclude;
        if let Some(d) = a.purge_after_days {
            p.purge_after_days = d;
        }
        return Ok(p);
    }

    let mut categories = Vec::new();
    for c in &a.categories {
        categories.push(rules::parse_category(c).ok_or_else(|| anyhow!("未知类别: {c}"))?);
    }
    let mut risks = Vec::new();
    for r in &a.risks {
        risks.push(rules::parse_risk(r).ok_or_else(|| anyhow!("未知风险: {r}"))?);
    }
    if risks.is_empty() {
        risks.push(Risk::Safe);
    }
    Ok(preset::Preset {
        id: a.id,
        name,
        categories,
        risks,
        regenerable_only: a.regenerable_only,
        include_ids: a.include,
        exclude_ids: a.exclude,
        purge_after_days: a.purge_after_days.unwrap_or(7),
    })
}

fn cmd_preset(args: PresetArgs) -> Result<()> {
    match args.cmd {
        None | Some(PresetCmd::List) => {
            println!(
                "{:<18} {:<10} {:<6} {:<6} 类别 [风险]",
                "id", "名称", "来源", "保留"
            );
            println!("{}", "-".repeat(84));
            print_preset_row(&preset::Preset::builtin_default(), "内置");
            print_preset_row(&preset::Preset::builtin_dev(), "内置");
            for p in preset::load()? {
                print_preset_row(&p, "用户");
            }
            println!("\n预设文件: {}", preset::presets_path().display());
            println!("定时任务只能执行【用户】预设: thin schedule install --preset <id> --weekly");
        }
        Some(PresetCmd::Show { id }) => {
            let p = preset::get(&id).ok_or_else(|| anyhow!("未找到预设 {id}"))?;
            print_preset_detail(&p);
        }
        Some(PresetCmd::Add(a)) => {
            let p = build_preset(a)?;
            let saved = preset::upsert(p.clone())?;
            println!("已写入预设 \x1b[1m{}\x1b[0m → {}", p.id, saved.display());
            print_preset_detail(&p);
        }
        Some(PresetCmd::Remove { id }) => {
            if preset::remove(&id)? {
                println!("已删除预设 {id}");
            } else {
                println!("未找到用户预设 {id}（内置 default 不可删除）");
            }
        }
    }
    Ok(())
}

fn cmd_history(args: HistoryArgs) -> Result<()> {
    if args.reconcile {
        let n = history::reconcile_with_quarantine()?;
        if args.json {
            println!("{}", serde_json::json!({ "reconciled": n }));
        } else {
            println!("已从隔离区回填 {n} 条历史记录。");
        }
        return Ok(());
    }
    let list = history::load(Some(args.limit))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("暂无历史记录。");
    } else {
        println!(
            "{:<20} {:<10} {:<16} {:>6} {:>10} {:>6} {:>10}",
            "时间", "触发", "预设", "项", "释放", "跳过", "purged"
        );
        println!("{}", "-".repeat(88));
        for r in list {
            println!(
                "{:<20} {:<10} {:<16} {:>6} {:>10} {:>6} {:>10}",
                history::format_ts(r.timestamp),
                r.trigger,
                r.preset.unwrap_or_else(|| "-".into()),
                r.moved,
                human(r.moved_bytes),
                r.skipped,
                human(r.purged_bytes)
            );
        }
    }
    if let Ok(n) = history::unreconciled_count_in(&clean::thin_home())
        && n > 0
    {
        println!(
            "\n\x1b[90m另有 {n} 个隔离会话未记录，可用 `thin history --reconcile` 回填。\x1b[0m"
        );
    }
    Ok(())
}

fn build_schedule(a: &ScheduleInstallArgs) -> Result<schedule::Schedule> {
    if let Some(secs) = a.interval {
        if secs == 0 {
            return Err(anyhow!("--interval 必须大于 0"));
        }
        return Ok(schedule::Schedule::Interval { seconds: secs });
    }
    if a.hour > 23 || a.minute > 59 {
        return Err(anyhow!("时间非法: {}:{}", a.hour, a.minute));
    }
    if a.weekly {
        Ok(schedule::Schedule::Weekly {
            weekday: a.weekday,
            hour: a.hour,
            minute: a.minute,
        })
    } else {
        Ok(schedule::Schedule::Daily {
            hour: a.hour,
            minute: a.minute,
        })
    }
}

fn describe_schedule(s: &schedule::Schedule) -> String {
    match s {
        schedule::Schedule::Daily { hour, minute } => format!("每天 {hour:02}:{minute:02}"),
        schedule::Schedule::Weekly {
            weekday,
            hour,
            minute,
        } => {
            let names = ["日", "一", "二", "三", "四", "五", "六", "日"];
            let w = names.get(*weekday as usize).copied().unwrap_or("?");
            format!("每周{w} {hour:02}:{minute:02}")
        }
        schedule::Schedule::Interval { seconds } => format!("每 {seconds}s"),
    }
}

fn extract_preset(plist: &str) -> Option<String> {
    let marker = "<string>--preset</string>";
    let idx = plist.find(marker)? + marker.len();
    let rest = &plist[idx..];
    let s = rest.find("<string>")? + "<string>".len();
    let rest = &rest[s..];
    let e = rest.find("</string>")?;
    Some(rest[..e].to_string())
}

fn cmd_schedule(args: ScheduleArgs) -> Result<()> {
    match args.cmd {
        ScheduleCmd::Install(a) => {
            if !preset::is_user_defined(&a.preset) {
                return Err(anyhow!(
                    "预设 '{}' 不是用户自定义预设；定时任务只执行用户自定义预设。\n请先创建同名用户预设（默认只选缓存类、仅 safe、可再生）:\n  thin preset add {}",
                    a.preset,
                    a.preset
                ));
            }
            let sch = build_schedule(&a)?;
            let bin = std::env::current_exe().context("无法确定 thin 可执行文件路径")?;
            if a.dry_run {
                println!("{}", schedule::render_plist(&bin, &a.preset, &sch));
                println!("# 将写入: {}", schedule::plist_path().display());
                return Ok(());
            }
            let path = schedule::install(&bin, &a.preset, &sch)?;
            println!("已安装定时任务: {}", path.display());
            println!("预设: {}  |  周期: {}", a.preset, describe_schedule(&sch));
            println!(
                "\n\x1b[33m注意\x1b[0m: 需在 系统设置 → 隐私与安全 → 完全磁盘访问权限 中把\n  {}\n加入，否则读不到受保护目录。",
                bin.display()
            );
            println!("日志: ~/.thin/schedule.log / schedule.err");
            println!("查看: thin schedule status    卸载: thin schedule uninstall");
        }
        ScheduleCmd::Uninstall => {
            if schedule::uninstall()? {
                println!("已卸载定时任务。");
            } else {
                println!("未安装定时任务。");
            }
        }
        ScheduleCmd::Status => {
            let path = schedule::plist_path();
            let exists = path.exists();
            println!(
                "plist:  {} ({})",
                path.display(),
                if exists { "存在" } else { "不存在" }
            );
            println!(
                "已加载: {}",
                if schedule::is_loaded() { "是" } else { "否" }
            );
            if exists
                && let Ok(s) = std::fs::read_to_string(&path)
                && let Some(p) = extract_preset(&s)
            {
                println!("预设:   {p}");
            }
        }
        ScheduleCmd::Run {
            preset: pid,
            dry_run,
        } => run_preset(&pid, "schedule", dry_run)?,
    }
    Ok(())
}

/// 按预设执行一次清理（先回收旧会话，再隔离新项），并写入历史。
/// `dry_run=true` 时只打印清理计划，不 purge、不隔离、不写历史。
fn run_preset(preset_id: &str, trigger: &str, dry_run: bool) -> Result<()> {
    let p = preset::get(preset_id).ok_or_else(|| anyhow!("未找到预设 {preset_id}"))?;

    // 1) 先永久删除早于保留期的旧会话（否则隔离不释放空间）
    let (purged_sessions, purged_bytes) = if dry_run {
        (0, 0)
    } else {
        clean::purge_older_than(p.purge_after_days).unwrap_or((0, 0))
    };
    if purged_sessions > 0 {
        println!(
            "已永久删除 {purged_sessions} 个旧会话，释放 {}",
            human(purged_bytes)
        );
    }

    // 2) 扫描 + 预设筛选 + 安全门
    let catalog = rules::load()?;
    let items = scan::scan(&catalog, true, 1_048_576);
    let selected = preset::select(&p, &items);
    let plan = clean::plan(&selected);
    println!(
        "预设 {}：候选 {}，通过安全门 {}",
        p.id,
        selected.len(),
        plan.approved.len()
    );

    if plan.approved.is_empty() {
        println!("没有可清理项。");
        if !dry_run {
            record_history(
                trigger,
                Some(&p.id),
                selected.len(),
                0,
                (purged_sessions, purged_bytes),
                None,
            );
        }
        return Ok(());
    }

    if dry_run {
        print_plan(&plan, false, clean::default_mode());
        println!("\n（dry-run，未 purge / 未隔离 / 未写历史。去掉 --dry-run 即执行）");
        return Ok(());
    }

    // 3) 执行清理（默认系统废纸篓）
    let applied = clean::apply(&selected, clean::default_mode())?;
    match &applied {
        clean::Applied::Trash(report) => {
            print_trash_report(report);
            record_history_trash(Some(&p.id), selected.len(), plan.approved.len(), report);
        }
        clean::Applied::Quarantine(journal) => {
            print_journal(journal);
            record_history(
                trigger,
                Some(&p.id),
                selected.len(),
                plan.approved.len(),
                (purged_sessions, purged_bytes),
                Some(journal),
            );
        }
    }
    Ok(())
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
