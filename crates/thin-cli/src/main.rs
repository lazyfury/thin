mod report;
mod top;
mod treemap;
mod tui;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use thin_core::fmt::human;
use thin_core::model::{Category, Risk};
use thin_core::{CleanItem, apps, clean, discover, finder, fsutil, probe, rules, scan};

#[derive(Parser)]
#[command(
    name = "thin",
    about = "macOS 系统空间扫描与安全清理 (M4 · 多标签 TUI)",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 磁盘概览：容量、卷、快照、外接盘
    Probe,

    /// 扫描已知可清理项并估算可回收空间
    Scan(ScanArgs),

    /// 列出某目录下最大的子项（类似 du -sh PATH/* | sort -rh）
    Top(TopArgs),

    /// 交互式 TUI（浏览、勾选、移入隔离区）
    Tui(TuiArgs),

    /// 规则：列出 / 新增（agent 入口）/ 删除
    Rules(RulesArgs),

    /// 归因：找出未被规则覆盖的大目录
    Discover(DiscoverArgs),

    /// 清理：默认 dry-run 预览；--apply 移入隔离区（可恢复）
    Clean(CleanArgs),

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

    /// 显示指定规则的详细说明（可多次）
    #[arg(long = "detail")]
    detail: Vec<String>,
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
    /// 把每组除首个外的副本移入隔离区
    #[arg(long)]
    apply: bool,
    /// 跳过确认
    #[arg(long)]
    yes: bool,
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
}

#[derive(clap::Args)]
struct RulesArgs {
    #[command(subcommand)]
    cmd: Option<RulesCmd>,
}

#[derive(Subcommand)]
enum RulesCmd {
    /// 列出所有规则（默认）
    List,
    /// 显示用户规则文件路径
    Path,
    /// 新增/覆盖一条规则（agent 入口）
    Add(RuleAddArgs),
    /// 删除一条用户规则
    Remove(RuleRemoveArgs),
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Probe => probe::probe_summary()?,
        Cmd::Scan(args) => cmd_scan(args)?,
        Cmd::Top(args) => {
            let path = fsutil::expand(&args.path).unwrap_or_else(|| PathBuf::from(&args.path));
            top::run(path, args.limit);
        }
        Cmd::Tui(args) => cmd_tui(args)?,
        Cmd::Rules(args) => cmd_rules(args)?,
        Cmd::Discover(args) => cmd_discover(args)?,
        Cmd::Clean(args) => cmd_clean(args)?,
        Cmd::Quarantine(args) => cmd_quarantine(args)?,
        Cmd::Large(args) => cmd_large(args)?,
        Cmd::Dupes(args) => cmd_dupes(args)?,
        Cmd::Apps(args) => cmd_apps(args)?,
        Cmd::Uninstall(args) => cmd_uninstall(args)?,
    }
    Ok(())
}

fn cmd_scan(args: ScanArgs) -> Result<()> {
    let min = parse_size(&args.min).unwrap_or(1_048_576);
    let catalog = rules::load()?;
    let items = scan::scan(&catalog, args.all, min);
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

    report::print_table(&items);
    report::print_summary(&items);
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
    let min = parse_size(&args.min).unwrap_or(1_048_576);
    let root = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    tui::run(root, min)?;
    Ok(())
}

fn cmd_rules(args: RulesArgs) -> Result<()> {
    match args.cmd {
        None | Some(RulesCmd::List) => list_rules()?,
        Some(RulesCmd::Path) => println!("{}", rules::user_rules_path().display()),
        Some(RulesCmd::Add(a)) => cmd_rule_add(a)?,
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
    println!(
        "{:>8}  {:<10} {:<12} {}",
        "风险", "类别", "可再生", "名称 / 规则 id"
    );
    println!("{}", "-".repeat(80));
    for r in &catalog {
        println!(
            "  {:<6} {:<10} {:<12} {}  \x1b[90m({})\x1b[0m",
            r.risk.label(),
            r.category.label(),
            if r.regenerable { "是" } else { "否" },
            r.name,
            r.id
        );
    }
    println!(
        "\n共 {} 条规则。用户规则文件: {}",
        catalog.len(),
        rules::user_rules_path().display()
    );
    Ok(())
}

fn cmd_rule_add(args: RuleAddArgs) -> Result<()> {
    let rule = if let Some(spec) = &args.json {
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
        let reclaim = if args.reclaim.is_empty() {
            "移入隔离区".to_string()
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
            args.cost.unwrap_or_else(|| "移入隔离区，可恢复".into()),
            args.recover
                .unwrap_or_else(|| "thin quarantine restore".into()),
        )
    };

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
    let min = parse_size(&args.min).unwrap_or(500 * 1024 * 1024);
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

    println!("{:>10}  {:<20} {}", "大小", "归因", "路径");
    println!("{}", "-".repeat(90));
    for f in &report.findings {
        let tag = if f.coverage.is_uncovered() {
            format!("\x1b[33m{}\x1b[0m", f.coverage.label())
        } else {
            f.coverage.label()
        };
        println!("{:>10}  {:<20} {}", human(f.size), tag, shorten(&f.path));
    }

    let none: Vec<_> = report
        .findings
        .iter()
        .filter(|f| matches!(f.coverage, discover::Coverage::None))
        .collect();
    let partial: Vec<_> = report
        .findings
        .iter()
        .filter(|f| matches!(f.coverage, discover::Coverage::Partial(_)))
        .collect();
    if !none.is_empty() {
        println!("\n\x1b[1m未归类大目录 —— 可用 thin rule add 补规则:\x1b[0m");
        for f in none {
            let p = f.path.display().to_string();
            println!(
                "  thin rule add --path \"{}\" --id {}",
                p,
                rules::slug_for(&p)
            );
        }
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
    Ok(())
}

/// 按参数筛选清理项
fn select_items(args: &CleanArgs) -> Result<Vec<thin_core::CleanItem>> {
    let catalog = rules::load()?;
    let items = scan::scan(&catalog, true, 1_048_576);
    let selected: Vec<thin_core::CleanItem> = items
        .into_iter()
        .filter(|it| {
            if !args.ids.is_empty() {
                return args.ids.contains(&it.rule_id);
            }
            match it.risk {
                Risk::Safe => true,
                Risk::Confirm => args.all,
                Risk::Destructive => false,
            }
        })
        .collect();
    // 只处理最顶层项：避免父目录与其子目录重复计数、重复移动
    Ok(scan::top_level(&selected))
}

fn print_plan(selected: &[thin_core::CleanItem]) {
    println!("\x1b[1m清理计划\x1b[0m");
    println!(
        "\x1b[90m以下为官方推荐的清理方式；thin 统一将目标移入隔离区（可恢复），不会执行这些命令。\x1b[0m\n"
    );
    let mut manual = 0usize;
    for it in selected {
        println!("• {:<28} {:>10}", it.name, human(it.size));
        println!("  {:<28} {}", "官方方式:", it.reclaim);
        println!("  {:<28} {}", "thin 动作:", "移入隔离区（可恢复）");
        if it.sudo {
            manual += 1;
            println!(
                "  {:<28} {}",
                "注意:", "\x1b[33m需要 sudo（将跳过，不计入可释放）\x1b[0m"
            );
        }
        println!("  {:<28} {}", "路径:", it.path.display());
    }
    let total = scan::planned_bytes(selected);
    let note = if manual > 0 {
        format!("（其中 {manual} 项需 sudo 手动处理）")
    } else {
        String::new()
    };
    println!(
        "\n共 {} 项{}，预计可释放 \x1b[1m{}\x1b[0m",
        selected.len(),
        note,
        human(total)
    );
}

fn cmd_clean(args: CleanArgs) -> Result<()> {
    let selected = select_items(&args)?;
    if selected.is_empty() {
        println!("没有符合条件的清理项。");
        return Ok(());
    }

    let apply = args.apply && !args.dry_run;
    if !apply {
        print_plan(&selected);
        println!("\n（dry-run，未执行任何操作。加 --apply 移入隔离区，可恢复）");
        return Ok(());
    }

    if !args.yes && !confirm(&format!("将 {} 项移入隔离区？", selected.len()))? {
        println!("已取消。");
        return Ok(());
    }

    let journal = clean::quarantine(&selected, false)?;
    print_journal(&journal);
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
            if !p.older_than.is_empty() {
                let days = parse_days(&p.older_than).unwrap_or(7);
                let (n, freed) = clean::purge_older_than(days)?;
                println!("永久删除 {n} 个会话，释放 {}", human(freed));
            } else if p.all {
                let list = clean::list_journals()?;
                let (mut n, mut freed) = (0usize, 0u64);
                for j in list {
                    freed += clean::purge_session(&j.session)?;
                    n += 1;
                }
                println!("永久删除 {n} 个会话，释放 {}", human(freed));
            } else if let Some(s) = p.session {
                let freed = clean::purge_session(&s)?;
                println!("永久删除会话 {s}，释放 {}", human(freed));
            } else {
                println!("请指定会话、--all 或 --older-than 7d。");
            }
        }
    }
    Ok(())
}

fn expand_root(s: &str) -> PathBuf {
    fsutil::expand(s).unwrap_or_else(|| PathBuf::from(s))
}

/// 把家目录前缀显示为 ~
fn shorten(path: &PathBuf) -> String {
    let s = path.display().to_string();
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && s.starts_with(&home) {
        s.replacen(&home, "~", 1)
    } else {
        s
    }
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
    let min = parse_size(&args.min).unwrap_or(100 * 1024 * 1024);
    eprintln!("扫描大文件…");
    let files = finder::find_large(&[root], min, args.limit);
    if files.is_empty() {
        println!("未发现 >= {} 的文件。", human(min));
        return Ok(());
    }
    println!("{:>10}  {}", "大小", "文件");
    println!("{}", "-".repeat(80));
    for f in &files {
        println!("{:>10}  {}", human(f.size), shorten(&f.path));
    }
    Ok(())
}

fn cmd_dupes(args: DupesArgs) -> Result<()> {
    let root = expand_root(&args.root);
    let min = parse_size(&args.min).unwrap_or(1024 * 1024);
    eprintln!("扫描重复文件（需读取内容，可能较慢）…");
    let groups = finder::find_duplicates(&[root], min, args.limit);
    if groups.is_empty() {
        println!("未发现重复文件。");
        return Ok(());
    }
    let mut total = 0u64;
    for (i, g) in groups.iter().enumerate() {
        total += g.wasted();
        println!(
            "\x1b[1m组 {} · {} × {}  （可省 {}）\x1b[0m",
            i + 1,
            g.paths.len(),
            human(g.size),
            human(g.wasted())
        );
        for p in &g.paths {
            println!("   {}", shorten(p));
        }
    }
    println!("\n共 {} 组，可回收 {}", groups.len(), human(total));

    if !args.apply {
        println!("（只读报告；加 --apply 可把每组除首个外的副本移入隔离区）");
        return Ok(());
    }

    let mut items: Vec<CleanItem> = Vec::new();
    for g in &groups {
        for p in g.paths.iter().skip(1) {
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(g.size);
            items.push(CleanItem::synthetic(
                p.clone(),
                size,
                "dupes",
                "重复文件副本",
                Risk::Confirm,
            ));
        }
    }
    if items.is_empty() {
        return Ok(());
    }
    if !args.yes && !confirm(&format!("将 {} 个重复副本移入隔离区？", items.len()))? {
        println!("已取消。");
        return Ok(());
    }
    let journal = clean::quarantine(&items, false)?;
    print_journal(&journal);
    warn_snapshots();
    Ok(())
}

fn cmd_apps(args: AppsArgs) -> Result<()> {
    let min = parse_size(&args.min).unwrap_or(0);
    let apps = apps::list_apps();
    println!(
        "{:<6} {:>10}  {:<12} {:<28} {}",
        "等级", "总占用", "关联残留", "App", "Bundle ID"
    );
    println!("{}", "-".repeat(96));
    for a in apps.into_iter().filter(|a| a.total() >= min) {
        let t = apps::tier(a.total());
        println!(
            "  {}{}\x1b[0m   {:>10}  {:<12} {:<28} {}",
            tier_ansi(t),
            t.label(),
            human(a.total()),
            human(a.leftovers_size()),
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
    let mut items: Vec<CleanItem> = vec![CleanItem::synthetic(
        app.path.clone(),
        app.size,
        "app",
        &app.name,
        Risk::Confirm,
    )];
    for (p, s) in &app.leftovers {
        items.push(CleanItem::synthetic(
            p.clone(),
            *s,
            "app-leftover",
            &format!("{} 残留", app.name),
            Risk::Confirm,
        ));
    }

    println!("\n\x1b[1m卸载计划: {}\x1b[0m\n", app.name);
    for it in &items {
        println!("• {:<44} {:>10}", shorten(&it.path), human(it.size));
    }
    println!(
        "\n共 {} 项，预计可释放 \x1b[1m{}\x1b[0m",
        items.len(),
        human(app.total())
    );

    if !args.apply {
        println!("（预览；加 --apply 移入隔离区，可恢复）");
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
    if !args.yes && !confirm(&format!("卸载 {} 并移入隔离区？", app.name))? {
        println!("已取消。");
        return Ok(());
    }
    let journal = clean::quarantine(&items, false)?;
    print_journal(&journal);
    warn_snapshots();
    Ok(())
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
