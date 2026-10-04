//! 安全清理引擎（M1）。
//!
//! 设计原则：**默认不真正删除**，而是把目标「移入隔离区」，写入 Journal，可随时恢复。
//! 只有显式 purge 才会永久删除。

use crate::model::{CleanItem, Risk};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    if let Ok(p) = std::env::var("THIN_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
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
    if let Ok(o) = std::process::Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
    {
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
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let home = user_home();

    // 精确匹配：根目录与整个主目录
    if canon == Path::new("/") {
        return Some("根目录".into());
    }
    if let Some(h) = &home {
        if canon == *h {
            return Some("整个用户主目录".into());
        }
    }

    // 子树保护
    let mut subtrees: Vec<PathBuf> = vec![
        PathBuf::from("/System"),
        PathBuf::from("/private/var/vm"),
        PathBuf::from("/Library/Keychains"),
        PathBuf::from("/Library/Apple"),
        PathBuf::from("/Library/CloudStorage"),
    ];
    if let Some(h) = &home {
        subtrees.push(h.join("Library/Keychains"));
        subtrees.push(h.join("Library/Mobile Documents"));
        subtrees.push(h.join("Library/CloudStorage"));
        subtrees.push(h.join(".thin"));
    }
    for p in subtrees {
        if canon == p || canon.starts_with(&p) {
            return Some(format!("受保护路径 {}", p.display()));
        }
    }

    // 挂载点保护（路径自身是挂载点，与父目录设备号不同）
    if let Some(parent) = canon.parent() {
        if let (Ok(a), Ok(b)) = (std::fs::metadata(&canon), std::fs::metadata(parent)) {
            use std::os::unix::fs::MetadataExt;
            if a.dev() != b.dev() {
                return Some("是挂载点".into());
            }
        }
    }

    // 卷隔离：目标必须与隔离区同卷，否则拒绝（外接盘 / 其他挂载）
    if let Some(vol) = ref_vol {
        if let (Some(a), Some(b)) = (volume_device(&canon), volume_device(vol)) {
            if a != b {
                return Some(format!(
                    "位于不同卷（外接盘/其他挂载），不在隔离区所在卷 {}",
                    vol.display()
                ));
            }
        }
    }
    None
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
            if let Some(avail) = available_bytes(dst) {
                if avail < need {
                    anyhow::bail!(
                        "目标卷空间不足：需要 {}，可用 {}",
                        crate::fmt::human(need),
                        crate::fmt::human(avail)
                    );
                }
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

    let mut journal = Journal {
        session: session.clone(),
        created_at: now_secs(),
        dry_run,
        entries: Vec::new(),
        skipped: Vec::new(),
    };

    for (i, it) in items.iter().enumerate() {
        if !it.path.exists() {
            journal.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason: "路径不存在".into(),
            });
            continue;
        }
        if it.sudo {
            journal.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason: "需要 sudo，请手动处理".into(),
            });
            continue;
        }
        if let Some(reason) = protection_reason_in(&it.path, Some(home)) {
            journal.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason,
            });
            continue;
        }

        let base = it
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("item{i}"));
        let stored = payload.join(format!("{:04}-{}", i, sanitize(&base)));

        if !dry_run {
            if let Err(e) = move_path(&it.path, &stored) {
                journal.skipped.push(SkippedItem {
                    path: it.path.clone(),
                    reason: format!("{e:#}"),
                });
                continue;
            }
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
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
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

/// 永久删除早于 `days` 天的隔离会话，返回 (会话数, 释放字节)
pub fn purge_older_than(days: u64) -> Result<(usize, u64)> {
    purge_older_than_in(&thin_home(), days)
}

pub fn purge_older_than_in(home: &Path, days: u64) -> Result<(usize, u64)> {
    let cutoff = now_secs().saturating_sub(days * 86_400);
    let mut count = 0;
    let mut freed = 0;
    for j in list_journals_in(home)? {
        if j.created_at < cutoff {
            freed += purge_session_in(home, &j.session)?;
            count += 1;
        }
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
        // 普通文件不受保护
        assert!(protection_reason(Path::new("/tmp/thin-test-nonexistent")).is_none());
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
