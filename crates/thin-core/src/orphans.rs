//! 已卸载 App 的孤立残留检测（App 本体已不存在，缓存/容器/偏好仍在）。
//!
//! 只覆盖「条目名（去掉固定后缀后）基本就是 bundle id」的**高置信**来源：
//! `Containers`、`Application Scripts`、`HTTPStorages`、`WebKit`、`Preferences`、
//! `Saved Application State`、`Caches`、`Logs`、`Application Support` 以及
//! Darwin 每用户缓存 `C/`。App 名/厂商目录等中置信来源不在默认范围内
//! （宁可漏，不可错）；`Group Containers` 需要 entitlements/team id 才能归位，
//! 同样留待后续。
//!
//! 判定规则：条目名像反向域名（>=3 段、末段非纯数字），
//! 且不在「已安装 bundle id 家族」内（含 `.helper` 这类前缀变体），
//! 且非系统 id（`com.apple.*`）。清理仍走 [`crate::clean::plan`] 安全门。

use crate::apps::{self, Leftover};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 一个已卸载 App 的孤立残留（按 bundle id 聚合）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanApp {
    pub bundle_id: String,
    pub leftovers: Vec<Leftover>,
}

impl OrphanApp {
    pub fn total(&self) -> u64 {
        self.leftovers.iter().map(|l| l.size).sum()
    }
}

/// 单个高置信来源：目录 + 条目名需要去掉的后缀。
struct BundleRoot {
    dir: PathBuf,
    strip_suffix: Option<&'static str>,
}

/// 扫描全部高置信来源，返回按总占用降序的孤立残留。
pub fn find_orphans() -> Vec<OrphanApp> {
    let Ok(home) = std::env::var("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let installed = apps::installed_bundle_ids();
    let mut map: HashMap<String, (String, Vec<Leftover>)> = HashMap::new();

    for root in bundle_roots(&home) {
        collect_root(&root, &installed, &mut map);
    }
    for dir in darwin_cache_dirs() {
        collect_root(
            &BundleRoot {
                dir,
                strip_suffix: None,
            },
            &installed,
            &mut map,
        );
    }

    finalize(map)
}

/// 用户级高置信来源（`~/Library/**`）。
fn bundle_roots(home: &Path) -> Vec<BundleRoot> {
    let lib = home.join("Library");
    let mut out = Vec::new();
    for sub in [
        "Containers",
        "Application Scripts",
        "HTTPStorages",
        "WebKit",
        "Preferences",
        "Saved Application State",
        "Caches",
        "Logs",
        "Application Support",
    ] {
        let strip_suffix = match sub {
            "Preferences" => Some(".plist"),
            "Saved Application State" => Some(".savedState"),
            _ => None,
        };
        out.push(BundleRoot {
            dir: lib.join(sub),
            strip_suffix,
        });
    }
    out
}

/// Darwin 每用户缓存目录（`/private/var/folders/<随机>/C`）；解析失败返回空。
fn darwin_cache_dirs() -> Vec<PathBuf> {
    match crate::apps::darwin_user_cache_dir() {
        Some(p) if p.is_dir() => vec![p],
        _ => Vec::new(),
    }
}

/// 枚举一个来源下的条目，把 bundle-id 形态且未安装的记入 `map`。
fn collect_root(
    root: &BundleRoot,
    installed: &HashSet<String>,
    map: &mut HashMap<String, (String, Vec<Leftover>)>,
) {
    let Ok(rd) = std::fs::read_dir(&root.dir) else {
        return;
    };
    for e in rd.flatten() {
        let raw = e.file_name().to_string_lossy().to_string();
        let name = match root.strip_suffix {
            Some(sfx) => match raw.strip_suffix(sfx) {
                Some(n) => n,
                None => continue,
            },
            None => raw.as_str(),
        };
        let Some(id) = bundle_like(name) else {
            continue;
        };
        // `Application Scripts` 里常混有 group id（`TEAMID.group.*`）；
        // 无 entitlements 无法归位，一律不当孤儿（宁可漏）。
        if is_system_id(id) || is_group_id(id) || is_installed_family(id, installed) {
            continue;
        }
        let path = std::fs::canonicalize(e.path()).unwrap_or_else(|_| e.path());
        if crate::clean::static_protection_reason(&path).is_some() {
            continue;
        }
        let size = crate::fsutil::size_of(&path);
        let entry = map
            .entry(id.to_lowercase())
            .or_insert_with(|| (id.to_string(), Vec::new()));
        entry.1.push(Leftover {
            path,
            size,
            sudo: false,
        });
    }
}

/// 条目名是否像反向域名 bundle id（>=3 段、只含安全字符、末段非纯数字）。
fn bundle_like(name: &str) -> Option<&str> {
    if name.len() < 5 || name.split('.').count() < 3 {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return None;
    }
    let last = name.rsplit('.').next().unwrap_or("");
    if last.is_empty() || last.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(name)
}

/// 系统 id（无对应 .app 的 daemon/框架海量存在），一律不视为孤儿。
fn is_system_id(id: &str) -> bool {
    let l = id.to_lowercase();
    l == "com.apple" || l.starts_with("com.apple.")
}

/// 形如 `group.*` / `<TEAMID>.group.*` 的 group id：需要 entitlements 才能归位，跳过。
fn is_group_id(id: &str) -> bool {
    let l = id.to_lowercase();
    l.starts_with("group.") || l.contains(".group.")
}

/// `id` 是否属于某个已安装 App 的家族（本体、`.helper`、前缀变体）。
fn is_installed_family(id: &str, installed: &HashSet<String>) -> bool {
    let idl = id.to_lowercase();
    installed.iter().any(|b| {
        idl == *b || idl.starts_with(&format!("{b}.")) || b.starts_with(&format!("{idl}."))
    })
}

/// 合并嵌套/重复路径，按体积排序并丢弃空项。
fn finalize(map: HashMap<String, (String, Vec<Leftover>)>) -> Vec<OrphanApp> {
    let mut out: Vec<OrphanApp> = map
        .into_values()
        .map(|(bundle_id, mut ls)| {
            ls.sort_by(|a, b| {
                a.path
                    .components()
                    .count()
                    .cmp(&b.path.components().count())
                    .then_with(|| a.path.cmp(&b.path))
            });
            let mut kept: Vec<Leftover> = Vec::new();
            for l in ls {
                if kept
                    .iter()
                    .any(|k| l.path == k.path || l.path.starts_with(&k.path))
                {
                    continue;
                }
                kept.push(l);
            }
            kept.sort_by_key(|l| std::cmp::Reverse(l.size));
            OrphanApp {
                bundle_id,
                leftovers: kept,
            }
        })
        .filter(|o| !o.leftovers.is_empty())
        .collect();
    out.sort_by_key(|o| std::cmp::Reverse(o.total()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_like_accepts_reverse_dns_only() {
        assert_eq!(bundle_like("com.foo.bar"), Some("com.foo.bar"));
        assert_eq!(
            bundle_like("com.foo.bar.helper"),
            Some("com.foo.bar.helper")
        );
        assert_eq!(bundle_like("Google"), None); // 单段厂商目录
        assert_eq!(bundle_like("foo.bar"), None); // 段数不足
        assert_eq!(bundle_like("com.foo.123"), None); // 末段纯数字（版本）
        assert_eq!(bundle_like("com.foo.有中文"), None);
    }

    #[test]
    fn installed_family_covers_helpers_and_apple() {
        let installed: HashSet<String> = ["com.foo.bar".to_string()].into_iter().collect();
        assert!(is_installed_family("com.foo.bar", &installed));
        assert!(is_installed_family("com.foo.bar.helper", &installed));
        assert!(is_installed_family("com.foo", &installed)); // 前缀变体
        assert!(!is_installed_family("com.foo.baz", &installed));
        assert!(is_system_id("com.apple.Safari"));
        assert!(!is_system_id("com.appleizer.foo"));
        assert!(is_group_id("group.com.foo"));
        assert!(is_group_id("4K6FWZU8C4.group.cn.better365"));
        assert!(!is_group_id("com.foo.groupie"));
    }

    #[test]
    fn finalize_dedupes_nested_paths() {
        let mut map: HashMap<String, (String, Vec<Leftover>)> = HashMap::new();
        map.insert(
            "com.foo.bar".into(),
            (
                "com.foo.bar".into(),
                vec![
                    Leftover {
                        path: PathBuf::from("/x/com.foo.bar"),
                        size: 100,
                        sudo: false,
                    },
                    Leftover {
                        path: PathBuf::from("/x/com.foo.bar/Cache"),
                        size: 40,
                        sudo: false,
                    },
                ],
            ),
        );
        let out = finalize(map);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].total(), 100); // 子项被父项覆盖
    }
}
