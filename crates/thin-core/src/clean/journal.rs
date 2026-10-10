//! 隔离区账本：`Journal` 结构、移入隔离、恢复与永久清除。
//!
//! 默认不真正删除，而是把目标移入 `~/.thin/quarantine/<session>/payload`，
//! 写入 `journal.json`，可随时 `restore`。只有显式 `purge` 才永久删除。

use super::apply::{Moved, move_into, move_path};
use super::plan::{Plan, plan_in};
use super::policy::user_home;
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
    /// true：原目录带 macOS `deny delete` ACL，无法整体移动，只搬走了它的内容
    /// （原目录仍在原位，恢复时把内容移回）。默认 false = 整体移动。
    #[serde(default)]
    pub contents_only: bool,
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

/// 把选中的项移入隔离区，返回账本。`dry_run=true` 时只生成计划不落盘。
pub fn quarantine(items: &[CleanItem], dry_run: bool) -> Result<Journal> {
    quarantine_into(&thin_home(), items, dry_run)
}

/// 内部实现（可指定数据目录，便于测试）
pub fn quarantine_into(home: &Path, items: &[CleanItem], dry_run: bool) -> Result<Journal> {
    quarantine_plan_into(home, plan_in(home, items), dry_run)
}

/// 按已算好的计划移入隔离区（提权路径复用同一执行逻辑）。
pub fn quarantine_plan_into(home: &Path, mut plan: Plan, dry_run: bool) -> Result<Journal> {
    let session = now_session_id();
    let session_dir = home.join("quarantine").join(&session);
    let payload = session_dir.join("payload");

    let skipped = std::mem::take(&mut plan.skipped);
    let mut journal = Journal {
        session: session.clone(),
        created_at: now_secs(),
        dry_run,
        entries: Vec::new(),
        skipped,
    };

    for (i, it) in plan.moveable().enumerate() {
        let base = it
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("item{i}"));
        let stored = payload.join(format!("{:04}-{}", i, sanitize(&base)));

        let mut size = it.size;
        let mut contents_only = false;
        if !dry_run {
            match move_into(&it.path, &stored) {
                Ok(Moved::Whole) => {}
                Ok(Moved::Contents { bytes, failed }) => {
                    // 只搬了内容：如实记录实际搬走的体积，失败子项单独记入跳过。
                    size = bytes;
                    contents_only = true;
                    for (path, reason) in failed {
                        journal.skipped.push(SkippedItem { path, reason });
                    }
                }
                Err(e) => {
                    journal.skipped.push(SkippedItem {
                        path: it.path.clone(),
                        reason: describe_move_error(&e),
                    });
                    continue;
                }
            }
        }

        journal.entries.push(JournalEntry {
            index: i,
            original: it.path.clone(),
            stored,
            size,
            rule_id: it.rule_id.clone(),
            name: it.name.clone(),
            risk: it.risk,
            contents_only,
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
pub(super) fn describe_move_error(e: &anyhow::Error) -> String {
    let is_eperm = e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .and_then(|io| io.raw_os_error())
            == Some(libc::EPERM)
    });
    let msg = format!("{e:#}");
    if is_eperm {
        format!(
            "{msg}\n    提示：该目录受 macOS 隐私保护（TCC，App 沙盒容器常见）。请在 系统设置 → 隐私与安全 → 完全磁盘访问权限 中勾选你的终端 App（thin 随终端继承权限）后重试。"
        )
    } else {
        msg
    }
}

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
        if entry.contents_only {
            // 原目录还在（只搬了内容）：把隔离内容逐个移回原目录。
            std::fs::create_dir_all(&entry.original)
                .with_context(|| format!("创建目录失败: {}", entry.original.display()))?;
            let mut restored_any = false;
            for child in std::fs::read_dir(&entry.stored)? {
                let child = child?;
                let dest = entry.original.join(child.file_name());
                if dest.exists() {
                    report.conflicts.push(dest);
                    continue;
                }
                move_path(&child.path(), &dest)?;
                restored_any = true;
            }
            if restored_any {
                report.restored += 1;
            }
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
