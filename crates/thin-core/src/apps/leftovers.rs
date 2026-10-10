//! App 关联残留的候选生成、条件过滤与落盘。
//!
//! 覆盖用户 `~/Library` 各子目录（含插件/扩展）、主目录点目录/XDG、`/Users/Shared`、
//! 系统级 `/Library` 与 Homebrew（后两者标记 `sudo=true`，安全门会跳过并提示手动处理），
//! 以及代码签名 entitlements / 沙盒容器元数据。仅返回真实存在的路径。

use super::Leftover;
use super::tokens::{bundle_internal_tokens, name_tokens, normalize_token, push_alias_token};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

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

/// macOS 每用户 Darwin 目录根：`0`（User）、`T`（Temp）、`C`（Cache）。
///
/// 形如 `/private/var/folders/<xx>/<随机串>/{0,T,C}`，随机前缀无法硬编码，
/// 用 `confstr` 解析。非 macOS / 解析失败时返回空。
fn darwin_user_dirs() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut out: Vec<PathBuf> = Vec::new();
        for name in [
            libc::_CS_DARWIN_USER_DIR,
            libc::_CS_DARWIN_USER_TEMP_DIR,
            libc::_CS_DARWIN_USER_CACHE_DIR,
        ] {
            if let Some(p) = confstr_dir(name)
                && !out.contains(&p)
            {
                out.push(p);
            }
        }
        out
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// 解析一个 `confstr` 目录（第一次取长度，第二次取内容）。
#[cfg(target_os = "macos")]
fn confstr_dir(name: libc::c_int) -> Option<PathBuf> {
    let len = unsafe { libc::confstr(name, std::ptr::null_mut(), 0) };
    if len == 0 {
        return None;
    }
    let mut buf: Vec<libc::c_char> = vec![0; len];
    let n = unsafe { libc::confstr(name, buf.as_mut_ptr(), len) };
    if n == 0 || n > len {
        return None;
    }
    let bytes: Vec<u8> = buf[..n - 1].iter().map(|&c| c as u8).collect();
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Darwin 每用户缓存目录（`_CS_DARWIN_USER_CACHE_DIR`）。
///
/// 供孤儿残留检测枚举 `C/<bundle-id>` 使用；非 macOS / 解析失败返回 `None`。
pub fn darwin_user_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        confstr_dir(libc::_CS_DARWIN_USER_CACHE_DIR)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 缓存 Darwin 用户目录下的条目：`(路径, 小写文件名)`。只枚举一次。
fn darwin_user_entries() -> &'static [(PathBuf, String)] {
    static ENTRIES: OnceLock<Vec<(PathBuf, String)>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        let mut out = Vec::new();
        for root in darwin_user_dirs() {
            let Ok(rd) = std::fs::read_dir(&root) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_lowercase();
                out.push((e.path(), name));
            }
        }
        out
    })
}

/// 从 Darwin 用户目录条目里按 token 匹配候选：小写文件名等于 token，或（仅对含 `.`
/// 的 bundle 式 token）以 `token.` 为前缀。这样既壮住 `.helper` 变体，又不会把
/// `com.foo.bar` 误配成同前缀的 `com.foo.barista`。
fn darwin_user_candidates(
    tokens: &[String],
    entries: &[(PathBuf, String)],
) -> Vec<(PathBuf, bool)> {
    let mut out = Vec::new();
    for (path, name) in entries {
        for t in tokens {
            let tl = t.to_lowercase();
            if *name == tl || (tl.contains('.') && name.starts_with(&format!("{tl}."))) {
                out.push((path.clone(), false));
                break;
            }
        }
    }
    out
}

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

    // macOS 每用户 Darwin 临时/缓存目录（`/private/var/folders/<随机>/{0,T,C}`）：
    // 不少 App 会在 `$TMPDIR` / 用户缓存下按 bundle id 建目录；安全门已允许该子树。
    out.extend(darwin_user_candidates(tokens, darwin_user_entries()));

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
        // 别名按显示名规则补充：原样 + 归一化，供目录名与 `--deep` 强 token 命中
        for a in &c.aliases {
            push_alias_token(&mut tokens, a);
        }
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
        for a in &c.aliases {
            v.push(a.clone());
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
    use super::super::tokens::normalize_token;
    use super::*;

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
    fn darwin_user_candidates_match_bundle_and_helpers_only() {
        let entries = vec![
            (PathBuf::from("/p/C/com.foo.bar"), "com.foo.bar".to_string()),
            (
                PathBuf::from("/p/C/com.foo.bar.helper"),
                "com.foo.bar.helper".to_string(),
            ),
            // 同前缀的另一个产品，不应被 `com.foo.bar` 命中
            (
                PathBuf::from("/p/C/com.foo.barista"),
                "com.foo.barista".to_string(),
            ),
        ];
        let got = darwin_user_candidates(&["com.foo.bar".to_string()], &entries);
        let paths: Vec<&PathBuf> = got.iter().map(|(p, _)| p).collect();
        assert!(paths.contains(&&PathBuf::from("/p/C/com.foo.bar")));
        assert!(paths.contains(&&PathBuf::from("/p/C/com.foo.bar.helper")));
        assert!(!paths.contains(&&PathBuf::from("/p/C/com.foo.barista")));
        assert_eq!(got.len(), 2);
    }
}
