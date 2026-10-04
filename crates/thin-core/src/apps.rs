//! App 列表与卸载（含关联残留）（M2）。

use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub size: u64,
    /// 关联残留：(路径, 大小)
    pub leftovers: Vec<(PathBuf, u64)>,
}

impl AppInfo {
    pub fn leftovers_size(&self) -> u64 {
        self.leftovers.iter().map(|(_, s)| *s).sum()
    }

    pub fn total(&self) -> u64 {
        self.size.saturating_add(self.leftovers_size())
    }
}

/// 体积分级
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Large,
    Medium,
    Small,
}

impl Tier {
    pub fn label(&self) -> &'static str {
        match self {
            Tier::Large => "大",
            Tier::Medium => "中",
            Tier::Small => "小",
        }
    }
}

/// 按总占用分级：>=1GB 大，>=100MB 中，其余小
pub fn tier(bytes: u64) -> Tier {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB100: u64 = 100 * 1024 * 1024;
    if bytes >= GB {
        Tier::Large
    } else if bytes >= MB100 {
        Tier::Medium
    } else {
        Tier::Small
    }
}

fn app_roots() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/Applications")];
    if let Ok(h) = std::env::var("HOME") {
        v.push(PathBuf::from(h).join("Applications"));
    }
    v
}

/// 列出已安装的 App（按总占用降序）
pub fn list_apps() -> Vec<AppInfo> {
    // 先收集所有 .app 路径，再并行统计体积（含 plutil 子进程与递归目录）
    let mut app_paths: Vec<PathBuf> = Vec::new();
    for root in app_roots() {
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("app") {
                app_paths.push(path);
            }
        }
    }

    let mut apps: Vec<AppInfo> = app_paths
        .into_par_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let size = crate::fsutil::dir_size(&path);
            let bundle_id = bundle_id(&path);
            let leftovers: Vec<(PathBuf, u64)> = bundle_id
                .as_deref()
                .map(find_leftovers)
                .unwrap_or_default()
                .into_iter()
                .map(|p| {
                    let s = crate::fsutil::size_of(&p);
                    (p, s)
                })
                .collect();
            AppInfo {
                name,
                path,
                bundle_id,
                size,
                leftovers,
            }
        })
        .collect();
    apps.sort_by(|a, b| b.total().cmp(&a.total()));
    apps
}

/// 按名字模糊匹配 App（不区分大小写）
pub fn find_app(query: &str) -> Vec<AppInfo> {
    let q = query.to_lowercase();
    list_apps()
        .into_iter()
        .filter(|a| a.name.to_lowercase().contains(&q))
        .collect()
}

/// 判断某个 .app 当前是否在运行（其可执行文件位于 Contents/MacOS）。
///
/// 探测失败或超时视为「无法确认」，保守地当作运行中（拒绝卸载）。
pub fn is_running(app: &Path) -> bool {
    let needle = app.join("Contents/MacOS");
    let pattern = format!("{}/", needle.to_string_lossy());
    crate::proc::output_with_timeout("pgrep", &["-f", &pattern], Duration::from_secs(2))
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(true)
}

/// 系统关键 App（不可卸载）。
///
/// 刻意用显式列表而非 `com.apple.*` 通配：后者会连带阻止用户自行安装的
/// Apple 应用（Xcode、Final Cut Pro 等）被卸载。
const SYSTEM_CRITICAL_BUNDLES: &[&str] = &[
    "com.apple.finder",
    "com.apple.dock",
    "com.apple.Safari",
    "com.apple.mail",
    "com.apple.systempreferences",
    "com.apple.SystemSettings",
    "com.apple.controlcenter",
    "com.apple.Spotlight",
    "com.apple.loginwindow",
    "com.apple.Preview",
    "com.apple.TextEdit",
    "com.apple.Notes",
    "com.apple.iCal",
    "com.apple.AddressBook",
    "com.apple.Photos",
    "com.apple.AppStore",
    "com.apple.Terminal",
    "com.apple.ActivityMonitor",
    "com.apple.DiskUtility",
    "com.apple.KeychainAccess",
];

/// 是否是不可卸载的系统 App。
///
/// 读不到 bundle id 时保守地视为受保护（宁可拒绝，不可误卸）。
pub fn is_system_protected(bundle_id: Option<&str>) -> bool {
    match bundle_id {
        Some(id) => SYSTEM_CRITICAL_BUNDLES
            .iter()
            .any(|p| id == *p || id.starts_with(&format!("{p}."))),
        None => true,
    }
}

/// 读取 .app 的 CFBundleIdentifier
pub fn bundle_id(app: &Path) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    let plist_s = plist.to_string_lossy().into_owned();
    let out = crate::proc::output_with_timeout(
        "plutil",
        &["-extract", "CFBundleIdentifier", "raw", "-o", "-", &plist_s],
        Duration::from_secs(5),
    )?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// 查找某个 bundle id 在用户库中的关联残留
pub fn find_leftovers(bundle_id: &str) -> Vec<PathBuf> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return Vec::new(),
    };
    let lib = home.join("Library");
    let candidates = [
        lib.join("Application Support").join(bundle_id),
        lib.join("Caches").join(bundle_id),
        lib.join("Logs").join(bundle_id),
        lib.join("Containers").join(bundle_id),
        lib.join("WebKit").join(bundle_id),
        lib.join("HTTPStorages").join(bundle_id),
        lib.join("Saved Application State")
            .join(format!("{bundle_id}.savedState")),
        lib.join("Preferences").join(format!("{bundle_id}.plist")),
    ];
    candidates.into_iter().filter(|p| p.exists()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn leftovers_only_existing() {
        // 几乎不可能存在的 bundle id
        let v = find_leftovers("com.thin.definitely-not-installed-xyz");
        assert!(v.is_empty());
    }

    #[test]
    fn tier_thresholds() {
        assert_eq!(tier(2 * 1024 * 1024 * 1024), Tier::Large);
        assert_eq!(tier(200 * 1024 * 1024), Tier::Medium);
        assert_eq!(tier(50 * 1024 * 1024), Tier::Small);
    }

    #[test]
    fn system_apps_protected() {
        assert!(is_system_protected(Some("com.apple.Safari")));
        assert!(is_system_protected(Some("com.apple.SystemSettings")));
        assert!(is_system_protected(None), "未知 bundle id 保守拒绝");
        // 用户自行安装的 Apple 应用仍可卸载
        assert!(!is_system_protected(Some("com.apple.dt.Xcode")));
        assert!(!is_system_protected(Some("com.apple.FinalCut")));
        assert!(!is_system_protected(Some("com.example.app")));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn detects_running_app() {
        let app = std::env::temp_dir().join(format!("thin-run-{}.app", std::process::id()));
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(format!("exec -a '{}/Foo' sleep 5", macos.display()))
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(is_running(&app), "应检测到正在运行的 App");

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&app);
    }
}
