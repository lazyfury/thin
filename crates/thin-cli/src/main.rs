mod report;
mod top;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use thin_core::fmt::human;
use thin_core::model::Risk;
use thin_core::{CleanItem, apps, clean, finder, fsutil, probe, rules, scan};

#[derive(Parser)]
#[command(
    name = "thin",
    about = "macOS 系统空间扫描与安全清理 (M2 · 隔离区可恢复)",
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

    /// 列出内置规则目录
    Rules,

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
    /// 最小总占用过滤
    #[arg(long, default_value = "100MB")]
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
        Cmd::Rules => cmd_rules()?,
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

    if args.json {
        let out = serde_json::json!({
            "items": items,
            "summary": scan::summarize(&items),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    report::print_table(&items);
    report::print_summary(&items);

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
    let catalog = rules::load()?;
    eprintln!("扫描中…");
    let items = scan::scan(&catalog, true, min);
    tui::run(items)?;
    Ok(())
}

fn cmd_rules() -> Result<()> {
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
        "\n共 {} 条规则。可用 THIN_RULES=/path/to.json 覆盖。",
        catalog.len()
    );
    Ok(())
}

/// 按参数筛选清理项
fn select_items(args: &CleanArgs) -> Result<Vec<thin_core::CleanItem>> {
    let catalog = rules::load()?;
    let items = scan::scan(&catalog, true, 1_048_576);
    Ok(items
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
        .collect())
}

fn print_plan(selected: &[thin_core::CleanItem]) {
    println!("\x1b[1m清理计划\x1b[0m\n");
    let mut total: u64 = 0;
    for it in selected {
        total = total.saturating_add(it.size);
        println!("• {:<28} {:>10}", it.name, human(it.size));
        println!("  {:<28} {}", "方式:", it.reclaim);
        if it.sudo {
            println!("  {:<28} {}", "注意:", "\x1b[33m需要 sudo（将跳过）\x1b[0m");
        }
        println!("  {:<28} {}", "路径:", it.path.display());
    }
    println!(
        "\n共 {} 项，预计释放 \x1b[1m{}\x1b[0m",
        selected.len(),
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
    Ok(())
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
    Ok(())
}

fn cmd_apps(args: AppsArgs) -> Result<()> {
    let min = parse_size(&args.min).unwrap_or(100 * 1024 * 1024);
    let apps = apps::list_apps();
    println!(
        "{:<28} {:>10}  {:<12} {}",
        "App", "总占用", "关联残留", "Bundle ID"
    );
    println!("{}", "-".repeat(92));
    for a in apps.into_iter().filter(|a| a.total() >= min) {
        println!(
            "{:<28} {:>10}  {:<12} {}",
            truncate(&a.name, 28),
            human(a.total()),
            human(a.leftovers_size()),
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

    println!("\x1b[1m卸载计划: {}\x1b[0m\n", app.name);
    for it in &items {
        println!("• {:<44} {:>10}", shorten(&it.path), human(it.size));
    }
    println!(
        "\n共 {} 项，预计释放 \x1b[1m{}\x1b[0m",
        items.len(),
        human(app.total())
    );

    if !args.apply {
        println!("（预览；加 --apply 移入隔离区，可恢复）");
        return Ok(());
    }
    if !args.yes && !confirm(&format!("卸载 {} 并移入隔离区？", app.name))? {
        println!("已取消。");
        return Ok(());
    }
    let journal = clean::quarantine(&items, false)?;
    print_journal(&journal);
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
    let (num, mult) = if let Some(n) = upper.strip_suffix("GB").or(upper.strip_suffix("G")) {
        (n, 1024u64.pow(3))
    } else if let Some(n) = upper.strip_suffix("MB").or(upper.strip_suffix("M")) {
        (n, 1024u64.pow(2))
    } else if let Some(n) = upper.strip_suffix("KB").or(upper.strip_suffix("K")) {
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
