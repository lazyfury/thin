//! 预设 / 历史 / 定时任务。

use crate::*;

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

pub(crate) fn cmd_preset(args: PresetArgs) -> Result<()> {
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

pub(crate) fn cmd_history(args: HistoryArgs) -> Result<()> {
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

pub(crate) fn cmd_schedule(args: ScheduleArgs) -> Result<()> {
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
