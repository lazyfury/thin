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
    /// 关联残留
    pub leftovers: Vec<Leftover>,
}

/// 一条关联残留
#[derive(Debug, Clone)]
pub struct Leftover {
    pub path: PathBuf,
    pub size: u64,
    /// 位于系统目录（/Library 等），删除需 root
    pub sudo: bool,
}

impl AppInfo {
    pub fn leftovers_size(&self) -> u64 {
        self.leftovers.iter().map(|l| l.size).sum()
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
                // 规范化大小写/符号链接，便于去重与显示
                app_paths.push(std::fs::canonicalize(&path).unwrap_or(path));
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
            let leftovers = find_leftovers(&name, bundle_id.as_deref(), &path);
            AppInfo {
                name,
                path,
                bundle_id,
                size,
                leftovers,
            }
        })
        .collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.total()));
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
    // NSWorkspace 权威判断：真·App 进程会命中
    if let Some(true) = crate::platform::platform().is_app_running(app) {
        return true;
    }
    // 补充：NSWorkspace 只认识注册为 App 的进程；脚本直接 exec 的同路径进程
    // 不在其中，用进程表再确认一次（保守地视为运行中）。
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
    // 优先 Swift `Bundle`，无后端/读不到时回退 `plutil`
    if let Some(id) = crate::platform::platform().bundle_id(app) {
        return Some(id);
    }
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

/// 少量常见 App 的目录名提示（bundle id 精确匹配）。
///
/// 有些 App 的数据目录名与显示名/ bundle id 无关（如 VS Code → `Code`、
/// Chrome → `Google`），这里做精确映射，避免用弱名称做竞配。
const APP_HINTS: &[(&str, &[&str])] = &[
    ("com.microsoft.VSCode", &["Code"]),
    ("com.microsoft.VSCodeInsiders", &["Code - Insiders"]),
    ("com.google.Chrome", &["Google"]),
    ("com.google.Chrome.canary", &["Google"]),
    ("com.brave.Browser", &["BraveSoftware", "Brave Browser"]),
    ("com.docker.docker", &["Docker"]),
    ("com.electron.docker-frontend", &["Docker Desktop"]),
    ("com.tinyspeck.slackmacgap", &["Slack"]),
    ("org.mozilla.firefox", &["Firefox"]),
    ("com.spotify.client", &["Spotify"]),
    ("com.hnc.Discord", &["discord"]),
];

/// 把名字归一化为 alnum 小写，用于目录名匹配
fn normalize_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 过于通用、不适合单独作为目录 token 的段（避免误配）
const GENERIC_TOKENS: &[&str] = &[
    "app", "apps", "com", "org", "net", "mac", "macos", "osx", "desktop", "client", "helper",
    "service", "main", "core", "pro", "lite",
];

/// 从显示名 / bundle id 推导可能的数据目录名。
///
/// 只用强证据：完整 bundle id、bundle 末段（非通用词）、显示名、归一化显示名，
/// 以及精确匹配的提示表。**不用** bundle 中间段（如 `google`）做竞配，
/// 避免误删其它同厂商 App 的数据。
fn name_tokens(name: &str, bundle_id: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let n = name.trim();
    if !n.is_empty() {
        v.push(n.to_string());
    }
    let norm = normalize_token(n);
    if norm.len() >= 3 {
        v.push(norm);
    }
    if let Some(b) = bundle_id {
        let bl = b.to_lowercase();
        v.push(bl.clone());
        if let Some(last) = bl.rsplit('.').next()
            && last.len() >= 3
            && !GENERIC_TOKENS.contains(&last)
        {
            v.push(last.to_string());
        }
        for (bid, hints) in APP_HINTS {
            if bid.eq_ignore_ascii_case(b) {
                for h in *hints {
                    v.push((*h).to_string());
                }
            }
        }
    }
    v.retain(|t| t.len() >= 3 && !t.chars().all(|c| c == '.'));
    v.sort();
    v.dedup();
    v
}

/// 生成所有候选残留路径（path, sudo）
fn candidate_paths(home: &Path, tokens: &[String]) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    let lib = home.join("Library");

    // 用户 Library 子目录
    for sub in [
        "Application Support",
        "Caches",
        "Logs",
        "Containers",
        "Application Scripts",
        "WebKit",
        "HTTPStorages",
        "Preferences",
        "LaunchAgents",
        "Saved Application State",
        "Group Containers",
    ] {
        for t in tokens {
            out.push((lib.join(sub).join(t), false));
        }
    }
    // 带后缀的常见形式
    for t in tokens {
        out.push((lib.join("Preferences").join(format!("{t}.plist")), false));
        out.push((lib.join("LaunchAgents").join(format!("{t}.plist")), false));
        out.push((
            lib.join("Saved Application State")
                .join(format!("{t}.savedState")),
            false,
        ));
        out.push((
            lib.join("Cookies").join(format!("{t}.binarycookies")),
            false,
        ));
    }

    // 主目录点目录 / XDG
    for base in [".config", ".cache", ".local/share", ".local/state"] {
        for t in tokens {
            out.push((home.join(base).join(t), false));
        }
    }
    for t in tokens {
        out.push((home.join(format!(".{t}")), false));
    }

    // 系统级 /Library（需 sudo）
    let sys = Path::new("/Library");
    for sub in [
        "Application Support",
        "Caches",
        "Logs",
        "Preferences",
        "LaunchAgents",
        "LaunchDaemons",
        "PrivilegedHelperTools",
        "Application Scripts",
    ] {
        for t in tokens {
            out.push((sys.join(sub).join(t), true));
        }
    }
    for t in tokens {
        out.push((sys.join("Preferences").join(format!("{t}.plist")), true));
        out.push((sys.join("LaunchAgents").join(format!("{t}.plist")), true));
        out.push((sys.join("LaunchDaemons").join(format!("{t}.plist")), true));
    }
    out
}

/// 查找某个 App 的关联残留。
///
/// 覆盖：用户 `~/Library` 各子目录、主目录点目录/XDG、以及系统级 `/Library`
/// （后者标记 `sudo=true`，安全门会跳过并提示手动处理）。仅返回真实存在的路径。
pub fn find_leftovers(name: &str, bundle_id: Option<&str>, app_path: &Path) -> Vec<Leftover> {
    let Ok(home) = std::env::var("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let tokens = name_tokens(name, bundle_id);
    let mut candidates = candidate_paths(&home, &tokens);

    // ByHost 偏好：<bundle>.<uuid>.plist
    if let Some(b) = bundle_id {
        let byhost = home.join("Library/Preferences/ByHost");
        if let Ok(rd) = std::fs::read_dir(&byhost) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(b) {
                    candidates.push((e.path(), false));
                }
            }
        }

        // Group Containers：目录名通常含 bundle 前缀（如 group.com.docker）。
        // 用“去掉末段的 bundle 前缀”做强匹配，避免泛词误配。
        let bl = b.to_lowercase();
        let prefix = bl
            .rsplit_once('.')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| bl.clone());
        let gc = home.join("Library/Group Containers");
        if let Ok(rd) = std::fs::read_dir(&gc) {
            for e in rd.flatten() {
                let fname = e.file_name().to_string_lossy().to_lowercase();
                if fname.contains(&bl) || (prefix.len() >= 6 && fname.contains(&prefix)) {
                    candidates.push((e.path(), false));
                }
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Leftover> = Vec::new();
    for (p, sudo) in candidates {
        if !p.exists() || p == app_path || p.starts_with(app_path) {
            continue;
        }
        // APFS 默认大小写不敏感：`Demo` 与 `demo` 可能是同一目录，
        // 用真实路径（大小写以磁盘为准）去重并存储，避免重复与错误大小写
        let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if !seen.insert(real.clone()) {
            continue;
        }
        out.push(Leftover {
            size: crate::fsutil::size_of(&real),
            path: real,
            sudo,
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.size));
    out
}

/// 匹配与 App 相关的安装包 receipt id（供 `sudo pkgutil --forget` 参考）。
pub fn pkg_receipt_ids(bundle_id: Option<&str>, name: &str) -> Vec<String> {
    let Some(out) =
        crate::proc::output_with_timeout("pkgutil", &["--pkgs"], Duration::from_secs(10))
    else {
        return Vec::new();
    };
    let norm = normalize_token(name);
    let mut matched: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|id| {
            let l = id.to_lowercase();
            if let Some(b) = bundle_id
                && l == b.to_lowercase()
            {
                return true;
            }
            // 名称匹配需要足够长，避免泛词误报
            norm.len() >= 5 && l.replace(['.', '-', '_'], "").contains(&norm)
        })
        .collect();
    matched.sort();
    matched.dedup();
    matched
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn leftovers_only_existing() {
        // 几乎不可能存在的 bundle id
        let v = find_leftovers(
            "DefinitelyNotInstalledXyz",
            Some("com.thin.definitely-not-installed-xyz"),
            Path::new("/Applications/DefinitelyNotInstalledXyz.app"),
        );
        assert!(v.is_empty());
    }

    #[test]
    fn tokens_are_strong_only() {
        let t = name_tokens("Docker", Some("com.docker.docker"));
        assert!(t.contains(&"docker".to_string()));
        assert!(t.contains(&"com.docker.docker".to_string()));
        assert!(t.contains(&"Docker".to_string()));
        // 不用 bundle 中间段（避免误删同厂商其它 App 数据）
        let g = name_tokens("Foo", Some("com.google.foo"));
        assert!(!g.contains(&"google".to_string()));
    }

    #[test]
    fn hints_add_directory_names() {
        let t = name_tokens("Visual Studio Code", Some("com.microsoft.VSCode"));
        assert!(t.contains(&"Code".to_string()));
        let c = name_tokens("Google Chrome", Some("com.google.Chrome"));
        assert!(c.contains(&"Google".to_string()));
    }

    #[test]
    fn candidates_cover_system_and_dotdirs() {
        let home = Path::new("/Users/x");
        let tokens = vec!["docker".to_string()];
        let c = candidate_paths(home, &tokens);
        // 用户点目录
        assert!(c.iter().any(|(p, _)| p == Path::new("/Users/x/.docker")));
        assert!(
            c.iter()
                .any(|(p, _)| p == Path::new("/Users/x/.config/docker"))
        );
        // 系统级 /Library（标记 sudo）
        assert!(
            c.iter()
                .any(|(p, s)| p == Path::new("/Library/Application Support/docker") && *s)
        );
        assert!(
            c.iter()
                .any(|(p, s)| p == Path::new("/Library/LaunchDaemons/docker.plist") && *s)
        );
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
