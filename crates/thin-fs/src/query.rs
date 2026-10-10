//! 按谓词查找目录 / 文件（`findDir` / `findFile` 的统一底座）。

use crate::kind::{Entry, Kind};
use crate::progress::Control;
use crate::walk::{Visit, Walk, WalkOptions};
use std::path::PathBuf;

/// 查找谓词。可组合（[`Predicate::All`]）。
#[derive(Clone, Debug)]
pub enum Predicate {
    /// 目录名精确匹配（对应 `findDir` 的 `dirName`）。
    DirName(String),
    /// 文件后缀匹配（忽略大小写与前导点，支持 `tar.gz`；对应 `findFile` 的 `extensions`）。
    Suffix(Vec<String>),
    /// 同级必须存在该标志文件（对应 `requireSibling` / `Podfile` 等）。
    Sibling(String),
    /// 不小于该字节数。
    MinSize(u64),
    /// 全部满足。
    All(Vec<Predicate>),
}

impl Predicate {
    /// 后缀规范化：统一为「小写、带前导点」。
    fn normalize_suffixes(exts: &[String]) -> Vec<String> {
        exts.iter()
            .map(|e| format!(".{}", e.trim().trim_start_matches('.').to_ascii_lowercase()))
            .filter(|e| e.len() > 1)
            .collect()
    }

    /// 返回一份已预处理的谓词（后缀已规范化），供遍历时零分配匹配。
    fn normalized(&self) -> Predicate {
        match self {
            Predicate::Suffix(exts) => Predicate::Suffix(Self::normalize_suffixes(exts)),
            Predicate::All(list) => Predicate::All(list.iter().map(|p| p.normalized()).collect()),
            other => other.clone(),
        }
    }

    fn matches(&self, e: &Entry<'_>) -> bool {
        match self {
            Predicate::DirName(name) => {
                e.kind() == Kind::Dir
                    && e.path.file_name().and_then(|n| n.to_str()) == Some(name.as_str())
            }
            Predicate::Suffix(exts) => {
                if e.kind() != Kind::File {
                    return false;
                }
                let Some(name) = e.path.file_name().and_then(|n| n.to_str()) else {
                    return false;
                };
                let name = name.to_ascii_lowercase();
                exts.iter().any(|ext| name.ends_with(ext.as_str()))
            }
            Predicate::Sibling(sib) => e
                .path
                .parent()
                .map(|p| p.join(sib).exists())
                .unwrap_or(false),
            Predicate::MinSize(min) => e.meta.map(|m| m.size >= *min).unwrap_or(false),
            Predicate::All(list) => list.iter().all(|p| p.matches(e)),
        }
    }
}

/// 查找规格。
#[derive(Clone, Debug)]
pub struct FindSpec {
    pub pred: Predicate,
    pub opts: WalkOptions,
    /// 命中后是否不再下钻（对应 `findDir` 命中后 `skip_current_dir`）。
    pub once: bool,
}

impl FindSpec {
    pub fn new(pred: Predicate, opts: WalkOptions) -> Self {
        Self {
            pred,
            opts,
            once: false,
        }
    }
}

/// 在多个根目录下查找满足谓词的路径。
pub fn find(roots: &[PathBuf], spec: &FindSpec, ctl: &Control<'_>) -> Vec<PathBuf> {
    // 与既有语义一致：只遍历存在的目录。
    let dirs: Vec<PathBuf> = roots.iter().filter(|r| r.is_dir()).cloned().collect();
    if dirs.is_empty() {
        return Vec::new();
    }

    let mut found = Vec::new();
    let pred = spec.pred.normalized();
    Walk::new(spec.opts.clone()).run(&dirs, ctl, |e| {
        if pred.matches(&e) {
            found.push(e.path.to_path_buf());
            if spec.once {
                return Visit::Skip;
            }
        }
        Visit::Continue
    });
    found
}

/// 便捷：按目录名 + 可选同级文件查找（对应 `findDir`）。
pub fn find_dir(
    roots: &[PathBuf],
    dir_name: &str,
    require_sibling: Option<&str>,
    opts: WalkOptions,
    ctl: &Control<'_>,
) -> Vec<PathBuf> {
    let mut preds = vec![Predicate::DirName(dir_name.to_string())];
    if let Some(sib) = require_sibling {
        preds.push(Predicate::Sibling(sib.to_string()));
    }
    let mut spec = FindSpec::new(Predicate::All(preds), opts);
    spec.once = true;
    find(roots, &spec, ctl)
}

/// 便捷：按后缀 + 最小大小查找文件（对应 `findFile`）。
pub fn find_files(
    roots: &[PathBuf],
    extensions: &[String],
    opts: WalkOptions,
    min_size: u64,
    ctl: &Control<'_>,
) -> Vec<PathBuf> {
    let mut preds = vec![Predicate::Suffix(extensions.to_vec())];
    if min_size > 0 {
        preds.push(Predicate::MinSize(min_size));
    }
    let spec = FindSpec::new(Predicate::All(preds), opts);
    find(roots, &spec, ctl)
}
