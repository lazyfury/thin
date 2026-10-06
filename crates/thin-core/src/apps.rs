//! App 列表与卸载（含关联残留）（M2）。

use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

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

/// Bundle 内不应作为匹配 token 的通用二进制名（避免误配）。
const GENERIC_BINARY_TOKENS: &[&str] = &[
    "crashhandler",
    "crash handler",
    "electron",
    "helper",
    "updater",
    "uninstaller",
    "installer",
    "launcher",
    "service",
    "agent",
    "daemon",
    "plugin",
    "framework",
    "renderer",
    "gpu",
    "utility",
    "main",
    "app",
    "node",
    "python",
    "java",
];

/// 加入一个来自文件名 / 可执行名的 token（原始名 + 归一化名）。
///
/// 只接受足够长（>=5）且非通用词的名字，避免用 `Helper`、`Electron` 之类误配。
fn push_token(out: &mut Vec<String>, raw: &str) {
    let s = raw.trim();
    if s.len() < 5 || s.starts_with('.') {
        return;
    }
    let norm = normalize_token(s);
    if norm.len() < 5 || GENERIC_BINARY_TOKENS.contains(&norm.as_str()) {
        return;
    }
    out.push(s.to_string());
    if norm != s {
        out.push(norm);
    }
}

/// `Contents/MacOS` 下明显非可执行文件的扩展名（避免把数据文件当 token）。
const NON_EXECUTABLE_EXTS: &[&str] = &[
    "dat", "txt", "dylib", "so", "json", "plist", "png", "icns", "pdf", "html", "js", "css", "map",
    "pak", "log", "md", "xml", "yaml", "yml", "ttf", "otf", "woff", "woff2", "jpg", "jpeg", "gif",
    "svg", "webp",
];

/// 收集目录下一层条目的名字，作为 token。
fn collect_dir_names(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let ext = e
            .path()
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase());
        if let Some(ext) = ext
            && NON_EXECUTABLE_EXTS.contains(&ext.as_str())
        {
            continue;
        }
        push_token(out, &e.file_name().to_string_lossy());
    }
}

/// 收集目录下一层 `*.app` 的 bundle 名与其 `Contents/MacOS` 可执行名。
fn collect_nested_bundles(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("app") {
            continue;
        }
        if let Some(stem) = p.file_stem() {
            push_token(out, &stem.to_string_lossy());
        }
        collect_dir_names(&p.join("Contents/MacOS"), out);
    }
}

/// 从 App Bundle 内部推导额外匹配 token（参考 Pearcleaner）。
///
/// App 的可执行文件、嵌套 helper / 登录项 bundle 名，常被用作残留目录名
/// （如 `<Helper>.plist`、`Application Support/<Helper>`）。这里只产出强证据
/// 的名字，并过滤通用词与过短名字。
fn bundle_internal_tokens(app_path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let contents = app_path.join("Contents");

    // Contents/MacOS 下的可执行文件名（含 CFBundleExecutable，按约定就在此目录）
    collect_dir_names(&contents.join("MacOS"), &mut out);
    // Contents/Library/* 下的 helper / 登录项 / XPC
    for sub in [
        "Library/LoginItems",
        "Library/LaunchServices",
        "Library/XPCServices",
        "Library/Helpers",
        "Library/PrivilegedHelperTools",
    ] {
        collect_nested_bundles(&contents.join(sub), &mut out);
    }
    // Contents/<任意子目录>/*.app（一层，参考 Pearcleaner）
    if let Ok(rd) = std::fs::read_dir(&contents) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_nested_bundles(&p, &mut out);
            }
        }
    }

    out.sort();
    out.dedup();
    out
}

/// 从显示名 / bundle id 推导可能的数据目录名。
///
/// 只用强证据：完整 bundle id、bundle 末段（非通用词）、显示名、归一化显示名。
/// **不用** bundle 中间段（如 `google`）做竞配，避免误删其它同厂商 App 的数据；
/// 额外目录名提示见 [`crate::app_conditions`]（声明式条件表）。
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
    }
    v.retain(|t| t.len() >= 3 && !t.chars().all(|c| c == '.'));
    v.sort();
    v.dedup();
    v
}

/// 插件/扩展类目录：匹配 `<token>.<ext>` 形式的子项。
const PLUGIN_DIRS: &[(&str, &str)] = &[
    ("Internet Plug-Ins", "plugin"),
    ("Internet Plug-Ins", "webplugin"),
    ("PreferencePanes", "prefPane"),
    ("QuickLook", "qlgenerator"),
    ("Screen Savers", "saver"),
    ("ColorPickers", "colorPicker"),
    ("Dictionaries", "dictionary"),
    ("Automator", "action"),
    ("Automator", "workflow"),
    ("Spotlight", "mdimporter"),
    ("Services", "service"),
    ("Widgets", "wdgt"),
    ("Audio/Plug-Ins/Components", "component"),
    ("Audio/Plug-Ins/VST", "vst"),
    ("Audio/Plug-Ins/VST3", "vst3"),
    ("Audio/Plug-Ins/CLAP", "clap"),
];

/// 生成所有候选残留路径（path, sudo）
fn candidate_paths(home: &Path, tokens: &[String]) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    let lib = home.join("Library");

    // 用户 Library 子目录（按目录名精确匹配）
    for sub in [
        // 应用数据 / 缓存 / 偏好
        "Application Support",
        "Application Support/CrashReporter",
        "Caches",
        "Logs",
        "Containers",
        "Group Containers",
        "Application Scripts",
        "WebKit",
        "HTTPStorages",
        "Preferences",
        "LaunchAgents",
        "Saved Application State",
        // 插件 / 扩展 / 系统集成
        "Internet Plug-Ins",
        "PreferencePanes",
        "Services",
        "QuickLook",
        "Screen Savers",
        "ColorPickers",
        "Dictionaries",
        "Automator",
        "Spotlight",
        "Input Methods",
        "Widgets",
        "ScriptingAdditions",
        "PDF Services",
        "Address Book Plug-Ins",
        "Contextual Menu Items",
        "Safari/Extensions",
        "Mail/Bundles",
        "Audio/Plug-Ins/Components",
        "Audio/Plug-Ins/HAL",
        "Audio/Plug-Ins/VST",
        "Audio/Plug-Ins/VST3",
        "Audio/Plug-Ins/CLAP",
    ] {
        for t in tokens {
            out.push((lib.join(sub).join(t), false));
        }
    }
    // `<token>.<ext>` 形式的插件/扩展
    for (sub, ext) in PLUGIN_DIRS {
        for t in tokens {
            out.push((lib.join(sub).join(format!("{t}.{ext}")), false));
        }
    }
    // 带后缀的常见偏好/状态文件
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

    // 共享目录（用户可写，无需 root）
    for sub in ["", "Library/Application Support"] {
        for t in tokens {
            out.push((Path::new("/Users/Shared").join(sub).join(t), false));
        }
    }

    // 系统级 /Library（需 sudo，交安全门跳过并提示手动处理）
    let sys = Path::new("/Library");
    for sub in [
        "Application Support",
        "Application Support/CrashReporter",
        "Caches",
        "Logs",
        "Preferences",
        "LaunchAgents",
        "LaunchDaemons",
        "PrivilegedHelperTools",
        "Application Scripts",
        "Internet Plug-Ins",
        "PreferencePanes",
        "QuickLook",
        "Screen Savers",
        "ColorPickers",
        "Dictionaries",
        "Automator",
        "Spotlight",
        "Input Methods",
        "Widgets",
        "Services",
        "Contextual Menu Items",
        "Audio/Plug-Ins/Components",
        "Audio/Plug-Ins/HAL",
        "Audio/Plug-Ins/VST",
        "Audio/Plug-Ins/VST3",
        "Audio/Plug-Ins/CLAP",
    ] {
        for t in tokens {
            out.push((sys.join(sub).join(t), true));
        }
    }
    for (sub, ext) in PLUGIN_DIRS {
        for t in tokens {
            out.push((sys.join(sub).join(format!("{t}.{ext}")), true));
        }
    }
    for t in tokens {
        out.push((sys.join("Preferences").join(format!("{t}.plist")), true));
        out.push((sys.join("LaunchAgents").join(format!("{t}.plist")), true));
        out.push((sys.join("LaunchDaemons").join(format!("{t}.plist")), true));
    }

    // Homebrew / 本地安装（标 sudo：交安全门手动处理，不自动删 brew 管理的文件）
    for base in ["/usr/local", "/opt/homebrew"] {
        for sub in [
            "bin", "etc", "opt", "share", "var", "sbin", "Cellar", "Caskroom",
        ] {
            for t in tokens {
                out.push((Path::new(base).join(sub).join(t), true));
            }
        }
    }

    out
}

/// 沙盒容器映射：`identifier -> [容器目录]`，进程内只批量枚举一次。
///
/// 容器目录名可能是 UUID（并不等于 bundle id），权威标识来自容器内的
/// `.com.apple.containermanagerd.metadata.plist`；由 Swift 后端一次返回。
fn sandbox_container_map() -> &'static HashMap<String, Vec<PathBuf>> {
    static MAP: OnceLock<HashMap<String, Vec<PathBuf>>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let Ok(home) = std::env::var("HOME").map(PathBuf::from) else {
            return m;
        };
        if let Some(list) = crate::platform::platform().sandbox_containers(&home) {
            for c in list {
                m.entry(c.identifier).or_default().push(c.path);
            }
        }
        m
    })
}

/// 声明式条件是否允许一个候选路径（相对 home 归一化后做子串匹配）。
///
/// `require` 非空时须命中任一子串；命中 `exclude` 任一子串则拒绝。
/// 仅用于 token / forcePath 派生的候选，entitlements 等权威来源不经过此过滤。
fn condition_allows(c: &crate::app_conditions::AppCondition, path: &Path, home: &Path) -> bool {
    let rel = path.strip_prefix(home).unwrap_or(path);
    let norm = normalize_token(&rel.to_string_lossy());
    if !c.require.is_empty() && !c.require.iter().any(|r| norm.contains(&normalize_token(r))) {
        return false;
    }
    !c.exclude.iter().any(|e| norm.contains(&normalize_token(e)))
}

/// 查找某个 App 的关联残留。
///
/// 覆盖：App Bundle 内部可执行文件 / helper / 登录项名、用户 `~/Library` 各
/// 子目录（含插件/扩展）、主目录点目录/XDG、`/Users/Shared`、系统级 `/Library`
/// 与 Homebrew（后两者标记 `sudo=true`，安全门会跳过并提示手动处理），
/// 以及代码签名 entitlements / 沙盒容器元数据。仅返回真实存在的路径。
pub fn find_leftovers(name: &str, bundle_id: Option<&str>, app_path: &Path) -> Vec<Leftover> {
    let Ok(home) = std::env::var("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let cond = crate::app_conditions::lookup(bundle_id);

    let mut tokens = name_tokens(name, bundle_id);
    // App Bundle 内部可执行文件 / 嵌套 helper / 登录项名（参考 Pearcleaner）
    tokens.extend(bundle_internal_tokens(app_path));
    // 声明式条件表补充的目录名 token
    if let Some(c) = cond {
        tokens.extend(c.tokens.iter().cloned());
    }
    tokens.retain(|t| t.len() >= 3 && !t.chars().all(|c| c == '.'));
    tokens.sort();
    tokens.dedup();
    let mut candidates = candidate_paths(&home, &tokens);

    // 条件表：精确补充候选路径，并用 require/exclude 收敛 token / forcePath 候选。
    //（厂商目录共享、同族 App 多用这里的 require/exclude 区分。）
    if let Some(c) = cond {
        for raw in &c.force_paths {
            if let Some(p) = crate::fsutil::expand(raw) {
                candidates.push((p, false));
            }
        }
        candidates.retain(|(p, _)| condition_allows(c, p, &home));
    }

    // 代码签名 entitlements：精确补充沙盒容器、group 容器、iCloud 容器与 team id，
    // 不再只靠名称猜测（group id 常与 bundle id 无关）。
    if let Some(info) = crate::platform::platform().app_sandbox_info(app_path) {
        let lib = home.join("Library");
        if info.sandboxed
            && let Some(b) = bundle_id.or(info.bundle_id.as_deref())
        {
            candidates.push((lib.join("Containers").join(b), false));
        }
        for g in &info.groups {
            candidates.push((lib.join("Group Containers").join(g), false));
        }
        for c in &info.icloud_containers {
            // iCloud 容器目录：`iCloud.com.foo` 常落盘为 `iCloud~com~foo`
            candidates.push((lib.join("Mobile Documents").join(c), false));
            candidates.push((
                lib.join("Mobile Documents").join(c.replace('.', "~")),
                false,
            ));
        }
        // team id 是 Group Containers 的常见前缀（如 `TEAMID.group.com.foo`）
        if let Some(team) = &info.team_id {
            let gc = home.join("Library/Group Containers");
            if let Ok(rd) = std::fs::read_dir(&gc) {
                let prefix = format!("{team}.");
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        candidates.push((e.path(), false));
                    }
                }
            }
        }
    }

    // 沙盒容器（目录名可能是 UUID）：查一次批量枚举得到的映射
    if let Some(b) = bundle_id
        && let Some(paths) = sandbox_container_map().get(b)
    {
        for p in paths {
            candidates.push((p.clone(), false));
        }
    }

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

    materialize_leftovers(app_path, candidates)
}

/// App 深扫（`--deep`）：在精确候选之外，用 Spotlight 补充带后缀/嵌套的残留。
///
/// 结果仍作为普通候选项，走同一安全门；Spotlight 只提供候选，不改变删除语义。
/// 后端不可用/未索引/超时时优雅退回精确候选。
pub fn find_leftovers_deep(name: &str, bundle_id: Option<&str>, app_path: &Path) -> Vec<Leftover> {
    let base = find_leftovers(name, bundle_id, app_path);
    let Ok(home) = std::env::var("HOME").map(PathBuf::from) else {
        return base;
    };
    let terms = spotlight_terms(name, bundle_id);
    if terms.is_empty() {
        return base;
    }
    let raw = crate::spotlight::search(&home, &terms);
    if raw.is_empty() {
        return base;
    }
    let extra = materialize_leftovers(app_path, raw.into_iter().map(|p| (p, false)).collect());
    merge_leftovers(base, extra)
}

/// Spotlight 深扫使用的强 token：App 名、bundle id 与条件表 token。
///
/// 只保留归一化后长度 >= 5 的 token，避免 `Code` 之类泛词在整盘搜索里误配
/// （这类精确目录已由候选生成覆盖，无需 Spotlight）。
fn spotlight_terms(name: &str, bundle_id: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let n = name.trim();
    if !n.is_empty() {
        v.push(n.to_string());
    }
    if let Some(b) = bundle_id {
        v.push(b.to_string());
    }
    if let Some(c) = crate::app_conditions::lookup(bundle_id) {
        for t in &c.tokens {
            v.push(t.clone());
        }
    }
    v.retain(|t| normalize_token(t).len() >= 5);
    v.sort();
    v.dedup();
    v
}

/// 合并精确候选与深扫补充：父目录优先，去掉被父项覆盖的子项。
fn merge_leftovers(base: Vec<Leftover>, extra: Vec<Leftover>) -> Vec<Leftover> {
    let mut all: Vec<Leftover> = Vec::with_capacity(base.len() + extra.len());
    all.extend(extra);
    all.extend(base);

    // 组件数升序 → 父目录在前；被已有父项覆盖的子项跳过
    all.sort_by(|a, b| {
        a.path
            .components()
            .count()
            .cmp(&b.path.components().count())
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut kept: Vec<Leftover> = Vec::new();
    for item in all {
        if kept
            .iter()
            .any(|k| item.path == k.path || item.path.starts_with(&k.path))
        {
            continue;
        }
        kept.push(item);
    }
    kept.sort_by_key(|a| std::cmp::Reverse(a.size));
    kept
}

/// 把候选路径落成真实存在的残留（过滤 App 自身、解析符号链接、去重、排序）。
///
/// - 跳过不存在、指向 App 自身或 App 内部的候选（含符号链接解析后落在 App 内者，
///   如 VS Code 的 `/usr/local/bin/code`）；
/// - APFS 默认大小写不敏感，用真实路径去重并存储（大小写以磁盘为准）。
fn materialize_leftovers(app_path: &Path, candidates: Vec<(PathBuf, bool)>) -> Vec<Leftover> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Leftover> = Vec::new();
    for (p, sudo) in candidates {
        if !p.exists() || p == app_path || p.starts_with(app_path) {
            continue;
        }
        let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if real == app_path || real.starts_with(app_path) {
            continue;
        }
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
    fn conditions_provide_extra_tokens() {
        let vscode =
            crate::app_conditions::lookup(Some("com.microsoft.VSCode")).expect("应有 VS Code 条件");
        assert!(vscode.tokens.contains(&"Code".to_string()));

        let chrome =
            crate::app_conditions::lookup(Some("com.google.Chrome")).expect("应有 Chrome 条件");
        assert!(chrome.require.contains(&"chrome".to_string()));
    }

    #[test]
    fn condition_require_exclude_filter() {
        let home = Path::new("/Users/x");
        let chrome = crate::app_conditions::lookup(Some("com.google.Chrome")).unwrap();
        // 共享的 Google 厂商目录被 require 拒绝
        assert!(!condition_allows(
            chrome,
            Path::new("/Users/x/Library/Application Support/Google"),
            home
        ));
        // 具体 Chrome 目录保留
        assert!(condition_allows(
            chrome,
            Path::new("/Users/x/Library/Application Support/Google/Chrome"),
            home
        ));

        let code = crate::app_conditions::lookup(Some("com.microsoft.VSCode")).unwrap();
        assert!(!condition_allows(
            code,
            Path::new("/Users/x/Library/Application Support/Code - Insiders"),
            home
        ));
        assert!(condition_allows(
            code,
            Path::new("/Users/x/Library/Application Support/Code"),
            home
        ));
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
    fn candidates_cover_plugins_and_shared() {
        let home = Path::new("/Users/x");
        let tokens = vec!["myapp".to_string()];
        let c = candidate_paths(home, &tokens);
        assert!(
            c.iter()
                .any(|(p, _)| p == Path::new("/Users/x/Library/QuickLook/myapp.qlgenerator"))
        );
        assert!(
            c.iter()
                .any(|(p, _)| p == Path::new("/Users/x/Library/Audio/Plug-Ins/VST3/myapp.vst3"))
        );
        assert!(
            c.iter()
                .any(|(p, _)| p == Path::new("/Users/x/Library/Internet Plug-Ins/myapp.plugin"))
        );
        // /Users/Shared 用户可写（不需 sudo）
        assert!(
            c.iter()
                .any(|(p, s)| p == Path::new("/Users/Shared/myapp") && !*s)
        );
        // Homebrew 标 sudo（交安全门手动处理）
        assert!(
            c.iter()
                .any(|(p, s)| p == Path::new("/opt/homebrew/bin/myapp") && *s)
        );
    }

    #[test]
    fn generic_binary_tokens_filtered() {
        let mut v = Vec::new();
        push_token(&mut v, "Helper");
        push_token(&mut v, "Electron");
        push_token(&mut v, "xy");
        assert!(v.is_empty(), "通用/过短名字应被过滤: {v:?}");
        push_token(&mut v, "FooBar");
        assert!(v.contains(&"FooBar".to_string()));
    }

    #[test]
    fn bundle_tokens_include_executables_and_helpers() {
        let root = std::env::temp_dir().join(format!("thin-bundle-{}", std::process::id()));
        let app = root.join("Foo.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/FooMainBinary"), b"").unwrap();
        let login = app.join("Contents/Library/LoginItems/FooLoginHelper.app/Contents/MacOS");
        std::fs::create_dir_all(&login).unwrap();
        std::fs::write(login.join("FooLoginHelper"), b"").unwrap();

        let t = bundle_internal_tokens(&app);
        assert!(t.contains(&"FooMainBinary".to_string()), "{t:?}");
        assert!(t.contains(&"FooLoginHelper".to_string()), "{t:?}");
        assert!(!t.iter().any(|s| normalize_token(s) == "helper"), "{t:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_into_app_is_not_a_leftover() {
        let base = std::env::temp_dir().join(format!("thin-link-{}", std::process::id()));
        let app = base.join("Foo.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/Foo"), b"x").unwrap();
        let app = std::fs::canonicalize(&app).unwrap();

        let links = base.join("links");
        std::fs::create_dir_all(&links).unwrap();
        let link = links.join("foo");
        std::os::unix::fs::symlink(app.join("Contents/MacOS/Foo"), &link).unwrap();

        let out = materialize_leftovers(&app, vec![(link, true)]);
        assert!(out.is_empty(), "指向 App 内的符号链接不应作为残留: {out:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn spotlight_terms_require_length() {
        let t = spotlight_terms("Visual Studio Code", Some("com.microsoft.VSCode"));
        assert!(t.contains(&"Visual Studio Code".to_string()));
        assert!(t.contains(&"com.microsoft.VSCode".to_string()));
        // 条件表 token "Code" 仅 4 位，不参与整盘深扫
        assert!(!t.contains(&"Code".to_string()));

        let arc = spotlight_terms("Arc", Some("company.thebrowser.Browser"));
        assert!(arc.iter().all(|t| normalize_token(t).len() >= 5), "{arc:?}");
    }

    #[test]
    fn deep_merge_prefers_parent() {
        let base = vec![Leftover {
            path: PathBuf::from("/Users/x/Library/Application Support/Foo/Sub"),
            size: 10,
            sudo: false,
        }];
        let extra = vec![
            Leftover {
                path: PathBuf::from("/Users/x/Library/Application Support/Foo"),
                size: 100,
                sudo: false,
            },
            Leftover {
                path: PathBuf::from("/Users/x/Library/Application Scripts/com.foo.bar.Ext"),
                size: 5,
                sudo: false,
            },
        ];
        let merged = merge_leftovers(base, extra);
        let paths: Vec<PathBuf> = merged.iter().map(|l| l.path.clone()).collect();
        assert!(paths.contains(&PathBuf::from("/Users/x/Library/Application Support/Foo")));
        assert!(!paths.contains(&PathBuf::from(
            "/Users/x/Library/Application Support/Foo/Sub"
        )));
        assert!(paths.contains(&PathBuf::from(
            "/Users/x/Library/Application Scripts/com.foo.bar.Ext"
        )));
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
