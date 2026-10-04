mod fsutil;
mod model;
mod probe;
mod report;
mod rules;
mod scan;
mod top;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "spacekit",
    about = "macOS 系统空间扫描与安全清理 CLI (M0 · 只读)",
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

    /// 列出内置规则目录
    Rules,

    /// 生成清理计划（M0 仅支持 --dry-run，不会真正删除）
    Clean(CleanArgs),
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
struct CleanArgs {
    /// 仅预览计划（M0 必须）
    #[arg(long)]
    dry_run: bool,

    /// 只处理「安全」项
    #[arg(long)]
    safe: bool,

    /// 只处理指定规则 id（可多次）
    #[arg(long = "id")]
    ids: Vec<String>,
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
        Cmd::Rules => cmd_rules()?,
        Cmd::Clean(args) => cmd_clean(args)?,
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

fn cmd_clean(args: CleanArgs) -> Result<()> {
    if !args.dry_run {
        eprintln!("M0 仅支持 --dry-run（只读预览）。真正删除将在后续版本提供。");
        std::process::exit(2);
    }

    let catalog = rules::load()?;
    let items = scan::scan(&catalog, false, 1_048_576);

    let selected: Vec<_> = items
        .into_iter()
        .filter(|it| {
            if !args.ids.is_empty() {
                return args.ids.contains(&it.rule_id);
            }
            if args.safe {
                return it.risk == model::Risk::Safe;
            }
            it.risk == model::Risk::Safe
        })
        .collect();

    if selected.is_empty() {
        println!("没有符合条件的清理项。");
        return Ok(());
    }

    println!("\x1b[1m清理计划 (dry-run)\x1b[0m\n");
    let mut total: u64 = 0;
    for it in &selected {
        total = total.saturating_add(it.size);
        println!("• {:<28} {:>10}", it.name, report::human(it.size));
        println!("  {:<28} {}", "方式:", it.reclaim);
        if it.sudo {
            println!("  {:<28} {}", "注意:", "\x1b[33m需要 sudo\x1b[0m");
        }
        println!("  {:<28} {}", "路径:", it.path.display());
    }
    println!(
        "\n共计 {} 项，预计释放 \x1b[1m{}\x1b[0m",
        selected.len(),
        report::human(total)
    );
    println!("（dry-run，未执行任何删除）");
    Ok(())
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
}
