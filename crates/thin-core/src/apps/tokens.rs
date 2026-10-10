//! App 名 / bundle 名 / Bundle 内部结构 → 候选目录 token 的推导与过滤。
//!
//! 只产出**强证据**的名字（完整 bundle id、bundle 末段、显示名、Bundle 内可执行名），
//! 过滤通用词与过短名字，避免残留匹配误伤同厂商的其它 App。

use std::path::Path;

/// 把名字归一化为 alnum 小写，用于目录名匹配。
pub(super) fn normalize_token(s: &str) -> String {
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

/// `Contents/MacOS` 下明显非可执行文件的扩展名（避免把数据文件当 token）。
const NON_EXECUTABLE_EXTS: &[&str] = &[
    "dat", "txt", "dylib", "so", "json", "plist", "png", "icns", "pdf", "html", "js", "css", "map",
    "pak", "log", "md", "xml", "yaml", "yml", "ttf", "otf", "woff", "woff2", "jpg", "jpeg", "gif",
    "svg", "webp",
];

/// 加入一个来自文件名 / 可执行名的 token（原始名 + 归一化名）。
///
/// 只接受足够长（>=5）且非通用词的名字，避免用 `Helper`、`Electron` 之类误配。
pub(super) fn push_token(out: &mut Vec<String>, raw: &str) {
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

/// 加入一个 App 别名（显示名/旧名）：原样 + 归一化，阈值与显示名一致（>=3）。
pub(super) fn push_alias_token(out: &mut Vec<String>, alias: &str) {
    let s = alias.trim();
    if s.len() < 3 || s.chars().all(|c| c == '.') {
        return;
    }
    out.push(s.to_string());
    let norm = normalize_token(s);
    if norm.len() >= 3 && norm != s {
        out.push(norm);
    }
}

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
pub(super) fn bundle_internal_tokens(app_path: &Path) -> Vec<String> {
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
pub(super) fn name_tokens(name: &str, bundle_id: Option<&str>) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn alias_tokens_keep_raw_and_normalized() {
        let mut tokens = Vec::new();
        push_alias_token(&mut tokens, "Visual Studio Code");
        push_alias_token(&mut tokens, "ab"); // 太短，忽略
        assert!(tokens.contains(&"Visual Studio Code".to_string()));
        assert!(tokens.contains(&"visualstudiocode".to_string()));
        assert_eq!(tokens.iter().filter(|t| t.as_str() == "ab").count(), 0);
    }
}
