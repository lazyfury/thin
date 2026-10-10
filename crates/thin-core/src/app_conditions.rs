//! App 关联残留的**声明式条件表**（参考 Pearcleaner 的 Conditions）。
//!
//! 与清理规则同样采用「内置默认 + 用户覆盖」：
//! - 内置：`rules/app-leftovers.json`（编译期 `include_str!` 嵌入）；
//! - 用户：`~/.thin/app-leftovers.d/*.json`（每个文件为单条条件或条件数组），
//!   按 `bundleId` 覆盖内置条目。
//!
//! 用途：消除「厂商目录共享 / 同族 App」带来的歧义。
//! - `tokens`：补充目录名 token（原样用作目录名，等价于原先硬编码的 hints）；
//! - `aliases`：App 的别名 / 旧名（按显示名规则归一化，用于残留候选与 `--deep`
//!   强 token，也可作为 `thin uninstall <别名>` 的查询词）；
//! - `forcePaths`：精确补充候选路径（支持 `~` 展开）；
//! - `require`：候选（相对 home 的归一化路径）必须命中其中任一子串；
//! - `exclude`：候选命中任一子串即丢弃。
//!
//! 过滤器只作用于 token / forcePath 派生的候选；entitlements / 沙盒容器等
//! **权威来源**的候选不受影响，避免误伤。

use crate::clean;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 内置条件（编译期嵌入）
const DEFAULT_CONDITIONS: &str = include_str!("../rules/app-leftovers.json");

/// 单个 App 的残留匹配条件
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppCondition {
    /// 精确匹配的 bundle id（忽略大小写）
    pub bundle_id: String,
    /// 额外目录名 token（原样用作目录名）
    #[serde(default)]
    pub tokens: Vec<String>,
    /// App 的别名 / 旧名（按显示名规则归一化后参与残留匹配与 `uninstall` 查询）
    #[serde(default)]
    pub aliases: Vec<String>,
    /// 精确补充的候选路径（支持 `~` 展开）
    #[serde(default)]
    pub force_paths: Vec<String>,
    /// 命中任一子串（对相对 home 的归一化路径）则丢弃该候选
    #[serde(default)]
    pub exclude: Vec<String>,
    /// 非空时，候选必须命中其中任一子串（对相对 home 的归一化路径）
    #[serde(default)]
    pub require: Vec<String>,
    /// 可选说明（维护用）
    #[serde(default)]
    pub note: Option<String>,
}

/// 内置条件
pub fn builtin() -> Result<Vec<AppCondition>> {
    serde_json::from_str(DEFAULT_CONDITIONS).context("内置 App 残留条件解析失败")
}

/// 用户条件目录：`~/.thin/app-leftovers.d/`（一文件可含单条或数组）
pub fn user_dir_in(home: &Path) -> PathBuf {
    home.join("app-leftovers.d")
}

/// 读取用户目录下的所有条件（按文件名排序）
pub fn load_dir_in(dir: &Path) -> Result<Vec<AppCondition>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for f in files {
        let raw = std::fs::read_to_string(&f)
            .with_context(|| format!("读取 App 条件文件失败: {}", f.display()))?;
        if raw.trim().is_empty() {
            continue;
        }
        // 先按数组解析，再退回单条
        match serde_json::from_str::<Vec<AppCondition>>(&raw) {
            Ok(v) => out.extend(v),
            Err(_) => out.push(
                serde_json::from_str::<AppCondition>(&raw)
                    .with_context(|| format!("App 条件解析失败: {}", f.display()))?,
            ),
        }
    }
    Ok(out)
}

/// 合并：内置 → 用户（按 bundleId 覆盖）。解析失败时退回内置，保证卸载可用。
pub fn load() -> Vec<AppCondition> {
    load_in(&clean::thin_home())
}

pub fn load_in(home: &Path) -> Vec<AppCondition> {
    let mut conds = builtin().unwrap_or_default();
    for c in load_dir_in(&user_dir_in(home)).unwrap_or_default() {
        match conds
            .iter()
            .position(|x| x.bundle_id.eq_ignore_ascii_case(&c.bundle_id))
        {
            Some(pos) => conds[pos] = c,
            None => conds.push(c),
        }
    }
    conds
}

/// 进程内缓存：`bundleId`（小写）→ 条件。文件系统只读一次。
pub fn index() -> &'static HashMap<String, AppCondition> {
    static MAP: OnceLock<HashMap<String, AppCondition>> = OnceLock::new();
    MAP.get_or_init(|| {
        load()
            .into_iter()
            .map(|c| (c.bundle_id.to_lowercase(), c))
            .collect()
    })
}

/// 按 bundle id 查条件（忽略大小写）。
pub fn lookup(bundle_id: Option<&str>) -> Option<&'static AppCondition> {
    bundle_id.and_then(|b| index().get(&b.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses_and_has_unique_ids() {
        let c = builtin().unwrap();
        assert!(!c.is_empty());
        let mut seen = std::collections::HashSet::new();
        for cond in &c {
            assert!(
                seen.insert(cond.bundle_id.to_lowercase()),
                "重复 bundleId: {}",
                cond.bundle_id
            );
        }
    }

    #[test]
    fn vscode_conditions_are_distinct() {
        let c = builtin().unwrap();
        let stable = c
            .iter()
            .find(|x| x.bundle_id == "com.microsoft.VSCode")
            .unwrap();
        assert!(stable.tokens.contains(&"Code".to_string()));
        assert!(stable.exclude.contains(&"insiders".to_string()));

        let insiders = c
            .iter()
            .find(|x| x.bundle_id == "com.microsoft.VSCodeInsiders")
            .unwrap();
        assert!(insiders.tokens.contains(&"Code - Insiders".to_string()));
    }

    #[test]
    fn user_dir_overrides_builtin() {
        let base = std::env::temp_dir().join(format!("thin-cond-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = user_dir_in(&base);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("vscode.json"),
            r#"{"bundleId":"com.microsoft.VSCode","tokens":["Override"],"aliases":["VSCode","Visual Studio Code"]}"#,
        )
        .unwrap();

        let merged = load_in(&base);
        let vscode = merged
            .iter()
            .find(|x| x.bundle_id == "com.microsoft.VSCode")
            .unwrap();
        assert_eq!(vscode.tokens, vec!["Override".to_string()]);
        assert_eq!(
            vscode.aliases,
            vec!["VSCode".to_string(), "Visual Studio Code".to_string()]
        );

        // 未覆盖的其它条目仍在
        assert!(merged.iter().any(|x| x.bundle_id == "com.google.Chrome"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
