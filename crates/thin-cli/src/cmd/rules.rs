//! 规则管理（list / add / export）。

use crate::*;

pub(crate) fn cmd_rules(args: RulesArgs) -> Result<()> {
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
pub(crate) fn cmd_rules_export(args: ExportArgs) -> Result<()> {
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

pub(crate) fn cmd_rule_add(args: RuleAddArgs) -> Result<()> {
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
