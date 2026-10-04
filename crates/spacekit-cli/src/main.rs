mod report;
mod top;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use spacekit_core::fmt::human;
use spacekit_core::model::Risk;
use spacekit_core::{clean, fsutil, probe, rules, scan};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "spacekit",
    about = "macOS 系统空间扫描与安全清理 (M1 · 隔离区可恢复)",
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
        "\n共 {} 条规则。可用 SPACEKIT_RULES=/path/to.json 覆盖。",
        catalog.len()
    );
    Ok(())
}

/// 按参数筛选清理项
fn select_items(args: &CleanArgs) -> Result<Vec<spacekit_core::CleanItem>> {
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

fn print_plan(selected: &[spacekit_core::CleanItem]) {
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
    if journal.entries.is_empty() {
        println!("\n没有可执行项（全部被安全门跳过）:");
        for s in &journal.skipped {
            println!("  \x1b[33m跳过\x1b[0m {}：{}", s.path.display(), s.reason);
        }
        return Ok(());
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
    println!(
        "\n恢复:       spacekit quarantine restore {}",
        journal.session
    );
    println!("永久删除:   spacekit quarantine purge {}", journal.session);
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
            println!("数据目录: {}\n", clean::spacekit_home().display());
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
