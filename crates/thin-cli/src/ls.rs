//! `thin ls [PATH]`：列出一层（或 `--depth N` 多层）子项，并给每个目录标注「用途」。
//!
//! 面向「看懂系统盘」的学习场景：`thin ls /` 会解释根目录每个 Unix 风格目录
//! 是干什么的、能不能删。识别是只读的，不触发任何清理。

use anyhow::Result;
use std::path::{Path, PathBuf};
use thin_core::catalog::Safety;
use thin_core::fmt::human;
use thin_core::fsutil::{self, ChildEntry, EntryKind};
use thin_core::recognize::{Recognition, Recognizer, Source};

#[derive(clap::Args)]
pub struct LsArgs {
    /// 要列出的目录
    #[arg(default_value = "/")]
    pub path: String,
    /// 递归层数（1 = 只列一层）
    #[arg(short, long, default_value_t = 1)]
    pub depth: usize,
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

struct Node {
    child: ChildEntry,
    rec: Recognition,
    children: Vec<Node>,
}

pub fn run(args: LsArgs) -> Result<()> {
    let root = fsutil::expand(&args.path).unwrap_or_else(|| PathBuf::from(&args.path));
    let recognizer = Recognizer::load()?;
    let home = std::env::var("HOME").unwrap_or_default();
    let depth = args.depth.max(1);

    let nodes = collect(&root, depth, &recognizer, args.all);
    let root_rec = recognizer.recognize(&root);

    if args.json {
        let out = serde_json::json!({
            "path": root,
            "purpose": root_rec,
            "children": nodes.iter().map(node_json).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

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
    print_nodes(&nodes, 0, &home, &args);
    Ok(())
}

/// 递归收集一层/多层（mount 不递归，避免跨卷）
fn collect(dir: &Path, depth: usize, recognizer: &Recognizer, all: bool) -> Vec<Node> {
    if depth == 0 {
        return Vec::new();
    }
    fsutil::children_entries(dir)
        .into_iter()
        .filter(|c| all || !is_hidden(&c.path))
        .map(|child| {
            let rec = recognizer.recognize(&child.path);
            let children = if depth > 1 && matches!(child.kind, EntryKind::Dir) {
                collect(&child.path, depth - 1, recognizer, all)
            } else {
                Vec::new()
            };
            Node {
                child,
                rec,
                children,
            }
        })
        .collect()
}

fn print_nodes(nodes: &[Node], indent: usize, home: &str, args: &LsArgs) {
    for n in nodes {
        let prefix = "  ".repeat(indent);
        let name = n
            .child
            .path
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        let name_disp = match &n.child.target {
            Some(t) => format!("{name} → {}", shorten(t, home)),
            None => name,
        };
        println!(
            "{:>10}  {:<4} {}{:<24} {:<26} {}",
            size_cell(n.child.size, n.child.kind),
            n.child.kind.label(),
            prefix,
            truncate(&name_disp, 24),
            truncate(&n.rec.title, 26),
            status_text(&n.rec)
        );
        if args.long {
            if !n.rec.note.is_empty() {
                println!("            {}{}", "  ".repeat(indent), n.rec.note);
            }
            if let Some(reference) = &n.rec.reference {
                println!(
                    "            {}\x1b[90m参考: {reference}\x1b[0m",
                    "  ".repeat(indent)
                );
            }
        }
        print_nodes(&n.children, indent + 1, home, args);
    }
}

fn node_json(n: &Node) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "name": n.child.path.file_name().map(|x| x.to_string_lossy().to_string()),
        "path": n.child.path,
        "kind": n.child.kind.label(),
        "size": n.child.size,
        "target": n.child.target,
        "purpose": n.rec,
    });
    if !n.children.is_empty() {
        obj["children"] = serde_json::Value::Array(n.children.iter().map(node_json).collect());
    }
    obj
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
