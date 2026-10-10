//! 安全门（SafetyGate）：受保护路径、卷隔离、敏感目录名判定。
//!
//! 这些判断与卷/挂载无关（卷隔离除外），供清理安全门与「归因 / 建规则预检」共用，
//! **必须与 [`crate::clean::plan`] 使用同一套**，否则会出现「扫描得到却清不掉」的矛盾。

use std::path::{Component, Path, PathBuf};

/// 树级保护（含子目录）
const DENY_SUBTREES: &[&str] = &[
    "/System",
    "/bin",
    "/sbin",
    "/usr",
    "/etc",
    "/private/etc",
    "/private/var/vm",
    "/private/var/db",
    "/private/var/protected",
    "/Library/Extensions",
    "/Library/Apple",
    "/Library/Keychains",
    "/Library/CloudStorage",
    "/dev",
    "/cores",
];

/// 可重建的系统缓存/日志：位于拒绝子树内，但允许清理（在拒绝清单之前判断）。
const ALLOW_SUBTREES: &[&str] = &[
    "/private/var/log",
    "/private/var/db/diagnostics",
    "/private/var/db/uuidtext",
    "/private/var/folders",
    "/private/tmp",
    "/usr/local",
    "/opt/homebrew",
    "/Library/Logs",
    "/Library/Caches",
];

/// 裸顶层根：即使子项可清理，也绝不删除这些目录**本身**（防止整目录被搬走）。
const BARE_ROOTS: &[&str] = &[
    "/Applications",
    "/Library",
    "/Library/Application Support",
    "/Library/Caches",
    "/Library/Logs",
    "/Volumes",
    "/opt",
    "/opt/homebrew",
    "/usr/local",
    "/Users",
    "/private",
    "/private/var",
    "/var",
];

/// 用户个人目录「顶层」：绝不整体搬走，但其内部的具体缓存/项目产物仍可清理。
///
/// 与 [`DENY_SUBTREES`] 不同，这里只保护目录**本身**（精确匹配），不保护其子树，
/// 因此`~/Documents/proj/target` 这类仍可被规则命中，而 `~/Documents` 本身不会被清空。
const HOME_BARE_ROOTS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Library",
    "Movies",
    "Music",
    "Pictures",
    "Public",
];

/// 用户主目录（`HOME`）。
pub(super) fn user_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// 解析路径所在卷的设备号；路径不存在时向上找到最近的已存在祖先。
fn volume_device(path: &Path) -> Option<u64> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if let Ok(md) = std::fs::metadata(p) {
            use std::os::unix::fs::MetadataExt;
            return Some(md.dev());
        }
        cur = p.parent();
    }
    None
}

/// 若路径受保护，返回原因。受保护路径永不被移动/删除。
///
/// `ref_vol` 为隔离区所在卷的参考路径：与它不处于同一卷的目标（外接盘、其他挂载）
/// 一律拒绝，避免跨卷复制带来的双倍空间占用与半成品数据。
pub fn protection_reason_in(path: &Path, ref_vol: Option<&Path>) -> Option<String> {
    if let Some(reason) = static_protection_reason(path) {
        return Some(reason);
    }

    // 卷隔离：目标必须与隔离区同卷，否则拒绝（外接盘 / 其他挂载）
    // 注：静态校验不涉及卷，故这里单独判断，避免 discover 在任意卷上误报。
    if let Some(vol) = ref_vol {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let (Some(a), Some(b)) = (volume_device(&canon), volume_device(vol))
            && a != b
        {
            return Some(format!(
                "位于不同卷（外接盘/其他挂载），不在隔离区所在卷 {}",
                vol.display()
            ));
        }
    }
    None
}

/// 与卷/挂载无关的静态保护判断：根目录、主目录、隐私目录、个人目录顶层、
/// 拒绝子树与裸顶层根、挂载点。供清理安全门与「归因 / 建规则预检」共用。
pub fn static_protection_reason(path: &Path) -> Option<String> {
    // 基础校验：空路径 / 控制字符 / `..` 组件
    if path.as_os_str().is_empty() {
        return Some("空路径".into());
    }
    if path.to_string_lossy().chars().any(|c| c.is_control()) {
        return Some("路径含控制字符".into());
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Some("路径包含 .. 组件".into());
    }

    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let home = user_home();

    // 精确匹配：根目录与整个主目录
    if canon == Path::new("/") {
        return Some("根目录".into());
    }
    if let Some(h) = &home
        && canon == *h
    {
        return Some("整个用户主目录".into());
    }

    // 用户隐私目录（始终保护，先于 allow 判断）
    if let Some(h) = &home {
        for sub in [
            "Library/Keychains",
            "Library/Mobile Documents",
            "Library/CloudStorage",
            ".thin",
        ] {
            let p = h.join(sub);
            if canon == p || canon.starts_with(&p) {
                return Some(format!("受保护路径 {}", p.display()));
            }
        }
        // 个人目录顶层：只保护目录本身，允许清理其内部的具体缓存/项目产物
        for sub in HOME_BARE_ROOTS {
            if canon == h.join(sub) {
                return Some(format!("个人目录顶层，禁止整体清理 {}", canon.display()));
            }
        }
    }

    // 其他用户主目录（/Users/<name> 本身）
    if let Ok(rest) = canon.strip_prefix("/Users")
        && rest.components().count() == 1
    {
        return Some("用户主目录".into());
    }

    // 裸顶层根：即使其子项可清理，也绝不删除这些目录**本身**。
    // 注意：必须在允许清单之前**独立**判断——否则 `/Library/Logs`、`/usr/local`
    // 这些同时出现在 ALLOW_SUBTREES 里的裸根会被 `allowed` 绕过而整目录被搬走。
    for s in BARE_ROOTS {
        if canon == Path::new(s) {
            return Some(format!("禁止删除顶层目录 {s}"));
        }
    }

    // 拒绝子树（允许清单内的可重建缓存/日志例外，例如 /private/var/log）
    let allowed = ALLOW_SUBTREES.iter().any(|s| {
        let p = Path::new(s);
        canon == p || canon.starts_with(p)
    });
    if !allowed {
        for s in DENY_SUBTREES {
            let p = Path::new(s);
            if canon == p || canon.starts_with(p) {
                return Some(format!("受保护路径 {s}"));
            }
        }
    }

    // 挂载点保护（路径自身是挂载点，与父目录设备号不同）
    if let Some(parent) = canon.parent()
        && let (Ok(a), Ok(b)) = (std::fs::metadata(&canon), std::fs::metadata(parent))
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() != b.dev() {
            return Some("是挂载点".into());
        }
    }
    None
}

/// 敏感目录名（用于 `findDir` 规则预检）。
///
/// 这些名字的目录本身是裸顶层根或用户个人目录；按名字“发现并整体清理”它们
/// 风险极高（例如 `dirName = "Documents"`）。返回一句人类可读的说明。
pub fn is_sensitive_dir_name(name: &str) -> Option<&'static str> {
    const SENSITIVE: &[(&str, &str)] = &[
        ("Library", "系统/用户库目录"),
        ("Documents", "用户文稿"),
        ("Downloads", "下载目录"),
        ("Desktop", "桌面"),
        ("Movies", "影片"),
        ("Music", "音乐"),
        ("Pictures", "图片"),
        ("Public", "公共目录"),
        ("Applications", "应用程序目录"),
        ("System", "系统目录"),
        ("Users", "用户目录"),
        ("private", "系统私有目录"),
        ("var", "系统变量目录"),
        ("etc", "系统配置目录"),
        ("usr", "系统目录"),
        ("opt", "系统目录"),
        ("Volumes", "挂载卷目录"),
    ];
    SENSITIVE.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
}

/// 若路径受保护，返回原因（以用户主目录作为隔离区参考卷）
pub fn protection_reason(path: &Path) -> Option<String> {
    let home = user_home();
    protection_reason_in(path, home.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 防止 `HOME_BARE_ROOTS` 与 `is_sensitive_dir_name` 的名单漂移：
    /// 后者用于 `findDir` 规则预检，必须覆盖前者，否则可能放过「按名字发现
    /// 并整体清理用户个人目录」的危险规则。
    #[test]
    fn sensitive_names_cover_home_bare_roots() {
        for name in HOME_BARE_ROOTS {
            assert!(
                is_sensitive_dir_name(name).is_some(),
                "HOME_BARE_ROOTS 的 {name} 未出现在 is_sensitive_dir_name 名单中"
            );
        }
    }
}
