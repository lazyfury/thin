//! App 列表与卸载（含关联残留）（M2）。
//!
//! 拆分（见 `docs/thin-fs-plan.md` §9）：
//! - 本模块：App 列表 / 运行态 / 系统保护 / bundle id；
//! - [`tokens`]：App 名与 Bundle 内部结构 → 候选目录 token；
//! - [`leftovers`]：候选生成、条件过滤、落盘与深扫。

use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod leftovers;
mod tokens;

pub use leftovers::{darwin_user_cache_dir, find_leftovers, find_leftovers_deep, pkg_receipt_ids};

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub size: u64,
    /// 当前是否在运行（运行中的 App 不允许卸载）
    pub running: bool,
    /// 关联残留
    pub leftovers: Vec<Leftover>,
}

/// 一条关联残留
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
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

/// 已安装 App 的 bundle id 集合（小写），含系统 App，用于孤儿残留判定。
///
/// 与 [`list_apps`] 不同：只看 bundle id、不算体积/残留，因此很轻。
/// 必须包含 `/System/Applications`，否则系统 App 的容器会被误判为孤儿。
pub fn installed_bundle_ids() -> HashSet<String> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    if let Ok(h) = std::env::var("HOME") {
        roots.push(PathBuf::from(h).join("Applications"));
    }
    let mut ids = HashSet::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some("app") {
                if let Some(b) = bundle_id(&path) {
                    ids.insert(b.to_lowercase());
                }
                // 登录项 / 扩展等嵌套 bundle 也是该 App 合法的一部分，
                // 否则它们的容器会被误判为孤儿残留。
                collect_nested_bundle_ids(&path.join("Contents"), &mut ids, 5);
            }
        }
    }
    ids
}

/// 递归收集 App 内部嵌套 bundle（`.app`/`.xpc`/`.appex`）的 bundle id。
///
/// 只沿已知容器目录下钻，限定深度，避免遍历整个资源树。
fn collect_nested_bundle_ids(dir: &Path, ids: &mut HashSet<String>, depth: u8) {
    const NESTED_DIRS: &[&str] = &[
        "Library",
        "LoginItems",
        "XPCServices",
        "PlugIns",
        "Helpers",
        "SystemExtensions",
    ];
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if (name.ends_with(".app") || name.ends_with(".xpc") || name.ends_with(".appex"))
            && let Some(b) = bundle_id(&path)
        {
            ids.insert(b.to_lowercase());
        }
        if path.is_dir() && NESTED_DIRS.contains(&name.as_ref()) {
            collect_nested_bundle_ids(&path, ids, depth - 1);
        }
    }
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
            let running = is_running(&path);
            let leftovers = find_leftovers(&name, bundle_id.as_deref(), &path);
            AppInfo {
                name,
                path,
                bundle_id,
                size,
                running,
                leftovers,
            }
        })
        .collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.total()));
    apps
}

/// 按名字模糊匹配 App（不区分大小写，也认条件表里的别名）
pub fn find_app(query: &str) -> Vec<AppInfo> {
    let q = query.to_lowercase();
    list_apps()
        .into_iter()
        .filter(|a| {
            a.name.to_lowercase().contains(&q)
                || crate::app_conditions::lookup(a.bundle_id.as_deref())
                    .is_some_and(|c| c.aliases.iter().any(|al| al.to_lowercase().contains(&q)))
        })
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

/// 退出正在运行的 App（先优雅退出，超时后按可执行文件路径强制结束）。
///
/// 只结束该 `.app` 自身的进程，不做任何文件删除；卸载仍走废纸篓/隔离区。
/// 返回 `Ok` 表示已确认不再运行，`Err` 表示仍在运行（调用方应取消卸载）。
pub fn kill_app(app: &Path) -> anyhow::Result<()> {
    let name = app.file_stem().and_then(|s| s.to_str()).unwrap_or("该 App");

    // 1) 真·GUI App（NSWorkspace 已注册）优先请求优雅退出，让它保存状态。
    //    脚本直接 exec 的同路径进程不在此列，直接强制结束，避免无谓的 TCC 弹窗。
    let is_gui = crate::platform::platform()
        .is_app_running(app)
        .unwrap_or(false);
    if is_gui && let Some(stem) = app.file_stem().and_then(|s| s.to_str()) {
        let script = format!("tell application \"{stem}\" to quit");
        let _ =
            crate::proc::output_with_timeout("osascript", &["-e", &script], Duration::from_secs(5));
        if wait_until_stopped(app, Duration::from_secs(2)) {
            return Ok(());
        }
    }

    // 2) 强制结束：只匹配该 App 的可执行目录，避免误杀同名进程
    let needle = app.join("Contents/MacOS");
    let pattern = format!("{}/", needle.to_string_lossy());
    let _ = crate::proc::output_with_timeout("pkill", &["-f", &pattern], Duration::from_secs(3));
    if wait_until_stopped(app, Duration::from_secs(2)) {
        return Ok(());
    }

    anyhow::bail!("无法退出 {name}，请手动退出后再试")
}

/// 轮询等待 App 停止（`is_running` 探测失败时保守视为仍在运行）。
fn wait_until_stopped(app: &Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_running(app) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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

// 常见 App 的目录名提示已迁移到声明式条件表：`rules/app-leftovers.json`
// （见 [`crate::app_conditions`]），用户可在 `~/.thin/app-leftovers.d/*.json` 覆盖。

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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

    #[test]
    #[cfg(target_os = "macos")]
    fn kill_app_terminates_running_process() {
        let app = std::env::temp_dir().join(format!("thin-kill-{}.app", std::process::id()));
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(format!("exec -a '{}/Foo' sleep 30", macos.display()))
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(is_running(&app), "应检测到正在运行的 App");

        kill_app(&app).expect("应能退出 App");
        assert!(!is_running(&app), "退出后不应再检测到运行");

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&app);
    }
}
