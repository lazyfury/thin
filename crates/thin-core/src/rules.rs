use crate::clean;
use crate::fsutil;
use crate::model::{Category, Explain, Matcher, Risk, Rule};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// 内置规则（编译期嵌入，随版本更新）
const DEFAULT_RULES: &str = include_str!("../rules/default.json");

/// 内置规则
pub fn builtin() -> Result<Vec<Rule>> {
    serde_json::from_str(DEFAULT_RULES).context("内置规则解析失败")
}

/// 旧版用户规则文件（单文件数组）。`thin rules add` **不再写入**，仅兼容读取。
pub fn user_rules_path() -> PathBuf {
    user_rules_path_in(&clean::thin_home())
}

pub fn user_rules_path_in(home: &Path) -> PathBuf {
    home.join("rules.json")
}

/// 用户规则目录（**唯一写入点**）：`rules.d/<id>.json`，每个文件为单条规则。
/// 加载顺序：内置 →（兼容读取的旧 `rules.json`）→ `rules.d/*.json`（按文件名），同名 id 后者覆盖。
pub fn user_rules_dir() -> PathBuf {
    user_rules_dir_in(&clean::thin_home())
}

pub fn user_rules_dir_in(home: &Path) -> PathBuf {
    home.join("rules.d")
}

/// 读取用户规则（不存在则为空）
pub fn load_user_rules() -> Result<Vec<Rule>> {
    load_user_rules_in(&clean::thin_home())
}

pub fn load_user_rules_in(home: &Path) -> Result<Vec<Rule>> {
    let path = user_rules_path_in(home);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取用户规则失败: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).with_context(|| format!("用户规则解析失败: {}", path.display()))
}

/// 读取 `rules.d` 下的所有规则（按文件名排序）。每个文件可为单条规则或规则数组。
pub fn load_dir_rules() -> Result<Vec<Rule>> {
    load_dir_rules_in(&user_rules_dir())
}

/// [`load_dir_rules`] 的可指定目录版本（便于测试）
pub fn load_dir_rules_in(dir: &Path) -> Result<Vec<Rule>> {
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
            .with_context(|| format!("读取规则文件失败: {}", f.display()))?;
        if raw.trim().is_empty() {
            continue;
        }
        // 先按数组解析，再退回单条
        match serde_json::from_str::<Vec<Rule>>(&raw) {
            Ok(v) => out.extend(v),
            Err(_) => out.push(
                serde_json::from_str::<Rule>(&raw)
                    .with_context(|| format!("规则解析失败: {}", f.display()))?,
            ),
        }
    }
    Ok(out)
}

/// 所有用户来源的规则（`rules.json` + `rules.d`），同 id 后者覆盖。
pub fn load_all_user_rules() -> Result<Vec<Rule>> {
    load_all_user_rules_in(&clean::thin_home())
}

pub fn load_all_user_rules_in(home: &Path) -> Result<Vec<Rule>> {
    let mut all = load_user_rules_in(home)?;
    all.extend(load_dir_rules_in(&user_rules_dir_in(home))?);
    let mut merged: Vec<Rule> = Vec::new();
    for r in all {
        merged.retain(|x| x.id != r.id);
        merged.push(r);
    }
    Ok(merged)
}

/// 写入用户规则文件
pub fn save_user_rules(rules: &[Rule]) -> Result<PathBuf> {
    save_user_rules_in(&clean::thin_home(), rules)
}

pub fn save_user_rules_in(home: &Path, rules: &[Rule]) -> Result<PathBuf> {
    let path = user_rules_path_in(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(rules)?)?;
    Ok(path)
}

/// 把单条规则写入 `rules.d/<id>.json`（一规则一文件，便于逐步添加/管理）
pub fn save_dir_rule(rule: &Rule) -> Result<PathBuf> {
    save_dir_rule_in(&clean::thin_home(), rule)
}

pub fn save_dir_rule_in(home: &Path, rule: &Rule) -> Result<PathBuf> {
    let dir = user_rules_dir_in(home);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", rule.id));
    std::fs::write(&path, serde_json::to_string_pretty(rule)?)?;
    Ok(path)
}

/// 新增/覆盖一条用户规则（按 id），返回写入的文件路径。
///
/// **唯一写入点**：`rules.d/<id>.json`（一规则一文件）。好处：
/// - 避免读改写整个 `rules.json` 的竞态，适合 agent 逐步增删；
/// - 加载顺序简单：内置 →（兼容读取的旧 `rules.json`）→ `rules.d`。
///
/// 若旧的 `rules.json` 里存在同 id 的陈旧条目，会一并清除，避免两份不一致。
pub fn upsert_user_rule(rule: Rule) -> Result<PathBuf> {
    upsert_user_rule_in(&clean::thin_home(), rule)
}

pub fn upsert_user_rule_in(home: &Path, rule: Rule) -> Result<PathBuf> {
    let saved = save_dir_rule_in(home, &rule)?;
    let mut legacy = load_user_rules_in(home)?;
    let before = legacy.len();
    legacy.retain(|r| r.id != rule.id);
    if legacy.len() != before {
        save_user_rules_in(home, &legacy)?;
    }
    Ok(saved)
}

/// 删除一条用户规则（`rules.json` 与 `rules.d/<id>.json`）；返回是否删除成功
pub fn remove_user_rule(id: &str) -> Result<bool> {
    let mut removed = false;
    let mut rules = load_user_rules()?;
    let before = rules.len();
    rules.retain(|r| r.id != id);
    if rules.len() != before {
        save_user_rules(&rules)?;
        removed = true;
    }
    let dir_file = user_rules_dir().join(format!("{id}.json"));
    if dir_file.exists() {
        std::fs::remove_file(&dir_file)?;
        removed = true;
    }
    Ok(removed)
}

/// 合并加载：内置/THIN_RULES → rules.json → rules.d（用户按 id 覆盖）
pub fn load() -> Result<Vec<Rule>> {
    let mut rules = base_rules()?;
    for ur in load_all_user_rules().unwrap_or_default() {
        match rules.iter().position(|r| r.id == ur.id) {
            Some(pos) => rules[pos] = ur,
            None => rules.push(ur),
        }
    }
    Ok(rules)
}

/// 基础规则：THIN_RULES 指向外部文件时覆盖内置
fn base_rules() -> Result<Vec<Rule>> {
    match std::env::var("THIN_RULES") {
        Ok(p) if !p.is_empty() => {
            let raw =
                std::fs::read_to_string(&p).with_context(|| format!("读取外部规则失败: {p}"))?;
            serde_json::from_str(&raw).context("外部规则 JSON 解析失败")
        }
        _ => builtin(),
    }
}

/// 把规则展开成具体的候选路径。
///
/// 结果统一 `canonicalize`（解析 symlink，如 `/tmp → /private/tmp`）并去重，
/// 保证后续嵌套去重与安全门前缀判断不会因路径写法不同而失效。
pub fn expand_rule(rule: &Rule) -> Vec<PathBuf> {
    expand_rule_scoped(rule, None)
}

/// 展开规则的命中路径。
///
/// `scope=Some(root)` 时，`findDir` 规则改为在 `root` 下查找，使 `--root .` 能发现
/// 任意位置项目里的 `target/`、`node_modules/` 等产物（否则只会扫内置的 `~/Documents` 等）。
/// `path` 规则保持原样，随后由 `scan` 的作用域过滤裁掉不属于该 root 的项。
pub fn expand_rule_scoped(rule: &Rule, scope: Option<&Path>) -> Vec<PathBuf> {
    match &rule.matcher {
        Matcher::Path { paths } => {
            let mut out: Vec<PathBuf> = paths
                .iter()
                .filter_map(|p| fsutil::expand(p))
                .filter(|p| p.exists())
                .map(|p| fsutil::canonicalize_or(&p))
                .collect();
            out.sort();
            out.dedup();
            out
        }
        Matcher::FindDir {
            roots,
            dir_name,
            require_sibling,
            max_depth,
        } => {
            // 指定作用域时，只在该根目录下查找
            let roots: Vec<PathBuf> = if let Some(root) = scope {
                vec![root.to_path_buf()]
            } else {
                roots.iter().filter_map(|r| fsutil::expand(r)).collect()
            };
            let roots: Vec<PathBuf> = roots
                .into_iter()
                .filter(|r| r.is_dir())
                .map(|r| fsutil::canonicalize_or(&r))
                .collect();
            let mut found =
                fsutil::find_dirs(&roots, dir_name, require_sibling.as_deref(), *max_depth);
            found = found
                .into_iter()
                .map(|p| fsutil::canonicalize_or(&p))
                .collect();
            found.sort();
            found.dedup();
            found
        }
        Matcher::FindFile {
            roots,
            extensions,
            max_depth,
            min_size,
        } => {
            let roots: Vec<PathBuf> = if let Some(root) = scope {
                vec![root.to_path_buf()]
            } else {
                roots.iter().filter_map(|r| fsutil::expand(r)).collect()
            };
            let roots: Vec<PathBuf> = roots
                .into_iter()
                .filter(|r| r.is_dir())
                .map(|r| fsutil::canonicalize_or(&r))
                .collect();
            let mut found =
                fsutil::find_files(&roots, extensions, *max_depth, min_size.unwrap_or(0));
            found = found
                .into_iter()
                .map(|p| fsutil::canonicalize_or(&p))
                .collect();
            found.sort();
            found.dedup();
            found
        }
        Matcher::Script {
            roots,
            script,
            timeout_secs,
            review,
        } => {
            // 风险审查：脚本哈希不匹配（未审查 / 已变更）或含危险片段则拒绝运行。
            // 运行时也查一遍，避免有人绕过 `rules add` 直接改写 rules.d。
            if !crate::script::review_ok(script, review)
                || crate::script::review_script(script).is_err()
            {
                return Vec::new();
            }
            // 指定作用域时收窄 roots，脚本输出随后仍会被 containment 过滤
            let roots: Vec<PathBuf> = if let Some(root) = scope {
                vec![root.to_path_buf()]
            } else {
                roots.iter().filter_map(|r| fsutil::expand(r)).collect()
            };
            crate::script::run(script, *timeout_secs, &roots)
        }
    }
}

/// 写盘前的规则安全预检。
///
/// - `path` 规则：目标不得是受保护路径 / 个人目录顶层 / 裸顶层根，
///   否则会“整目录被搬走”。
/// - `findDir` 规则：不允许在**没有 `requireSibling` 约束**的情况下按名字查找
///   敏感目录（如 `Documents`、`Library`），避免误命中个人/系统目录。
///
/// 返回 `Err(原因)` 表示应拒绝写入。
pub fn check_rule_safety(rule: &Rule) -> Result<(), String> {
    match &rule.matcher {
        Matcher::Path { paths } => {
            for raw in paths {
                let Some(p) = fsutil::expand(raw) else {
                    continue;
                };
                if let Some(reason) = clean::static_protection_reason(&p) {
                    return Err(format!("目标 {} 受保护（{}）", p.display(), reason));
                }
            }
        }
        Matcher::FindDir {
            dir_name,
            require_sibling,
            ..
        } => {
            if require_sibling.is_none()
                && let Some(what) = clean::is_sensitive_dir_name(dir_name)
            {
                return Err(format!(
                    "按名字查找 {dir_name:?}（{what}）且未设置 requireSibling，可能误命中受保护目录"
                ));
            }
        }
        Matcher::FindFile {
            roots, extensions, ..
        } => {
            if roots.is_empty() {
                return Err("findFile 规则必须声明 roots（查找范围）".into());
            }
            let valid = extensions
                .iter()
                .any(|e| !e.trim().trim_matches('.').is_empty());
            if !valid {
                return Err("findFile 规则必须声明至少一个扩展名".into());
            }
        }
        Matcher::Script {
            roots,
            script,
            review,
            ..
        } => {
            if roots.is_empty() {
                return Err("script 规则必须声明 roots（输出路径的允许范围）".into());
            }
            for raw in roots {
                let Some(p) = fsutil::expand(raw) else {
                    continue;
                };
                if let Some(reason) = clean::static_protection_reason(&p) {
                    return Err(format!(
                        "script roots {} 过于宽泛 / 受保护（{reason}）",
                        p.display()
                    ));
                }
            }
            crate::script::review_script(script)?;
            if !crate::script::review_ok(script, review) {
                return Err(format!(
                    "脚本未经风险审查或已变更；审查通过后设置 review.hash = {}（或 `thin rules add --approve-script`）",
                    crate::script::hash(script)
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 便于 agent 构造规则
// ---------------------------------------------------------------------------

/// 把分类字符串解析为 Category
pub fn parse_category(s: &str) -> Option<Category> {
    Some(match s.to_lowercase().as_str() {
        "system-cache" | "system" => Category::SystemCache,
        "app-cache" | "app" => Category::AppCache,
        "dev-cache" | "dev" => Category::DevCache,
        "vm" => Category::Vm,
        "log" | "logs" => Category::Log,
        "trash" => Category::Trash,
        "leftover" => Category::Leftover,
        "other" => Category::Other,
        _ => return None,
    })
}

/// 把风险字符串解析为 Risk
pub fn parse_risk(s: &str) -> Option<Risk> {
    Some(match s.to_lowercase().as_str() {
        "safe" => Risk::Safe,
        "confirm" => Risk::Confirm,
        "destructive" => Risk::Destructive,
        _ => return None,
    })
}

/// 由路径生成一个 slug，用作默认 id
pub fn slug_for(path: &str) -> String {
    let base = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("rule");
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "custom".to_string()
    } else {
        format!("custom-{slug}")
    }
}

/// 构造一条最简单的 path 规则（agent 入口用）
#[allow(clippy::too_many_arguments)]
pub fn make_path_rule(
    id: String,
    name: String,
    path: String,
    category: Category,
    risk: Risk,
    regenerable: bool,
    reclaim: String,
    what: String,
    cost: String,
    recover: String,
) -> Rule {
    Rule {
        id,
        name,
        category,
        risk,
        regenerable,
        sudo: false,
        matcher: Matcher::Path { paths: vec![path] },
        reclaim,
        explain: Explain {
            what,
            cost,
            recover,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_and_parsers() {
        assert_eq!(slug_for("~/Library/Caches/Homebrew"), "custom-homebrew");
        assert_eq!(slug_for("/tmp/"), "custom-tmp");
        assert_eq!(parse_risk("SAFE"), Some(Risk::Safe));
        assert_eq!(parse_category("dev-cache"), Some(Category::DevCache));
        assert_eq!(parse_risk("nope"), None);
    }

    #[test]
    fn builtin_rules_parse_and_have_unique_ids() {
        let rules = builtin().expect("内置规则应能解析");
        assert!(!rules.is_empty());
        let mut ids = std::collections::HashSet::new();
        for r in &rules {
            assert!(ids.insert(r.id.clone()), "内置规则 id 重复: {}", r.id);
        }
    }

    #[test]
    fn rules_d_accepts_array_and_single() {
        let base = std::env::temp_dir().join(format!("thin-rulesd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // a.json：数组；b.json：单条
        std::fs::write(
            base.join("a.json"),
            r#"[{"id":"a1","name":"A1","category":"dev-cache","risk":"safe","regenerable":true,"matcher":{"kind":"path","paths":["/tmp/a1"]},"reclaim":"x","explain":{"what":"w","cost":"c","recover":"r"}}]"#,
        )
        .unwrap();
        std::fs::write(
            base.join("b.json"),
            r#"{"id":"b1","name":"B1","category":"dev-cache","risk":"confirm","regenerable":false,"matcher":{"kind":"path","paths":["/tmp/b1"]},"reclaim":"x","explain":{"what":"w","cost":"c","recover":"r"}}"#,
        )
        .unwrap();
        // 非 json 忽略
        std::fs::write(base.join("c.txt"), "ignore me").unwrap();

        let rules = load_dir_rules_in(&base).unwrap();
        let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a1", "b1"]);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upsert_writes_rules_d_and_clears_legacy_rules_json() {
        let base = std::env::temp_dir().join(format!("thin-upsert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let mut r = make_path_rule(
            "dup".into(),
            "旧".into(),
            "/tmp/dup".into(),
            Category::DevCache,
            Risk::Safe,
            true,
            "x".into(),
            "w".into(),
            "c".into(),
            "r".into(),
        );
        // 旧格式：rules.json 里已有同 id 的旧规则
        save_user_rules_in(&base, &[r.clone()]).unwrap();

        r.name = "新".into();
        let saved = upsert_user_rule_in(&base, r).unwrap();

        // 统一写入 rules.d/<id>.json，且旧 rules.json 条目被清理（不残留两份）
        assert!(saved.ends_with("rules.d/dup.json"), "{}", saved.display());
        assert!(
            load_user_rules_in(&base).unwrap().is_empty(),
            "旧 rules.json 条目应被清理"
        );
        let all = load_all_user_rules_in(&base).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "新");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expand_rule_canonicalizes_symlinks_and_dedupes() {
        let base = std::env::temp_dir().join(format!("thin-canon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // 同时指向真实路径与符号链接 → 规范化后应合并为一条
        let base_rule = make_path_rule(
            "x".into(),
            "x".into(),
            String::new(),
            Category::DevCache,
            Risk::Safe,
            true,
            "x".into(),
            "w".into(),
            "c".into(),
            "r".into(),
        );
        let rule = Rule {
            matcher: Matcher::Path {
                paths: vec![
                    link.to_string_lossy().into_owned(),
                    real.to_string_lossy().into_owned(),
                ],
            },
            ..base_rule
        };
        let got = expand_rule(&rule);
        assert_eq!(got, vec![real.canonicalize().unwrap()]);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn find_file_rule_matches_by_extension_and_respects_min_size() {
        let base = std::env::temp_dir().join(format!("thin-findfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("nested")).unwrap();
        std::fs::write(base.join("App.dmg"), b"x").unwrap();
        std::fs::write(base.join("Notes.txt"), b"x").unwrap();
        std::fs::write(base.join("nested/Archive.ZIP"), b"x").unwrap(); // 大小写不敏感

        let mut r = make_path_rule(
            "dl".into(),
            "dl".into(),
            String::new(),
            Category::Other,
            Risk::Confirm,
            false,
            "x".into(),
            "w".into(),
            "c".into(),
            "r".into(),
        );
        r.matcher = Matcher::FindFile {
            roots: vec![base.to_string_lossy().into_owned()],
            extensions: vec!["dmg".into(), "zip".into()],
            max_depth: Some(3),
            min_size: None,
        };
        assert_eq!(expand_rule(&r).len(), 2);

        // minSize 大于文件大小 → 全部过滤
        if let Matcher::FindFile { min_size, .. } = &mut r.matcher {
            *min_size = Some(1 << 20);
        }
        assert!(expand_rule(&r).is_empty());

        // 缺少扩展名 → 预检拒绝
        if let Matcher::FindFile {
            min_size,
            extensions,
            ..
        } = &mut r.matcher
        {
            *min_size = None;
            extensions.clear();
        }
        assert!(check_rule_safety(&r).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn check_rule_safety_blocks_dangerous_rules() {
        // path 规则指向裸顶层根 → 拒绝
        let r = make_path_rule(
            "p".into(),
            "p".into(),
            "/Library/Logs".into(),
            Category::Log,
            Risk::Safe,
            true,
            "rm".into(),
            "w".into(),
            "c".into(),
            "r".into(),
        );
        assert!(check_rule_safety(&r).is_err());

        // findDir 无 requireSibling 且 dirName 敏感 → 拒绝
        let mut r = make_path_rule(
            "f".into(),
            "f".into(),
            String::new(),
            Category::Other,
            Risk::Confirm,
            false,
            "x".into(),
            "w".into(),
            "c".into(),
            "r".into(),
        );
        r.matcher = Matcher::FindDir {
            roots: vec!["~/Documents".into()],
            dir_name: "Documents".into(),
            require_sibling: None,
            max_depth: Some(6),
        };
        assert!(check_rule_safety(&r).is_err());

        // 加上 requireSibling 后放行
        if let Matcher::FindDir {
            require_sibling, ..
        } = &mut r.matcher
        {
            *require_sibling = Some("Cargo.toml".into());
        }
        assert!(check_rule_safety(&r).is_ok());
    }

    #[test]
    fn script_rule_requires_review_and_enforces_containment() {
        let base = std::env::temp_dir().join(format!("thin-script-rule-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sub = base.join("releases");
        let old = sub.join("0.1.0");
        let cur = sub.join("0.2.0");
        let outside = base.join("outside");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&cur).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let script = format!(
            "printf '%s\\0' '{}' '{}' '{}'",
            old.display(),
            cur.display(),
            outside.display()
        );
        let mut r = make_path_rule(
            "s".into(),
            "s".into(),
            String::new(),
            Category::Other,
            Risk::Confirm,
            true,
            "x".into(),
            "w".into(),
            "c".into(),
            "r".into(),
        );
        r.matcher = Matcher::Script {
            roots: vec![sub.to_string_lossy().into_owned()],
            script: script.clone(),
            timeout_secs: Some(5),
            review: None,
        };
        // 未审查 → 拒绝写入
        assert!(check_rule_safety(&r).is_err());

        if let Matcher::Script { review, .. } = &mut r.matcher {
            *review = Some(crate::model::ScriptReview {
                hash: crate::script::hash(&script),
                note: None,
            });
        }
        assert!(check_rule_safety(&r).is_ok());
        // containment：roots 之下的 2 个被接受，outside 被丢弃
        assert_eq!(expand_rule(&r).len(), 2);

        // 脚本被篡改（hash 不变）→ 拒绝运行
        if let Matcher::Script { script, .. } = &mut r.matcher {
            *script = "echo tampered".into();
        }
        assert!(expand_rule(&r).is_empty());

        // roots 为空 → 拒绝写入（roots 校验先于审查校验）
        if let Matcher::Script { roots, .. } = &mut r.matcher {
            roots.clear();
        }
        assert!(check_rule_safety(&r).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expand_rule_scoped_uses_scope_as_find_dir_root() {
        let base = std::env::temp_dir().join(format!("thin-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let proj = base.join("proj");
        std::fs::create_dir_all(proj.join("target")).unwrap();
        std::fs::write(proj.join("Cargo.toml"), b"").unwrap();

        let mut r = make_path_rule(
            "r".into(),
            "r".into(),
            String::new(),
            Category::DevCache,
            Risk::Safe,
            true,
            "cargo clean".into(),
            "w".into(),
            "c".into(),
            "r".into(),
        );
        r.matcher = Matcher::FindDir {
            roots: vec!["~/thin-marker-does-not-exist".into()],
            dir_name: "target".into(),
            require_sibling: Some("Cargo.toml".into()),
            max_depth: Some(4),
        };
        // 无 scope：规则自身 roots 不存在 → 空
        assert!(expand_rule(&r).is_empty());
        // 有 scope：在 scope 下查找 → 命中 target
        let found = expand_rule_scoped(&r, Some(&proj));
        let want = std::fs::canonicalize(proj.join("target")).unwrap();
        assert_eq!(found, vec![want]);

        let _ = std::fs::remove_dir_all(&base);
    }
}
