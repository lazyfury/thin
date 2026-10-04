//! 异常大目录归因（M3）：找出「未被规则覆盖」的大目录，便于补规则。

use crate::fsutil;
use crate::model::Rule;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum Coverage {
    /// 已被某规则完整覆盖（路径在规则路径之内）
    Full(String),
    /// 部分覆盖（目录内部有规则，但本身还有未归类数据）
    Partial(Vec<String>),
    /// 未归类
    None,
}

impl Coverage {
    pub fn label(&self) -> String {
        match self {
            Coverage::Full(_) => "已归类".into(),
            Coverage::Partial(ids) => format!("部分({})", ids.join(",")),
            Coverage::None => "未归类".into(),
        }
    }

    pub fn is_uncovered(&self) -> bool {
        !matches!(self, Coverage::Full(_))
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub path: PathBuf,
    pub size: u64,
    pub coverage: Coverage,
}

/// 分析 root 的直接子项，标注哪些已被规则覆盖。
pub fn analyze(root: &Path, min_size: u64, catalog: &[Rule]) -> Vec<Finding> {
    // 展开所有规则路径（去重）
    let mut rule_paths: Vec<(PathBuf, String)> = Vec::new();
    for rule in catalog {
        for p in crate::rules::expand_rule(rule) {
            rule_paths.push((p, rule.id.clone()));
        }
    }

    let mut findings = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return findings,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let size = fsutil::size_of(&path);
        if size < min_size {
            continue;
        }
        let coverage = classify(&path, &rule_paths);
        findings.push(Finding {
            path,
            size,
            coverage,
        });
    }
    findings.sort_by(|a, b| b.size.cmp(&a.size));
    findings
}

fn classify(path: &Path, rule_paths: &[(PathBuf, String)]) -> Coverage {
    let mut partial = Vec::new();
    for (rp, id) in rule_paths {
        // 候选路径位于某规则路径之内 → 已完整覆盖
        if path.starts_with(rp) {
            return Coverage::Full(id.clone());
        }
        // 规则路径位于候选目录之内 → 部分覆盖
        if rp.starts_with(path) {
            partial.push(id.clone());
        }
    }
    if partial.is_empty() {
        Coverage::None
    } else {
        partial.sort();
        partial.dedup();
        Coverage::Partial(partial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules;

    #[test]
    fn classify_full_partial_none() {
        let rp = vec![
            (PathBuf::from("/a/b/cache"), "rule-cache".to_string()),
            (PathBuf::from("/a/d"), "rule-d".to_string()),
        ];
        assert!(matches!(
            classify(Path::new("/a/b/cache/x"), &rp),
            Coverage::Full(_)
        ));
        assert!(matches!(
            classify(Path::new("/a/b"), &rp),
            Coverage::Partial(_)
        ));
        assert!(matches!(classify(Path::new("/z"), &rp), Coverage::None));
    }

    #[test]
    fn slug_helpers_available() {
        // 保证 rules 模块的解析函数可用（间接覆盖）
        assert!(rules::parse_risk("safe").is_some());
    }
}
