//! App 列表 / 卸载 / 孤立残留。

use super::clean::warn_snapshots;
use crate::*;

pub(crate) fn cmd_apps(args: AppsArgs) -> Result<()> {
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

pub(crate) fn cmd_uninstall(args: UninstallArgs) -> Result<()> {
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
    // --deep：用 Spotlight 补充带后缀/嵌套的残留（仍走同一安全门）
    let leftovers = if args.deep {
        apps::find_leftovers_deep(&app.name, app.bundle_id.as_deref(), &app.path)
    } else {
        app.leftovers.clone()
    };
    for l in &leftovers {
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
    if args.deep {
        println!("（已启用 Spotlight 深扫）");
    }
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

    // 需 root 的系统级残留：仅在显式 --sudo 时作为提权执行目标
    let sudo_pending: Vec<CleanItem> = if args.sudo {
        clean::sudo_items(&items)
    } else {
        Vec::new()
    };

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
        if !sudo_pending.is_empty() {
            println!(
                "其中 {} 项需管理员权限，加 --sudo 会弹系统授权框。",
                sudo_pending.len()
            );
        }
        if apps::is_running(&app.path) {
            println!("注意：{} 正在运行；加 --kill 可先退出再卸载。", app.name);
        }
        return Ok(());
    }

    // SafetyGate：运行中的 App 不硬删。--kill 时先退出该 App 自身进程，
    // 但仍走废纸篓/隔离区，不引入任何直接删除路径。
    if apps::is_running(&app.path) {
        if !args.kill {
            println!(
                "\x1b[31m已取消：{} 正在运行，请先退出（或用 --kill）。\x1b[0m",
                app.name
            );
            return Ok(());
        }
        // 退出进程是破坏性动作，即便 --yes 也单独确认一次
        if !confirm(&format!("{} 正在运行，退出其进程后再卸载？", app.name))? {
            println!("已取消。");
            return Ok(());
        }
        if let Err(e) = apps::kill_app(&app.path) {
            println!("\x1b[31m已取消：{e:#}\x1b[0m");
            return Ok(());
        }
        println!("已退出 {}", app.name);
    }
    if plan.approved.is_empty() && sudo_pending.is_empty() {
        println!("没有可通过安全门的项。");
        return Ok(());
    }
    if !args.yes
        && !confirm(&format!(
            "卸载 {} 并移入{}？{}",
            app.name,
            mode.label(),
            if sudo_pending.is_empty() {
                String::new()
            } else {
                format!("（另有 {} 项需管理员权限）", sudo_pending.len())
            }
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
                "uninstall",
                None,
                items.len(),
                plan.approved.len(),
                (0, 0),
                Some(journal),
            );
        }
    }

    // 需 root 的系统级残留（LaunchDaemons 等）：弹系统授权框
    if !sudo_pending.is_empty() {
        match clean::elevate_sudo(&sudo_pending) {
            Ok(journal) => {
                println!(
                    "\n\x1b[1m提权清理\x1b[0m：移入隔离区 {} 项 · {}（会话 {}）",
                    journal.entries.len(),
                    human(journal.total_size()),
                    journal.session
                );
                println!("可用 `thin quarantine restore {}` 恢复。", journal.session);
                record_history(
                    "uninstall-sudo",
                    None,
                    sudo_pending.len(),
                    journal.entries.len(),
                    (0, 0),
                    Some(&journal),
                );
            }
            Err(e) => eprintln!("\x1b[33m提权清理未执行：{e:#}\x1b[0m"),
        }
    }
    warn_snapshots();
    Ok(())
}

pub(crate) fn cmd_orphans(args: OrphansArgs) -> Result<()> {
    // 提权需要交互授权框，JSON 非交互模式不支持；先于扫描直接拒绝
    if args.sudo && args.json {
        return Err(anyhow!(
            "提权清理需交互授权框，暂不支持 --json；请去掉 --json 或改用隔离区手动处理"
        ));
    }
    let found = orphans::find_orphans();

    if found.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "orphans": [], "totalBytes": 0, "reclaimableBytes": 0 })
                )?
            );
        } else {
            println!("未发现已卸载 App 的孤立残留。");
        }
        return Ok(());
    }

    // 所有残留拼成清理项，走与 clean / uninstall 同一个安全门
    let items: Vec<CleanItem> = found
        .iter()
        .flat_map(|o| {
            o.leftovers.iter().map(|l| {
                let mut it = CleanItem::synthetic(
                    l.path.clone(),
                    l.size,
                    "orphan-leftover",
                    &format!("{} 孤立残留", o.bundle_id),
                    Risk::Confirm,
                );
                it.sudo = l.sudo;
                it
            })
        })
        .collect();
    let plan = clean::plan(&items);
    // 需 root 的系统级残留：仅在显式 --sudo 时作为提权执行目标
    let sudo_pending: Vec<CleanItem> = if args.sudo {
        clean::sudo_items(&items)
    } else {
        Vec::new()
    };
    // App 沙盒容器受 TCC 保护：缺 FDA 时只会得到「失败」，先明确提醒
    let container_paths = |l: &thin_core::apps::Leftover| {
        let s = l.path.to_string_lossy();
        s.contains("/Library/Containers/") || s.contains("/Library/Group Containers/")
    };
    let needs_fda = probe::full_disk_access() == Some(false)
        && found
            .iter()
            .any(|o| o.leftovers.iter().any(container_paths));

    if args.json {
        let out = serde_json::json!({
            "orphans": found,
            "totalBytes": found.iter().map(|o| o.total()).sum::<u64>(),
            "reclaimableBytes": plan.approved_bytes(),
            "needsFullDiskAccess": needs_fda,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        if !args.apply {
            return Ok(());
        }
    } else {
        println!("\n\x1b[1m已卸载 App 的孤立残留\x1b[0m\n");
        if needs_fda {
            println!(
                "\x1b[33m提示：包含 App 沙盒容器，但未授予「完全磁盘访问权限」，这些项会清理失败。\x1b[0m"
            );
            println!(
                "\x1b[90m      系统设置 → 隐私与安全性 → 完全磁盘访问权限，勾选你的终端 App 后重试。\x1b[0m\n"
            );
        }
        for o in &found {
            println!("\x1b[1m{}\x1b[0m  {}", o.bundle_id, human(o.total()));
            for l in &o.leftovers {
                println!("    {:<44} {:>10}", shorten(&l.path), human(l.size));
            }
        }
        println!(
            "\n共 {} 个 bundle、{} 项，可自动释放 \x1b[1m{}\x1b[0m",
            found.len(),
            items.len(),
            human(plan.approved_bytes())
        );
        for s in &plan.skipped {
            println!("  \x1b[33m跳过\x1b[0m {}：{}", shorten(&s.path), s.reason);
        }
    }

    let mode = if args.quarantine {
        clean::Mode::Quarantine
    } else {
        clean::default_mode()
    };
    if !args.apply {
        println!("（预览；加 --apply 移入{}，可恢复）", mode.label());
        if !sudo_pending.is_empty() {
            println!(
                "其中 {} 项需管理员权限，加 --sudo 会弹系统授权框。",
                sudo_pending.len()
            );
        }
        return Ok(());
    }
    if plan.approved.is_empty() && sudo_pending.is_empty() {
        println!("没有可通过安全门的项。");
        return Ok(());
    }
    if !args.yes
        && !confirm(&format!(
            "清理 {} 个孤立 bundle 的残留并移入{}？{}",
            found.len(),
            mode.label(),
            if sudo_pending.is_empty() {
                String::new()
            } else {
                format!("（另有 {} 项需管理员权限）", sudo_pending.len())
            }
        ))?
    {
        println!("已取消。");
        return Ok(());
    }
    match clean::apply(&items, mode)? {
        clean::Applied::Trash(report) => {
            print_trash_report(&report);
            record_history_trash(None, items.len(), plan.approved.len(), &report);
        }
        clean::Applied::Quarantine(journal) => {
            print_journal(&journal);
            record_history(
                "orphans",
                None,
                items.len(),
                plan.approved.len(),
                (0, 0),
                Some(&journal),
            );
        }
    }
    // 需 root 的系统级残留：弹系统授权框
    if !sudo_pending.is_empty() {
        match clean::elevate_sudo(&sudo_pending) {
            Ok(journal) => {
                println!(
                    "\n\x1b[1m提权清理\x1b[0m：移入隔离区 {} 项 · {}（会话 {}）",
                    journal.entries.len(),
                    human(journal.total_size()),
                    journal.session
                );
                println!("可用 `thin quarantine restore {}` 恢复。", journal.session);
                record_history(
                    "orphans-sudo",
                    None,
                    sudo_pending.len(),
                    journal.entries.len(),
                    (0, 0),
                    Some(&journal),
                );
            }
            Err(e) => eprintln!("\x1b[33m提权清理未执行：{e:#}\x1b[0m"),
        }
    }
    warn_snapshots();
    Ok(())
}
