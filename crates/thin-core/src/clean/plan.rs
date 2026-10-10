//! 清理计划：预演与执行使用**同一套**安全门，保证「预览即所得」。

use super::journal::{SkippedItem, thin_home};
use super::policy::protection_reason_in;
use crate::model::CleanItem;
use serde::Serialize;
use std::path::Path;

/// 清理计划：预演与执行使用**同一套**安全门，保证「预览即所得」。
#[derive(Debug, Default, Serialize)]
pub struct Plan {
    pub approved: Vec<CleanItem>,
    /// 通过除 `sudo` 标记外全部安全门、但当前身份无权移动的项。
    /// 仅由 [`plan_elevated_in`] 填充，供提权子进程复核后移动。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sudo: Vec<CleanItem>,
    pub skipped: Vec<SkippedItem>,
}

impl Plan {
    pub fn approved_bytes(&self) -> u64 {
        self.approved.iter().map(|i| i.size).sum()
    }

    /// 提权后可移动的项（普通项 + sudo 项）。
    pub(super) fn moveable(&self) -> impl Iterator<Item = &CleanItem> {
        self.approved.iter().chain(self.sudo.iter())
    }
}

/// 对候选项套用安全门（存在性 / sudo / 受保护路径 / 卷隔离），不落盘。
pub fn plan(items: &[CleanItem]) -> Plan {
    plan_in(&thin_home(), items)
}

/// 内部实现（可指定数据目录，便于测试）
pub fn plan_in(home: &Path, items: &[CleanItem]) -> Plan {
    plan_impl(home, items, false)
}

/// 提权清理的规划：与 [`plan_in`] **同一套**安全门，但不再因 `sudo` 标记跳过，
/// 而是把需要 root 的项放进 [`Plan::sudo`]，交由 root 子进程复核后移动。
pub fn plan_elevated_in(home: &Path, items: &[CleanItem]) -> Plan {
    plan_impl(home, items, true)
}

fn plan_impl(home: &Path, items: &[CleanItem], allow_sudo: bool) -> Plan {
    let mut p = Plan::default();
    for it in items {
        if !it.path.exists() {
            p.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason: "路径不存在".into(),
            });
            continue;
        }
        if it.sudo && !allow_sudo {
            p.skipped.push(SkippedItem {
                path: it.path.clone(),
                reason: "需要 sudo，请手动处理".into(),
            });
            continue;
        }
        // 无论是否提权，保护名单与静态保护 / 卷隔离都必须先通过。
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
        if it.sudo {
            p.sudo.push(it.clone());
        } else {
            p.approved.push(it.clone());
        }
    }
    p
}
