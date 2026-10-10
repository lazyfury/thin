//! 扫描 / 发现 / 计划 / 执行 / 大文件 / 重复文件。

use super::clean::{resolve_mode, warn_snapshots};
use crate::spin::with_progress;
use crate::*;

pub(crate) fn cmd_scan(args: ScanArgs) -> Result<()> {
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
pub(crate) fn cmd_discover(args: DiscoverArgs) -> Result<()> {
    let root = expand_root(&args.root);
    let min = parse_size_arg(&args.min)?;
    let catalog = rules::load()?;
    eprintln!("分析 {} …", root.display());
    let report = discover::analyze_opts(&root, min, &catalog, args.icloud);

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
            "denied": report.denied,
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
    if report.denied > 0 {
        println!(
            "  \x1b[33m{} 个条目元数据不可读（权限 / TCC），合计可能偏低；\
             可在 系统设置 → 隐私与安全性 → 完全磁盘访问权限 授权后重试\x1b[0m",
            report.denied
        );
    }
    Ok(())
}
/// `thin plan`：生成清理计划（只读 JSON）。
///
/// 这是给 agent / 脚本的两阶段契约的「第一阶段」：agent 先拿到计划，审阅/决策后
/// 再用 `thin apply --plan` 原样执行。计划中的项会带在 `approved` 里，
/// 执行时会再次过同一安全门，所以计划过期也不会误删。
pub(crate) fn cmd_plan(args: PlanArgs) -> Result<()> {
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
pub(crate) fn cmd_apply(args: ApplyArgs) -> Result<()> {
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
pub(crate) fn cmd_large(args: LargeArgs) -> Result<()> {
    let root = expand_root(&args.root);
    let min = parse_size_arg(&args.min)?;
    let files = with_progress("扫描大文件", |p| {
        finder::find_large(&[root], min, args.limit, p)
    });
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

pub(crate) fn cmd_dupes(args: DupesArgs) -> Result<()> {
    let root = expand_root(&args.root);
    let min = parse_size_arg(&args.min)?;
    let groups = with_progress("扫描重复文件（需读取内容）", |p| {
        finder::find_duplicates(&[root], min, args.limit, p)
    });
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
