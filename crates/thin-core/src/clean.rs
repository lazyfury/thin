//! 安全清理引擎（M1）。
//!
//! 设计原则：**默认不真正删除**，而是把目标「移入隔离区」，写入 Journal，可随时恢复。
//! 只有显式 purge 才会永久删除。

use crate::model::{CleanItem, Risk};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// 隔离区中一条记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub index: usize,
    pub original: PathBuf,
    pub stored: PathBuf,
    pub size: u64,
    pub rule_id: String,
    pub name: String,
    pub risk: Risk,
}

/// 被跳过的项及原因
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedItem {
    pub path: PathBuf,
    pub reason: String,
}

/// 一次清理会话的账本
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub session: String,
    pub created_at: u64,
    #[serde(default)]
    pub dry_run: bool,
    pub entries: Vec<JournalEntry>,
    #[serde(default)]
    pub skipped: Vec<SkippedItem>,
}

impl Journal {
    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    pub fn session_dir(&self, home: &Path) -> PathBuf {
        home.join("quarantine").join(&self.session)
    }
}

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

fn user_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// thin 的数据目录（隔离区/账本），支持 THIN_HOME 覆盖（便于测试）
pub fn thin_home() -> PathBuf {
    if let Ok(p) = std::env::var("THIN_HOME")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    user_home()
        .map(|h| h.join(".thin"))
        .unwrap_or_else(|| PathBuf::from(".thin"))
}

pub fn quarantine_root() -> PathBuf {
    thin_home().join("quarantine")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_session_id() -> String {
    if let Some(o) = crate::proc::output_with_timeout(
        "date",
        &["+%Y%m%d-%H%M%S"],
        std::time::Duration::from_secs(2),
    ) {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    format!("{}", now_secs())
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c == '/' || c == ':' { '_' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------
// 安全门（SafetyGate）
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// 移动 / 复制 / 删除
// ---------------------------------------------------------------------------

fn available_bytes(path: &Path) -> Option<u64> {
    let p = if path.exists() { path } else { path.parent()? };
    let c = std::ffi::CString::new(p.to_string_lossy().into_owned()).ok()?;
    unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        Some((st.f_bavail as u64).saturating_mul(st.f_bsize as u64))
    }
}

fn move_path(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).context("创建目标目录失败")?;
    }
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            // 跨文件系统：复制前先确认空间足够，再复制后删除
            let need = crate::fsutil::logical_size(src);
            if let Some(avail) = available_bytes(dst)
                && avail < need
            {
                anyhow::bail!(
                    "目标卷空间不足：需要 {}，可用 {}",
                    crate::fmt::human(need),
                    crate::fmt::human(avail)
                );
            }
            copy_recursive(src, dst)?;
            remove_path(src)?;
            Ok(())
        }
        Err(e) => {
            Err(e).with_context(|| format!("移动失败: {} -> {}", src.display(), dst.display()))
        }
    }
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 清理计划（SafetyGate）
// ---------------------------------------------------------------------------

/// 清理计划：预演与执行使用**同一套**安全门，保证「预览即所得」。
#[derive(Debug, Default, Serialize)]
pub struct Plan {
    pub approved: Vec<CleanItem>,
    pub skipped: Vec<SkippedItem>,
}

impl Plan {
    pub fn approved_bytes(&self) -> u64 {
        self.approved.iter().map(|i| i.size).sum()
    }
}

/// 对候选项套用安全门（存在性 / sudo / 受保护路径 / 卷隔离），不落盘。
pub fn plan(items: &[CleanItem]) -> Plan {
    plan_in(&thin_home(), items)
}

/// 内部实现（可指定数据目录，便于测试）
pub fn plan_in(home: &Path, items: &[CleanItem]) -> Plan {
    let mut p = Plan::default();
    for it in items {
        if !it.path.exists() {
            p.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason: "路径不存在".into(),
            });
            continue;
        }
        if it.sudo {
            p.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason: "需要 sudo，请手动处理".into(),
            });
            continue;
        }
        if crate::protect::is_protected_in(home, &it.path) {
            p.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason: "已在保护名单（thin protect）".into(),
            });
            continue;
        }
        if let Some(reason) = protection_reason_in(&it.path, Some(home)) {
            p.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason,
            });
            continue;
        }
        p.approved.push(it.clone());
    }
    p
}

// ---------------------------------------------------------------------------
// 隔离（quarantine）
// ---------------------------------------------------------------------------

/// 把选中的项移入隔离区，返回账本。`dry_run=true` 时只生成计划不落盘。
pub fn quarantine(items: &[CleanItem], dry_run: bool) -> Result<Journal> {
    quarantine_into(&thin_home(), items, dry_run)
}

/// 内部实现（可指定数据目录，便于测试）
pub fn quarantine_into(home: &Path, items: &[CleanItem], dry_run: bool) -> Result<Journal> {
    let session = now_session_id();
    let session_dir = home.join("quarantine").join(&session);
    let payload = session_dir.join("payload");

    // 与 dry-run 使用同一安全门：被跳过项与预览一致
    let plan = plan_in(home, items);
    let mut journal = Journal {
        session: session.clone(),
        created_at: now_secs(),
        dry_run,
        entries: Vec::new(),
        skipped: plan.skipped,
    };

    for (i, it) in plan.approved.iter().enumerate() {
        let base = it
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("item{i}"));
        let stored = payload.join(format!("{:04}-{}", i, sanitize(&base)));

        if !dry_run && let Err(e) = move_path(&it.path, &stored) {
            journal.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason: describe_move_error(&e),
            });
            continue;
        }

        journal.entries.push(JournalEntry {
            index: i,
            original: it.path.clone(),
            stored,
            size: it.size,
            rule_id: it.rule_id.clone(),
            name: it.name.clone(),
            risk: it.risk,
        });
    }

    if !dry_run && !journal.entries.is_empty() {
        std::fs::create_dir_all(&session_dir).context("创建会话目录失败")?;
        let path = session_dir.join("journal.json");
        let json = serde_json::to_string_pretty(&journal)?;
        std::fs::write(&path, json).context("写入账本失败")?;
    }
    Ok(journal)
}

/// 把移动失败转成可操作的建议。
///
/// macOS 上 `~/Library/Caches` 等目录整体搬迁常因 TCC（隐私保护）返回
/// `EPERM: Operation not permitted`，底层 errno 对用户毫无指引，这里补上。
fn describe_move_error(e: &anyhow::Error) -> String {
    let is_eperm = e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .and_then(|io| io.raw_os_error())
            == Some(libc::EPERM)
    });
    let msg = format!("{e:#}");
    if is_eperm {
        format!(
            "{msg}\n    提示：该目录受 macOS 隐私保护（TCC）。请在 系统设置 → 隐私与安全 → 完全磁盘访问权限 中授权 thin，或改清其内部的具体子项。"
        )
    } else {
        msg
    }
}

// ---------------------------------------------------------------------------
// 账本读取 / 恢复 / 清除
// ---------------------------------------------------------------------------

fn load_journal_at(session_dir: &Path) -> Result<Journal> {
    let path = session_dir.join("journal.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取账本失败: {}", path.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

/// 列出所有隔离会话（按创建时间倒序）
pub fn list_journals() -> Result<Vec<Journal>> {
    list_journals_in(&thin_home())
}

pub fn list_journals_in(home: &Path) -> Result<Vec<Journal>> {
    let root = home.join("quarantine");
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        if let Ok(j) = load_journal_at(&entry.path()) {
            out.push(j);
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.created_at));
    Ok(out)
}

/// 恢复报告
#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub missing: Vec<PathBuf>,
    pub conflicts: Vec<PathBuf>,
}

/// 恢复某个会话：把隔离区内容移回原位置
pub fn restore_session(session: &str) -> Result<RestoreReport> {
    restore_session_in(&thin_home(), session)
}

pub fn restore_session_in(home: &Path, session: &str) -> Result<RestoreReport> {
    let session_dir = home.join("quarantine").join(session);
    let journal = load_journal_at(&session_dir)?;
    let mut report = RestoreReport::default();

    for entry in &journal.entries {
        if !entry.stored.exists() {
            report.missing.push(entry.original.clone());
            continue;
        }
        if entry.original.exists() {
            report.conflicts.push(entry.original.clone());
            continue;
        }
        if let Some(parent) = entry.original.parent() {
            std::fs::create_dir_all(parent)?;
        }
        move_path(&entry.stored, &entry.original)?;
        report.restored += 1;
    }

    // 若已全部还原，移除会话目录
    if report.restored == journal.entries.len() {
        let _ = std::fs::remove_dir_all(&session_dir);
    }
    Ok(report)
}

/// 永久删除某个会话的隔离内容，返回释放的字节数
pub fn purge_session(session: &str) -> Result<u64> {
    purge_session_in(&thin_home(), session)
}

pub fn purge_session_in(home: &Path, session: &str) -> Result<u64> {
    let session_dir = home.join("quarantine").join(session);
    let freed = load_journal_at(&session_dir)
        .map(|j| j.total_size())
        .unwrap_or(0);
    std::fs::remove_dir_all(&session_dir)
        .with_context(|| format!("删除会话失败: {}", session_dir.display()))?;
    Ok(freed)
}

/// 早于 `days` 天的隔离会话（供 purge 预览与执行共用）
pub fn sessions_older_than(days: u64) -> Result<Vec<Journal>> {
    sessions_older_than_in(&thin_home(), days)
}

pub fn sessions_older_than_in(home: &Path, days: u64) -> Result<Vec<Journal>> {
    let cutoff = now_secs().saturating_sub(days.saturating_mul(86_400));
    Ok(list_journals_in(home)?
        .into_iter()
        .filter(|j| j.created_at < cutoff)
        .collect())
}

/// 永久删除早于 `days` 天的隔离会话，返回 (会话数, 释放字节)
pub fn purge_older_than(days: u64) -> Result<(usize, u64)> {
    purge_older_than_in(&thin_home(), days)
}

pub fn purge_older_than_in(home: &Path, days: u64) -> Result<(usize, u64)> {
    let mut count = 0;
    let mut freed = 0;
    for j in sessions_older_than_in(home, days)? {
        freed += purge_session_in(home, &j.session)?;
        count += 1;
    }
    Ok((count, freed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Category;

    fn item(path: PathBuf, size: u64) -> CleanItem {
        CleanItem {
            rule_id: "test".into(),
            name: "测试项".into(),
            path,
            category: Category::DevCache,
            risk: Risk::Safe,
            regenerable: true,
            sudo: false,
            size,
            reclaim: "手动删除".into(),
            explain: crate::model::Explain {
                what: "测试".into(),
                cost: "无".into(),
                recover: "重新生成".into(),
            },
            protected: false,
            protected_reason: None,
        }
    }

    #[test]
    fn protected_paths() {
        assert!(protection_reason(Path::new("/")).is_some());
        assert!(protection_reason(Path::new("/System/Library")).is_some());
        assert!(protection_reason(Path::new("/private/var/vm/sleepimage")).is_some());
        assert!(protection_reason(Path::new("/Library/Apple/Support")).is_some());
        let home = user_home().unwrap();
        assert!(protection_reason(&home).is_some());
        // 裸顶层根：整个目录不可删
        assert!(protection_reason(Path::new("/Applications")).is_some());
        assert!(protection_reason(Path::new("/Library")).is_some());
        assert!(protection_reason(Path::new("/private/var")).is_some());
        assert!(protection_reason(Path::new("/usr")).is_some());
        // 裸顶层根即使位于允许清单内，也不能删除目录本身；其子项仍可清理
        assert!(protection_reason(Path::new("/Library/Logs")).is_some());
        assert!(protection_reason(Path::new("/Library/Logs/DiagnosticReports")).is_none());
        assert!(protection_reason(Path::new("/usr/local")).is_some());
        assert!(protection_reason(Path::new("/usr/local/lib/foo")).is_none());
        // 个人目录顶层：整体不可清，但其内部具体缓存/产物允许
        assert!(protection_reason(&home.join("Documents")).is_some());
        assert!(protection_reason(&home.join("Downloads")).is_some());
        assert!(protection_reason(&home.join("Library")).is_some());
        assert!(protection_reason(&home.join("Documents/proj/node_modules")).is_none());
        // 普通文件不受保护
        assert!(protection_reason(Path::new("/tmp/thin-test-nonexistent")).is_none());
    }

    #[test]
    fn rejects_dangerous_shapes() {
        assert!(protection_reason(Path::new("/tmp/../etc/passwd")).is_some());
        assert!(protection_reason(Path::new("/tmp/a\nb")).is_some());
        assert!(protection_reason(Path::new("/private/var/db/other")).is_some());
        // 允许清单内的可重建缓存仍可清理
        assert!(protection_reason(Path::new("/tmp/thin-nonexistent")).is_none());
    }

    #[test]
    fn plan_matches_real_skip_reasons() {
        let base = std::env::temp_dir().join(format!("thin-plan-{}", std::process::id()));
        let data_home = base.join("data");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let target = work.join("cache");
        std::fs::create_dir_all(&target).unwrap();

        let mut sudo_item = item(target.clone(), 10);
        sudo_item.sudo = true;
        let missing = item(work.join("nope"), 10);
        let protected = item(PathBuf::from("/System/Library"), 10);
        let ok = item(target.clone(), 1024);

        let candidates = vec![sudo_item.clone(), missing, protected, ok];
        let p = plan_in(&data_home, &candidates);
        assert_eq!(p.approved.len(), 1, "只有合法项应通过");
        assert_eq!(p.approved_bytes(), 1024);
        assert_eq!(p.skipped.len(), 3);

        // 真实执行使用同一安全门：跳过项数量一致
        let j = quarantine_into(&data_home, &candidates, false).unwrap();
        assert_eq!(j.entries.len(), p.approved.len());
        assert_eq!(j.skipped.len(), p.skipped.len());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn quarantine_and_restore_roundtrip() {
        let base = std::env::temp_dir().join(format!("thin-test-{}", std::process::id()));
        let data_home = base.join("data");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();

        // 造一个待清理目录
        let target = work.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.bin"), vec![0u8; 1024]).unwrap();

        let j = quarantine_into(&data_home, &[item(target.clone(), 1024)], false).unwrap();
        assert_eq!(j.entries.len(), 1);
        assert!(!target.exists(), "原路径应已被移走");
        assert!(j.entries[0].stored.exists());

        // 能列出
        let list = list_journals_in(&data_home).unwrap();
        assert_eq!(list.len(), 1);

        // 恢复
        let report = restore_session_in(&data_home, &j.session).unwrap();
        assert_eq!(report.restored, 1);
        assert!(target.exists(), "恢复后原路径应存在");
        assert!(target.join("a.bin").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn purge_removes_session() {
        let base = std::env::temp_dir().join(format!("thin-purge-{}", std::process::id()));
        let data_home = base.join("data");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let f = work.join("cache.bin");
        std::fs::write(&f, vec![0u8; 2048]).unwrap();

        let j = quarantine_into(&data_home, &[item(f.clone(), 2048)], false).unwrap();
        let freed = purge_session_in(&data_home, &j.session).unwrap();
        assert_eq!(freed, 2048);
        assert!(!f.exists());
        assert!(list_journals_in(&data_home).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }
}
