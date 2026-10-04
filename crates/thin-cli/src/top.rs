use std::path::PathBuf;
use thin_core::fmt::human;

/// `thin top [PATH]`：列出某目录下各子项占用（类似 `du -sh PATH/* | sort -rh`）
pub fn run(path: PathBuf, limit: usize) {
    if !path.is_dir() {
        eprintln!("不是目录: {}", path.display());
        return;
    }

    // 并行统计各直接子项（不跨卷），已按大小降序
    let children = thin_core::fsutil::children_sizes(&path);

    let home = std::env::var("HOME").unwrap_or_default();

    println!("\x1b[1m{}\x1b[0m", path.display());
    println!("{:>10}  {}", "大小", "子项");
    println!("{}", "-".repeat(70));
    for (p, size) in children.into_iter().take(limit) {
        let s = p.display().to_string();
        let shown = if !home.is_empty() && s.starts_with(&home) {
            s.replacen(&home, "~", 1)
        } else {
            s
        };
        println!("{:>10}  {}", human(size), shown);
    }
}
