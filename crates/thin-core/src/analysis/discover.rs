//! 异常大目录归因（M3）：找出「未被规则覆盖」的大目录，便于补规则。

use crate::fsutil;
use crate::model::Rule;
use rayon::prelude::*;
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
    /// 实际占用（不含云占位）
    pub size: u64,
    /// iCloud 未下载占位的逻辑字节（本地不占空间）
    pub dataless: u64,
    pub coverage: Coverage,
}

/// 分析结果：命中的大目录 + 覆盖率自检
#[derive(Debug, Default)]
pub struct Report {
    /// 达到体积阈值的子项（降序）
    pub findings: Vec<Finding>,
    /// root 下所有直接子项的占用合计（不受阈值过滤）
    pub total: u64,
    /// 已完整归类的占用
    pub covered: u64,
    /// 部分覆盖的占用
    pub partial: u64,
    /// 完全未归类的占用
    pub uncovered: u64,
    /// 直接子项中 iCloud 未下载占位的逻辑字节合计
    pub dataless: u64,
    /// 直接子项中元数据不可读的条目数（权限 / TCC）。
    /// **诚实核算**：总量可能因此偏低，需提示用户授权完全磁盘访问。
    pub denied: u64,
}

impl Report {
    /// 已归类 + 部分覆盖占 root 的比例（0.0–1.0）
    pub fn coverage_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.covered + self.partial) as f64 / self.total as f64
        }
    }
}

/// 分析 root 的直接子项，标注哪些已被规则覆盖，并给出覆盖率自检。
///
/// 分类基于**全部**直接子项（不受 `min_size` 影响），`findings` 只保留达到
/// 阈值的项用于展示；因此 `uncovered` 能诚实回答「这个目录还有多少没归类」。
///
/// 默认不查询 iCloud 云占位（逐文件 XPC，较慢）；需要时用 [`analyze_opts`]。
pub fn analyze(root: &Path, min_size: u64, catalog: &[Rule]) -> Report {
    analyze_opts(root, min_size, catalog, false)
}

/// 同 [`analyze`]，`include_icloud=true` 时额外按需统计云占位（较慢）。
pub fn analyze_opts(root: &Path, min_size: u64, catalog: &[Rule], include_icloud: bool) -> Report {
    // 展开所有规则路径（去重）
    let mut rule_paths: Vec<(PathBuf, String)> = Vec::new();
    for rule in catalog {
        for p in crate::rules::expand_rule(rule) {
            rule_paths.push((p, rule.id.clone()));
        }
    }

    let paths: Vec<PathBuf> = match std::fs::read_dir(root) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => return Report::default(),
    };

    // 并行统计各子项占用（io 密集）；云占位仅在需要时按需查询。
    let sized: Vec<(PathBuf, fsutil::Usage, u64)> = paths
        .into_par_iter()
        .map(|path| {
            let usage = fsutil::usage(&path);
            let dataless = if include_icloud {
                fsutil::dir_dataless(&path).map(|(d, _)| d).unwrap_or(0)
            } else {
                0
            };
            (path, usage, dataless)
        })
        .collect();

    let mut report = Report::default();
    for (path, usage, dataless) in sized {
        let size = usage.allocated;
        report.dataless = report.dataless.saturating_add(dataless);
        report.total = report.total.saturating_add(size);
        report.denied = report.denied.saturating_add(usage.denied);
        let coverage = classify(&path, &rule_paths);
        match &coverage {
            Coverage::Full(_) => report.covered = report.covered.saturating_add(size),
            Coverage::Partial(_) => report.partial = report.partial.saturating_add(size),
            Coverage::None => report.uncovered = report.uncovered.saturating_add(size),
        }
        if size >= min_size {
            report.findings.push(Finding {
                path,
                size,
                dataless,
                coverage,
            });
        }
    }
    report.findings.sort_by_key(|f| std::cmp::Reverse(f.size));
    report
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
    fn report_totals_and_ratio() {
        let r = Report {
            findings: Vec::new(),
            total: 1000,
            covered: 400,
            partial: 100,
            uncovered: 500,
            dataless: 0,
            denied: 0,
        };
        assert!((r.coverage_ratio() - 0.5).abs() < 1e-9);
        let empty = Report::default();
        assert_eq!(empty.coverage_ratio(), 0.0);
    }

    #[test]
    fn slug_helpers_available() {
        // 保证 rules 模块的解析函数可用（间接覆盖）
        assert!(rules::parse_risk("safe").is_some());
    }
}
