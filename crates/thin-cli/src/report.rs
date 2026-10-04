use thin_core::fmt::human;
use thin_core::model::CleanItem;
use thin_core::tree::TreeNode;

fn char_width(s: &str) -> usize {
    s.chars().count()
}

fn pad_colored(label: &str, color: &str, width: usize) -> String {
    let w = char_width(label);
    let padding = " ".repeat(width.saturating_sub(w));
    format!("{}{}\x1b[0m{}", color, label, padding)
}

/// 打印扫描结果表格
pub fn print_table(items: &[CleanItem]) {
    println!(
        "{:>10}  {:<10} {:<8} {:<26} 路径",
        "大小", "风险", "类别", "名称"
    );
    println!("{}", "-".repeat(96));
    let home = std::env::var("HOME").unwrap_or_default();
    for it in items {
        let path = it.path.display().to_string();
        let shown = if !home.is_empty() && path.starts_with(&home) {
            path.replacen(&home, "~", 1)
        } else {
            path
        };
        let (label, color) = if it.protected {
            ("已保护", "\x1b[90m")
        } else if it.sudo {
            ("需 sudo", "\x1b[33m")
        } else {
            (it.risk.label(), it.risk.color())
        };
        let risk_cell = pad_colored(label, color, 8);
        let cat_cell = format!("{:<8}", it.category.label());
        println!(
            "{:>10}  {} {} {:<26} {}",
            human(it.size),
            risk_cell,
            cat_cell,
            truncate(&it.name, 26),
            shown
        );
    }
}

/// 打印按文件夹合并的树形视图。
///
/// 目录节点显示聚合体积与命中项数；命中项叶子显示风险标签与体积。
/// 被父项覆盖的嵌套项以「嵌套」标注，不计入父项聚合体积。
pub fn print_tree(forest: &[TreeNode]) {
    for node in forest {
        print_tree_node(node, "", true, true);
    }
}

fn print_tree_node(node: &TreeNode, prefix: &str, is_last: bool, root: bool) {
    let branch = if root {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };
    if let Some(it) = &node.item {
        let (label, color) = if it.protected {
            ("已保护", "\x1b[90m")
        } else if it.sudo {
            ("需 sudo", "\x1b[33m")
        } else {
            (it.risk.label(), it.risk.color())
        };
        let nested = if node.nested {
            " \x1b[90m(嵌套)\x1b[0m"
        } else {
            ""
        };
        // 树里用路径末段更有信息量（同级不同目录可区分）；规则名附在后面
        let rule = if it.name != node.name {
            format!(" \x1b[90m({})\x1b[0m", it.name)
        } else {
            String::new()
        };
        println!(
            "{prefix}{branch}{}  \x1b[1m{}\x1b[0m  {color}{label}\x1b[0m{nested}{rule}",
            node.name,
            human(node.size),
        );
    } else {
        println!(
            "{prefix}{branch}{}/  \x1b[1m{}\x1b[0m  \x1b[90m· {} 项\x1b[0m",
            node.name,
            human(node.size),
            node.count,
        );
    }
    let child_prefix = if root {
        String::new()
    } else {
        format!("{}{}", prefix, if is_last { "   " } else { "│  " })
    };
    let n = node.children.len();
    for (i, c) in node.children.iter().enumerate() {
        print_tree_node(c, &child_prefix, i + 1 == n, false);
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

/// 打印可回收汇总
pub fn print_summary(items: &[CleanItem]) {
    let s = thin_core::scan::summarize(items);
    let nested = thin_core::scan::nested_count(items);
    println!();
    let protected = if s.protected > 0 {
        format!(" / 已保护 {}", human(s.protected))
    } else {
        String::new()
    };
    let manual = if s.manual > 0 {
        format!(" / 需手动 {}", human(s.manual))
    } else {
        String::new()
    };
    println!(
        "\x1b[1m可回收总计: {}\x1b[0m  （安全 {} / 需确认 {} / 不可再生 {}{}{}）",
        human(s.total_reclaimable()),
        human(s.safe),
        human(s.confirm),
        human(s.destructive),
        manual,
        protected
    );
    let extra = if nested > 0 {
        format!("，已排除 {nested} 个嵌套重复项")
    } else {
        String::new()
    };
    let sudo_note = if s.manual > 0 {
        "；需 sudo 项需手动处理"
    } else {
        ""
    };
    println!(
        "共 {} 项{extra}。默认仅「安全+需确认」计入可回收{}。",
        items.len(),
        sudo_note
    );
    if s.protected > 0 {
        println!(
            "\x1b[90m标记「已保护」的项不会清理（thin protect 名单或系统保护路径）；用 thin protect list 查看/移除。\x1b[0m"
        );
    }
}

/// 打印单项详细解释
pub fn print_detail(it: &CleanItem) {
    println!("\x1b[1m{}\x1b[0m  ({})", it.name, human(it.size));
    println!("  路径:   {}", it.path.display());
    println!("  类别:   {}", it.category.label());
    println!(
        "  风险:   {}{}\x1b[0m   可再生: {}",
        it.risk.color(),
        it.risk.label(),
        if it.regenerable { "是" } else { "否" }
    );
    println!("  这是什么: {}", it.explain.what);
    println!("  删了会怎样: {}", it.explain.cost);
    println!("  能否恢复: {}", it.explain.recover);
    println!("  官方方式: {}", it.reclaim);
    println!("  thin 动作: 移入隔离区（可恢复）");
    if let Some(reason) = &it.protected_reason {
        println!("  \x1b[33m受保护: {reason}（不会清理）\x1b[0m");
    }
}
