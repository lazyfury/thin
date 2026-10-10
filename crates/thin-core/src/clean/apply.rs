//! 执行层：移动/复制/删除原语，系统废纸篓与隔离区的统一入口。
//!
//! **绝不直接 `rm`**：废纸篓走 `Platform::trash_item`，隔离区走 [`super::journal`]。
//! 带 `deny delete` ACL 的目录退化为「只搬内容」，与规则 `reclaim` 语义一致。

use super::journal::{Journal, SkippedItem, describe_move_error, quarantine};
use super::plan::{Plan, plan};
use crate::model::CleanItem;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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

pub(super) fn move_path(src: &Path, dst: &Path) -> Result<()> {
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

/// 一次移动的结果。
pub(super) enum Moved {
    /// 整个路径被移走（原路径不复存在）。
    Whole,
    /// 只搬走了目录内容：原目录因 `deny delete` ACL 无法整体移动，仍在原位。
    Contents {
        bytes: u64,
        failed: Vec<(PathBuf, String)>,
    },
}

/// 把 `src` 移入隔离区。优先整体 `rename`；若目录带 macOS `deny delete` ACL
/// （如 `~/Library/Caches`、`~/Library/Logs`）导致整体移动被拒，则退化为
/// 「只搬内容」——逐个把子项移入 `stored`，原目录留在原位。语义等价于这些
/// 规则 `reclaim` 里写的 `rm -rf <dir>/*`。
pub(super) fn move_into(src: &Path, stored: &Path) -> Result<Moved> {
    match move_path(src, stored) {
        Ok(()) => Ok(Moved::Whole),
        Err(e) if src.is_dir() && is_delete_denied(&e) => {
            std::fs::create_dir_all(stored).context("创建隔离目录失败")?;
            let mut moved = 0usize;
            let mut bytes = 0u64;
            let mut failed = Vec::new();
            let mut first_err: Option<anyhow::Error> = None;
            for entry in std::fs::read_dir(src)
                .with_context(|| format!("读取目录失败: {}", src.display()))?
            {
                let entry = entry?;
                let child = entry.path();
                let dest = stored.join(entry.file_name());
                match move_path(&child, &dest) {
                    Ok(()) => {
                        moved += 1;
                        bytes = bytes.saturating_add(crate::fsutil::size_of(&dest));
                    }
                    Err(ce) => {
                        failed.push((child, describe_move_error(&ce)));
                        first_err.get_or_insert(ce);
                    }
                }
            }
            if moved == 0 {
                // 内容也搬不动，回报原始错误
                return Err(first_err.unwrap_or(e));
            }
            Ok(Moved::Contents { bytes, failed })
        }
        Err(e) => Err(e),
    }
}

/// 废纸篓失败的说明。App 沙盒容器（`~/Library/Containers`、`Group Containers`）
/// 受 TCC 保护，未授予「完全磁盘访问权限」时目录及其内容都无法搬动，
/// 底层只返回 `Operation not permitted`，这里补上可操作的建议。
fn trash_failure_reason(path: &Path) -> String {
    if is_app_container(path) && crate::probe::full_disk_access() == Some(false) {
        return "受 macOS 隐私保护（App 容器）：请在 系统设置 → 隐私与安全性 → 完全磁盘访问权限 中授权后重试".to_string();
    }
    "移入废纸篓失败（权限或路径问题）".to_string()
}

/// App 沙盒容器（受 TCC 保护，需 FDA 才能搬动）。
fn is_app_container(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("/Library/Containers/") || s.contains("/Library/Group Containers/")
}

/// 移动失败是否因为目标带 `deny delete` ACL（EPERM / EACCES）。
fn is_delete_denied(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<std::io::Error>()
                .and_then(|io| io.raw_os_error()),
            Some(libc::EPERM) | Some(libc::EACCES)
        )
    })
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

/// 清理方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 移入系统废纸篓（默认，Finder 可恢复）
    Trash,
    /// 移入 thin 隔离区（`quarantine restore` 可恢复）
    Quarantine,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Trash => "系统废纸篓",
            Mode::Quarantine => "隔离区",
        }
    }
}

/// 默认清理方式：系统废纸篓可用则用它，否则回退隔离区。
pub fn default_mode() -> Mode {
    if crate::platform::platform().trash_available() {
        Mode::Trash
    } else {
        Mode::Quarantine
    }
}

/// 一次清理的结果（废纸篓或隔离区）。
pub enum Applied {
    Trash(TrashReport),
    Quarantine(Journal),
}

impl Applied {
    pub fn mode(&self) -> Mode {
        match self {
            Applied::Trash(_) => Mode::Trash,
            Applied::Quarantine(_) => Mode::Quarantine,
        }
    }

    /// 成功移走的项数
    pub fn moved(&self) -> usize {
        match self {
            Applied::Trash(r) => r.moved_count(),
            Applied::Quarantine(j) => j.entries.len(),
        }
    }

    /// 成功移走的字节数
    pub fn moved_bytes(&self) -> u64 {
        match self {
            Applied::Trash(r) => r.trashed_bytes,
            Applied::Quarantine(j) => j.total_size(),
        }
    }

    /// 被移走项的原路径（供 UI 就地剔除）
    pub fn originals(&self) -> Vec<PathBuf> {
        match self {
            Applied::Trash(r) => {
                let mut v = r.trashed.clone();
                v.extend(r.contents_only.iter().cloned());
                v
            }
            Applied::Quarantine(j) => j.entries.iter().map(|e| e.original.clone()).collect(),
        }
    }

    /// 失败项数（废纸篓专属；隔离区不会中途失败）
    pub fn failed(&self) -> usize {
        match self {
            Applied::Trash(r) => r.failed.len(),
            Applied::Quarantine(_) => 0,
        }
    }

    /// 安全门跳过项数
    pub fn skipped(&self) -> usize {
        match self {
            Applied::Trash(r) => r.skipped.len(),
            Applied::Quarantine(j) => j.skipped.len(),
        }
    }

    /// 隔离会话 id（仅隔离区模式）
    pub fn session(&self) -> Option<&str> {
        match self {
            Applied::Trash(_) => None,
            Applied::Quarantine(j) => Some(&j.session),
        }
    }
}

/// 按指定方式执行清理（内部复用同一安全门）。
pub fn apply(items: &[CleanItem], mode: Mode) -> Result<Applied> {
    match mode {
        Mode::Trash => trash(items).map(Applied::Trash),
        Mode::Quarantine => quarantine(items, false).map(Applied::Quarantine),
    }
}

/// 移入系统废纸篓的结果
#[derive(Debug, Default, Clone)]
pub struct TrashReport {
    /// 整体移入废纸篓的项
    pub trashed: Vec<PathBuf>,
    /// 原目录带 macOS `deny delete` ACL、无法整体移动，只移入了内容的项
    /// （目录仍在原位，语义等价于 `rm -rf <dir>/*`）
    pub contents_only: Vec<PathBuf>,
    pub trashed_bytes: u64,
    pub failed: Vec<(PathBuf, String)>,
    pub skipped: Vec<SkippedItem>,
}

impl TrashReport {
    /// 成功处理的项数（整体移入 + 仅移入内容）
    pub fn moved_count(&self) -> usize {
        self.trashed.len() + self.contents_only.len()
    }

    /// 其中「仅移入内容」的项数
    pub fn contents_count(&self) -> usize {
        self.contents_only.len()
    }
}

/// 把候选项移入系统废纸篓（Finder 可恢复）。
///
/// 与隔离区共用同一安全门（[`plan`]）；需要 Swift 后端（`FileManager.trashItem`），
/// 后端不可用时整体报错，**绝不回退到直接 `rm`**。
pub fn trash(items: &[CleanItem]) -> Result<TrashReport> {
    trash_with(crate::platform::platform(), items)
}

/// 内部实现：注入 [`crate::platform::Platform`]，便于测试。
pub fn trash_with(
    platform: &dyn crate::platform::Platform,
    items: &[CleanItem],
) -> Result<TrashReport> {
    trash_plan_with(platform, plan(items))
}

/// 按已算好的计划移入系统废纸篓（提权路径复用同一执行逻辑）。
fn trash_plan_with(platform: &dyn crate::platform::Platform, plan: Plan) -> Result<TrashReport> {
    let Plan {
        approved,
        sudo,
        skipped,
    } = plan;
    let mut report = TrashReport {
        skipped,
        ..Default::default()
    };
    let moveable: Vec<CleanItem> = approved.into_iter().chain(sudo).collect();
    for it in &moveable {
        match platform.trash_item(&it.path) {
            Some(true) => {
                report.trashed_bytes = report.trashed_bytes.saturating_add(it.size);
                report.trashed.push(it.path.clone());
            }
            // 目录带 macOS `deny delete` ACL 时（`~/Library/Caches`、`~/Library/Logs` 等）
            // 无法整体移入废纸篓；退化为「只搬内容」，原目录留在原位，
            // 语义等价于这些规则 reclaim 里写的 `rm -rf <dir>/*`。
            Some(false) if it.path.is_dir() => {
                let (moved, bytes, failed) = trash_contents(platform, &it.path);
                if moved == 0 {
                    report
                        .failed
                        .push((it.path.clone(), trash_failure_reason(&it.path)));
                } else {
                    report.trashed_bytes = report.trashed_bytes.saturating_add(bytes);
                    report.contents_only.push(it.path.clone());
                }
                report.failed.extend(failed);
            }
            Some(false) => report
                .failed
                .push((it.path.clone(), trash_failure_reason(&it.path))),
            None => anyhow::bail!("Swift 后端不可用，无法使用系统废纸篓；请改用默认隔离区模式"),
        }
    }
    Ok(report)
}

/// 把目录的直接子项逐个移入系统废纸篓，返回（成功数、字节数、失败子项）。
///
/// 仅用于带 `deny delete` ACL 的目录：目录本身无法整体移动，但规则要清的正是
/// 它的内容（等价 `rm -rf <dir>/*`）。只下一层，与规则语义一致。
fn trash_contents(
    platform: &dyn crate::platform::Platform,
    dir: &Path,
) -> (usize, u64, Vec<(PathBuf, String)>) {
    let mut moved = 0usize;
    let mut bytes = 0u64;
    let mut failed = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            failed.push((dir.to_path_buf(), format!("读取目录失败: {e}")));
            return (0, 0, failed);
        }
    };
    for entry in entries.flatten() {
        let child = entry.path();
        // 先量体积，移走后路径即不可访问
        let size = crate::fsutil::size_of(&child);
        match platform.trash_item(&child) {
            Some(true) => {
                moved += 1;
                bytes = bytes.saturating_add(size);
            }
            Some(false) => {
                let reason = trash_failure_reason(&child);
                failed.push((child, reason));
            }
            None => {
                failed.push((child, "Swift 后端不可用".to_string()));
                break;
            }
        }
    }
    (moved, bytes, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_app_container_paths() {
        assert!(is_app_container(Path::new(
            "/Users/x/Library/Containers/com.foo.bar"
        )));
        assert!(is_app_container(Path::new(
            "/Users/x/Library/Group Containers/TEAM.group.com.foo"
        )));
        assert!(!is_app_container(Path::new(
            "/Users/x/Library/Application Support/com.foo.bar"
        )));
    }
}
