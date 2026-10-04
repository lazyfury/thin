use spacekit_core::fmt::human;
use spacekit_core::model::CleanItem;

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
        "{:>10}  {:<10} {:<8} {:<26} {}",
        "大小", "风险", "类别", "名称", "路径"
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
        let risk_cell = pad_colored(it.risk.label(), it.risk.color(), 8);
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
    let s = spacekit_core::scan::summarize(items);
    let nested = spacekit_core::scan::nested_count(items);
    println!();
    println!(
        "\x1b[1m可回收总计: {}\x1b[0m  （安全 {} / 需确认 {} / 不可再生 {})",
        human(s.total_reclaimable()),
        human(s.safe),
        human(s.confirm),
        human(s.destructive)
    );
    let extra = if nested > 0 {
        format!("，已排除 {nested} 个嵌套重复项")
    } else {
        String::new()
    };
    println!(
        "共 {} 项{extra}。默认仅「安全+需确认」计入可回收。",
        items.len()
    );
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
    println!("  清理方式: {}", it.reclaim);
}
