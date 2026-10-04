//! `thin ls [PATH]`：列出一层子项，并给每个目录标注「用途」。
//!
//! 面向「看懂系统盘」的学习场景：`thin ls /` 会解释根目录每个 Unix 风格目录
//! 是干什么的、能不能删。识别是只读的，不触发任何清理。

use anyhow::Result;
use std::path::{Path, PathBuf};
use thin_core::catalog::Safety;
use thin_core::fmt::human;
use thin_core::fsutil::{self, EntryKind};
use thin_core::recognize::{Recognition, Recognizer, Source};

#[derive(clap::Args)]
pub struct LsArgs {
    /// 要列出的目录
    #[arg(default_value = "/")]
    pub path: String,
    /// 显示隐藏项（. 开头，如 APFS 的 .vol）
    #[arg(short, long)]
    pub all: bool,
    /// 显示用途说明正文与参考
    #[arg(short, long)]
    pub long: bool,
    /// 输出 JSON
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: LsArgs) -> Result<()> {
    let root = fsutil::expand(&args.path).unwrap_or_else(|| PathBuf::from(&args.path));
    let recognizer = Recognizer::load()?;
    let children = fsutil::children_entries(&root);
    let home = std::env::var("HOME").unwrap_or_default();

    let shown: Vec<_> = children
        .iter()
        .filter(|c| args.all || !is_hidden(&c.path))
        .collect();

    if args.json {
        let root_rec = recognizer.recognize(&root);
        let items: Vec<_> = shown
            .iter()
            .map(|c| {
                let r = recognizer.recognize(&c.path);
                serde_json::json!({
                    "name": c.path.file_name().map(|n| n.to_string_lossy().to_string()),
                    "path": c.path,
                    "kind": c.kind.label(),
                    "size": c.size,
                    "target": c.target,
                    "purpose": r,
                })
            })
            .collect();
        let out = serde_json::json!({
            "path": root,
            "purpose": root_rec,
            "children": items,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let root_rec = recognizer.recognize(&root);
    println!(
        "\x1b[1m{}\x1b[0m  \x1b[90m{}\x1b[0m",
        shorten(&root, &home),
        root_rec.title
    );
    if args.long && !root_rec.note.is_empty() {
        println!("  {}", root_rec.note);
    }
    println!(
        "{:>10}  {:<4} {:<24} {:<26} {}",
        "大小", "类型", "名称", "用途", "状态"
    );
    println!("{}", "-".repeat(104));

    for c in &shown {
        let r = recognizer.recognize(&c.path);
        let name = c
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let name_disp = match &c.target {
            Some(t) => format!("{name} → {}", shorten(t, &home)),
            None => name,
        };
        println!(
            "{:>10}  {:<4} {:<24} {:<26} {}",
            size_cell(c.size, c.kind),
            c.kind.label(),
            truncate(&name_disp, 24),
            truncate(&r.title, 26),
            status_text(&r)
        );
        if args.long {
            if !r.note.is_empty() {
                println!("            {}", r.note);
            }
            if let Some(reference) = &r.reference {
                println!("            \x1b[90m参考: {reference}\x1b[0m");
            }
        }
    }
    Ok(())
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

/// 符号链接/挂载点/无权限的大小没有意义，显示为 `—`
fn size_cell(size: u64, kind: EntryKind) -> String {
    match kind {
        EntryKind::Symlink | EntryKind::Mount | EntryKind::Inaccessible if size == 0 => "—".into(),
        _ => human(size),
    }
}

fn status_text(r: &Recognition) -> String {
    if r.protected {
        return "\x1b[90m受保护\x1b[0m".into();
    }
    if r.cleanable
        && let Some(risk) = r.risk
    {
        return format!("{}可清理·{}\x1b[0m", risk.color(), risk.label());
    }
    if r.source == Source::Heuristic {
        return "\x1b[33m疑似\x1b[0m".into();
    }
    match r.safety {
        Safety::Regenerable => "\x1b[32m可再生\x1b[0m".into(),
        Safety::Precious => "\x1b[33m重要\x1b[0m".into(),
        Safety::Protected => "\x1b[90m受保护\x1b[0m".into(),
        Safety::Unknown => String::new(),
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

fn shorten(path: &Path, home: &str) -> String {
    let s = path.display().to_string();
    if !home.is_empty() && s.starts_with(home) {
        s.replacen(home, "~", 1)
    } else {
        s
    }
}
