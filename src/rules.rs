use crate::fsutil;
use crate::model::{Matcher, Rule};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// 内置规则（编译期嵌入，随版本更新）
const DEFAULT_RULES: &str = include_str!("../rules/default.json");

/// 加载规则。可用环境变量 SPACEKIT_RULES 指向外部 JSON 覆盖内置规则。
pub fn load() -> Result<Vec<Rule>> {
    let raw: String = match std::env::var("SPACEKIT_RULES") {
        Ok(p) if !p.is_empty() => {
            std::fs::read_to_string(&p).with_context(|| format!("读取外部规则失败: {p}"))?
        }
        _ => DEFAULT_RULES.to_string(),
    };
    let rules: Vec<Rule> = serde_json::from_str(&raw).context("规则 JSON 解析失败")?;
    Ok(rules)
}

/// 把规则展开成具体的候选路径
pub fn expand_rule(rule: &Rule) -> Vec<PathBuf> {
    match &rule.matcher {
        Matcher::Path { paths } => paths
            .iter()
            .filter_map(|p| fsutil::expand(p))
            .filter(|p| p.exists())
            .collect(),
        Matcher::FindDir {
            roots,
            dir_name,
            require_sibling,
            max_depth,
        } => {
            let roots: Vec<PathBuf> = roots
                .iter()
                .filter_map(|r| fsutil::expand(r))
                .filter(|r| r.is_dir())
                .collect();
            fsutil::find_dirs(&roots, dir_name, require_sibling.as_deref(), *max_depth)
        }
    }
}
