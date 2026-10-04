use crate::report::human;
use std::path::PathBuf;

/// `spacekit top [PATH]`：列出某目录下各子项占用（类似 `du -sh PATH/* | sort -rh`）
pub fn run(path: PathBuf, limit: usize) {
    if !path.is_dir() {
        eprintln!("不是目录: {}", path.display());
        return;
    }

    let mut children: Vec<(u64, PathBuf)> = Vec::new();
    match std::fs::read_dir(&path) {
        Ok(entries) => {
            for e in entries.flatten() {
                let p = e.path();
                let size = crate::fsutil::size_of(&p);
                children.push((size, p));
            }
        }
        Err(e) => {
            eprintln!("无法读取 {}: {e}", path.display());
            return;
        }
    }

    children.sort_by(|a, b| b.0.cmp(&a.0));
    let home = std::env::var("HOME").unwrap_or_default();

    println!("\x1b[1m{}\x1b[0m", path.display());
    println!("{:>10}  {}", "大小", "子项");
    println!("{}", "-".repeat(70));
    for (size, p) in children.into_iter().take(limit) {
        let s = p.display().to_string();
        let shown = if !home.is_empty() && s.starts_with(&home) {
            s.replacen(&home, "~", 1)
        } else {
            s
        };
        println!("{:>10}  {}", human(size), shown);
    }
}
