use crate::model::{CleanItem, ReclaimSummary, Risk, Rule};
use crate::rules;
use rayon::prelude::*;

/// 执行规则扫描（并行），返回按大小降序的清理项
pub fn scan(rules: &[Rule], include_destructive: bool, min_size: u64) -> Vec<CleanItem> {
    let per_rule: Vec<Vec<CleanItem>> = rules
        .par_iter()
        .map(|rule| {
            let paths = rules::expand_rule(rule);
            paths
                .into_iter()
                .map(|path| {
                    let size = crate::fsutil::size_of(&path);
                    CleanItem {
                        rule_id: rule.id.clone(),
                        name: rule.name.clone(),
                        path,
                        category: rule.category,
                        risk: rule.risk,
                        regenerable: rule.regenerable,
                        sudo: rule.sudo,
                        size,
                        reclaim: rule.reclaim.clone(),
                        explain: rule.explain.clone(),
                    }
                })
                .filter(|item| item.size >= min_size)
                .collect()
        })
        .collect();

    let mut items: Vec<CleanItem> = per_rule.into_iter().flatten().collect();
    if !include_destructive {
        items.retain(|i| i.risk != Risk::Destructive);
    }
    items.sort_by(|a, b| b.size.cmp(&a.size));
    items
}

/// 判断某路径是否被列表中另一个路径包含（嵌套重复）
fn is_nested<'a>(path: &std::path::Path, all: &'a [CleanItem]) -> Option<&'a CleanItem> {
    all.iter()
        .find(|o| o.path != path && path.starts_with(&o.path))
}

/// 汇总可回收空间（排除嵌套重复项，避免父子路径重复计算）
pub fn summarize(items: &[CleanItem]) -> ReclaimSummary {
    let mut s = ReclaimSummary::default();
    for it in items {
        if is_nested(&it.path, items).is_some() {
            continue;
        }
        match it.risk {
            Risk::Safe => s.safe = s.safe.saturating_add(it.size),
            Risk::Confirm => s.confirm = s.confirm.saturating_add(it.size),
            Risk::Destructive => s.destructive = s.destructive.saturating_add(it.size),
        }
    }
    s
}

/// 统计被嵌套（重复）的项数
pub fn nested_count(items: &[CleanItem]) -> usize {
    items
        .iter()
        .filter(|it| is_nested(&it.path, items).is_some())
        .count()
}
