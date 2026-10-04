//! 清理预设（Preset）。
//!
//! 预设是一组「清理什么」的声明：类别、风险、是否可再生、显式包含/排除的规则 id，
//! 以及隔离保留天数。定时任务**只允许**执行用户自定义的预设，内置默认预设仅用于
//! 手动 `thin clean --preset default`，且只覆盖 cache 类（系统/应用/开发缓存）。

use crate::clean;
use crate::model::{Category, CleanItem, Risk};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_risks() -> Vec<Risk> {
    vec![Risk::Safe]
}

fn default_purge_days() -> u64 {
    7
}

/// 一条清理预设
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub id: String,
    pub name: String,
    /// 选中的类别；为空表示不按类别过滤
    #[serde(default)]
    pub categories: Vec<Category>,
    /// 允许的风险等级（默认仅 safe）
    #[serde(default = "default_risks")]
    pub risks: Vec<Risk>,
    /// 只处理可再生项
    #[serde(default)]
    pub regenerable_only: bool,
    /// 显式指定的规则 id（非空时忽略类别过滤，仅处理这些规则）
    #[serde(default)]
    pub include_ids: Vec<String>,
    /// 显式排除的规则 id
    #[serde(default)]
    pub exclude_ids: Vec<String>,
    /// 隔离内容保留天数（定时任务先 purge 早于该天数的会话）
    #[serde(default = "default_purge_days")]
    pub purge_after_days: u64,
}

impl Preset {
    /// 内置默认预设：只处理 cache 类型、仅 safe、仅可再生。
    pub fn builtin_default() -> Self {
        Preset {
            id: "default".to_string(),
            name: "默认缓存清理".to_string(),
            categories: vec![
                Category::SystemCache,
                Category::AppCache,
                Category::DevCache,
            ],
            risks: vec![Risk::Safe],
            regenerable_only: true,
            include_ids: Vec::new(),
            exclude_ids: Vec::new(),
            purge_after_days: 7,
        }
    }

    /// 新建预设的推荐默认值（只处理 cache），用于 `thin preset add` 未指定时。
    pub fn new_cache_only(id: String, name: String) -> Self {
        Preset {
            id,
            name,
            ..Preset::builtin_default()
        }
    }
}

/// 用户预设文件路径
pub fn presets_path() -> PathBuf {
    clean::thin_home().join("presets.json")
}

/// 读取用户预设（不存在则为空）
pub fn load() -> Result<Vec<Preset>> {
    let path = presets_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取预设失败: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).with_context(|| format!("预设解析失败: {}", path.display()))
}

/// 写入用户预设文件
pub fn save(presets: &[Preset]) -> Result<PathBuf> {
    let path = presets_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(presets)?)?;
    Ok(path)
}

/// 新增/覆盖一条用户预设（按 id）
pub fn upsert(preset: Preset) -> Result<PathBuf> {
    let mut list = load()?;
    list.retain(|p| p.id != preset.id);
    list.push(preset);
    save(&list)
}

/// 删除一条用户预设
pub fn remove(id: &str) -> Result<bool> {
    let mut list = load()?;
    let before = list.len();
    list.retain(|p| p.id != id);
    if list.len() == before {
        return Ok(false);
    }
    save(&list)?;
    Ok(true)
}

/// 用户预设是否已定义
pub fn is_user_defined(id: &str) -> bool {
    load()
        .map(|l| l.iter().any(|p| p.id == id))
        .unwrap_or(false)
}

/// 按 id 取预设：用户预设优先，其次内置默认
pub fn get(id: &str) -> Option<Preset> {
    if let Ok(list) = load() {
        if let Some(p) = list.into_iter().find(|p| p.id == id) {
            return Some(p);
        }
    }
    if id == "default" {
        return Some(Preset::builtin_default());
    }
    None
}

/// 预设是否命中某条清理项
pub fn matches(preset: &Preset, item: &CleanItem) -> bool {
    if preset.exclude_ids.iter().any(|id| id == &item.rule_id) {
        return false;
    }
    if !preset.include_ids.is_empty() {
        return preset.include_ids.iter().any(|id| id == &item.rule_id);
    }
    if !preset.categories.is_empty() && !preset.categories.contains(&item.category) {
        return false;
    }
    if !preset.risks.contains(&item.risk) {
        return false;
    }
    if preset.regenerable_only && !item.regenerable {
        return false;
    }
    true
}

/// 按预设筛选清理项（已去重嵌套），保持输入顺序
pub fn select(preset: &Preset, items: &[CleanItem]) -> Vec<CleanItem> {
    let picked: Vec<CleanItem> = items
        .iter()
        .filter(|it| matches(preset, it))
        .cloned()
        .collect();
    crate::scan::top_level(&picked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Explain;
    use std::path::PathBuf;

    fn item(rule: &str, cat: Category, risk: Risk, regen: bool) -> CleanItem {
        CleanItem {
            rule_id: rule.into(),
            name: rule.into(),
            path: PathBuf::from(format!("/tmp/{rule}")),
            category: cat,
            risk,
            regenerable: regen,
            sudo: false,
            size: 100,
            reclaim: String::new(),
            explain: Explain {
                what: String::new(),
                cost: String::new(),
                recover: String::new(),
            },
        }
    }

    #[test]
    fn default_is_cache_only_and_safe() {
        let p = Preset::builtin_default();
        assert!(matches!(
            p.categories.as_slice(),
            [
                Category::SystemCache,
                Category::AppCache,
                Category::DevCache
            ]
        ));
        assert_eq!(p.risks, vec![Risk::Safe]);
        assert!(p.regenerable_only);

        assert!(matches(
            &p,
            &item("a", Category::AppCache, Risk::Safe, true)
        ));
        // 非 cache 类别不选
        assert!(!matches(&p, &item("b", Category::Vm, Risk::Safe, true)));
        // 不可再生不选
        assert!(!matches(
            &p,
            &item("c", Category::DevCache, Risk::Safe, false)
        ));
        // 非 safe 不选
        assert!(!matches(
            &p,
            &item("d", Category::DevCache, Risk::Confirm, true)
        ));
    }

    #[test]
    fn include_and_exclude_override() {
        let mut p = Preset::new_cache_only("x".into(), "x".into());
        p.include_ids = vec!["only".into()];
        assert!(matches(
            &p,
            &item("only", Category::Other, Risk::Confirm, false)
        ));
        assert!(!matches(
            &p,
            &item("other", Category::AppCache, Risk::Safe, true)
        ));

        p.include_ids.clear();
        p.exclude_ids = vec!["skip".into()];
        assert!(!matches(
            &p,
            &item("skip", Category::AppCache, Risk::Safe, true)
        ));
        assert!(matches(
            &p,
            &item("keep", Category::AppCache, Risk::Safe, true)
        ));
    }
}
