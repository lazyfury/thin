//! 清理 / 隔离区 / 保护名单。

use crate::*;

pub(crate) fn cmd_protect(args: ProtectArgs) -> Result<()> {
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
pub(super) fn resolve_mode(quarantine: bool, explicit_trash: bool) -> clean::Mode {
    if quarantine {
        clean::Mode::Quarantine
    } else if explicit_trash {
        clean::Mode::Trash
    } else {
        clean::default_mode()
    }
}

pub(crate) fn cmd_clean(args: CleanArgs) -> Result<()> {
    let selected = select_items(&args)?;
    // 预演与执行共用同一安全门，保证「预览即所得」
    let plan = clean::plan(&selected);
    let apply = args.apply && !args.dry_run;
    let mode = resolve_mode(args.quarantine, args.trash);

    // 提权需要交互授权框，JSON 非交互模式不支持
    if args.sudo && args.json {
        return Err(anyhow!(
            "提权清理需交互授权框，暂不支持 --json；请去掉 --json 或改用隔离区手动处理"
        ));
    }

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
    // --manual 展示，--sudo 则视作提权执行目标；真正执行仍走完整 selected（安全门逐项把关）。
    let shown_selected: Vec<thin_core::CleanItem> = if args.manual || args.sudo {
        selected.clone()
    } else {
        selected
            .iter()
            .filter(|it| !scan::is_manual(it))
            .cloned()
            .collect()
    };
    let shown_plan = clean::plan(&shown_selected);

    // 需提权的项：仅在显式 --sudo 时视为可执行目标
    let sudo_pending: Vec<thin_core::CleanItem> = if args.sudo {
        clean::sudo_items(&shown_selected)
    } else {
        Vec::new()
    };

    if shown_plan.approved.is_empty() && sudo_pending.is_empty() {
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
        if !sudo_pending.is_empty() {
            let bytes: u64 = sudo_pending.iter().map(|i| i.size).sum();
            println!(
                "\n另有 {} 项需要管理员权限（{}），加 --apply --sudo 会弹系统授权框。",
                sudo_pending.len(),
                human(bytes)
            );
        }
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
            "将 {} 项移入{}？{}",
            plan.approved.len(),
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

    // 提权项：弹系统授权框，以 root 复核安全门后移入调用者隔离区
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
                    "manual-sudo",
                    args.preset.as_deref(),
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

/// 存在本地 APFS 快照时提醒：即使 purge，空间也可能不会立即释放
pub(super) fn warn_snapshots() {
    if !probe::local_snapshots().is_empty() {
        println!("\x1b[33m注意: 存在本地 APFS 快照，purge 后空间也可能不会立即释放。\x1b[0m");
    }
}

pub(crate) fn cmd_quarantine(args: QuarantineArgs) -> Result<()> {
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
