use crate::clean;
use crate::fsutil;
use crate::model::{Category, Explain, Matcher, Risk, Rule};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// 内置规则（编译期嵌入，随版本更新）
const DEFAULT_RULES: &str = include_str!("../rules/default.json");

/// 内置规则
pub fn builtin() -> Result<Vec<Rule>> {
    serde_json::from_str(DEFAULT_RULES).context("内置规则解析失败")
}

/// 用户规则文件（可热更新，agent 可写入）
pub fn user_rules_path() -> PathBuf {
    clean::thin_home().join("rules.json")
}

/// 读取用户规则（不存在则为空）
pub fn load_user_rules() -> Result<Vec<Rule>> {
    let path = user_rules_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取用户规则失败: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).with_context(|| format!("用户规则解析失败: {}", path.display()))
}

/// 写入用户规则文件
pub fn save_user_rules(rules: &[Rule]) -> Result<PathBuf> {
    let path = user_rules_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(rules)?)?;
    Ok(path)
}

/// 新增/覆盖一条用户规则（按 id），返回写入的文件路径
pub fn upsert_user_rule(rule: Rule) -> Result<PathBuf> {
    let mut rules = load_user_rules()?;
    rules.retain(|r| r.id != rule.id);
    rules.push(rule);
    save_user_rules(&rules)
}

/// 删除一条用户规则；返回是否删除成功
pub fn remove_user_rule(id: &str) -> Result<bool> {
    let mut rules = load_user_rules()?;
    let before = rules.len();
    rules.retain(|r| r.id != id);
    if rules.len() == before {
        return Ok(false);
    }
    save_user_rules(&rules)?;
    Ok(true)
}

/// 合并加载：内置/THIN_RULES + 用户规则（用户按 id 覆盖）
pub fn load() -> Result<Vec<Rule>> {
    let mut rules = base_rules()?;
    for ur in load_user_rules().unwrap_or_default() {
        match rules.iter().position(|r| r.id == ur.id) {
            Some(pos) => rules[pos] = ur,
            None => rules.push(ur),
        }
    }
    Ok(rules)
}

/// 基础规则：THIN_RULES 指向外部文件时覆盖内置
fn base_rules() -> Result<Vec<Rule>> {
    match std::env::var("THIN_RULES") {
        Ok(p) if !p.is_empty() => {
            let raw =
                std::fs::read_to_string(&p).with_context(|| format!("读取外部规则失败: {p}"))?;
            serde_json::from_str(&raw).context("外部规则 JSON 解析失败")
        }
        _ => builtin(),
    }
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

// ---------------------------------------------------------------------------
// 便于 agent 构造规则
// ---------------------------------------------------------------------------

/// 把分类字符串解析为 Category
pub fn parse_category(s: &str) -> Option<Category> {
    Some(match s.to_lowercase().as_str() {
        "system-cache" | "system" => Category::SystemCache,
        "app-cache" | "app" => Category::AppCache,
        "dev-cache" | "dev" => Category::DevCache,
        "vm" => Category::Vm,
        "log" | "logs" => Category::Log,
        "trash" => Category::Trash,
        "leftover" => Category::Leftover,
        "other" => Category::Other,
        _ => return None,
    })
}

/// 把风险字符串解析为 Risk
pub fn parse_risk(s: &str) -> Option<Risk> {
    Some(match s.to_lowercase().as_str() {
        "safe" => Risk::Safe,
        "confirm" => Risk::Confirm,
        "destructive" => Risk::Destructive,
        _ => return None,
    })
}

/// 由路径生成一个 slug，用作默认 id
pub fn slug_for(path: &str) -> String {
    let base = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("rule");
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "custom".to_string()
    } else {
        format!("custom-{slug}")
    }
}

/// 构造一条最简单的 path 规则（agent 入口用）
#[allow(clippy::too_many_arguments)]
pub fn make_path_rule(
    id: String,
    name: String,
    path: String,
    category: Category,
    risk: Risk,
    regenerable: bool,
    reclaim: String,
    what: String,
    cost: String,
    recover: String,
) -> Rule {
    Rule {
        id,
        name,
        category,
        risk,
        regenerable,
        sudo: false,
        matcher: Matcher::Path { paths: vec![path] },
        reclaim,
        explain: Explain {
            what,
            cost,
            recover,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_and_parsers() {
        assert_eq!(slug_for("~/Library/Caches/Homebrew"), "custom-homebrew");
        assert_eq!(slug_for("/tmp/"), "custom-tmp");
        assert_eq!(parse_risk("SAFE"), Some(Risk::Safe));
        assert_eq!(parse_category("dev-cache"), Some(Category::DevCache));
        assert_eq!(parse_risk("nope"), None);
    }
}
