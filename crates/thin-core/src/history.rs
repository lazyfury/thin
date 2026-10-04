//! 清理历史记录（History）。
//!
//! 每次实际清理（手动或定时）追加一条 JSONL 记录，便于回答
//! 「什么时候、按什么预设、清理了多少、回收了多少」。
//! 与隔离区 Journal 互补：Journal 是恢复用的明细账，History 是运行索引。

use crate::clean;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一次清理运行的历史记录
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub timestamp: u64,
    /// manual | schedule | uninstall | dupes
    pub trigger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// 候选数量（安全门前）
    pub scanned: usize,
    /// 通过安全门的数量
    pub approved: usize,
    /// 实际移入隔离区的数量与字节
    pub moved: usize,
    pub moved_bytes: u64,
    /// 被安全门/移动失败跳过的数量
    pub skipped: usize,
    /// 本次顺带永久删除的旧隔离会话
    #[serde(default)]
    pub purged_sessions: usize,
    #[serde(default)]
    pub purged_bytes: u64,
}

impl Record {
    pub fn new(trigger: &str) -> Self {
        Record {
            timestamp: now_secs(),
            trigger: trigger.to_string(),
            preset: None,
            session: None,
            scanned: 0,
            approved: 0,
            moved: 0,
            moved_bytes: 0,
            skipped: 0,
            purged_sessions: 0,
            purged_bytes: 0,
        }
    }
}

pub fn history_path() -> PathBuf {
    clean::thin_home().join("history.jsonl")
}

/// 把 Unix 时间戳格式化为本地时间字符串
pub fn format_ts(ts: u64) -> String {
    unsafe {
        let t = ts as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return ts.to_string();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// 追加一条历史记录
pub fn append(record: &Record) -> Result<()> {
    let path = history_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开历史文件失败: {}", path.display()))?;
    writeln!(f, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

/// 读取历史记录（按时间倒序 = 文件行倒序）；`limit` 为 None 表示全部
pub fn load(limit: Option<usize>) -> Result<Vec<Record>> {
    let path = history_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取历史失败: {}", path.display()))?;
    let mut out: Vec<Record> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Record>(l).ok())
        .collect();
    out.reverse();
    if let Some(n) = limit {
        out.truncate(n);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serialization_roundtrip() {
        let mut r = Record::new("schedule");
        r.preset = Some("weekly".into());
        r.moved = 3;
        r.moved_bytes = 4096;
        let line = serde_json::to_string(&r).unwrap();
        let back: Record = serde_json::from_str(&line).unwrap();
        assert_eq!(back.trigger, "schedule");
        assert_eq!(back.preset.as_deref(), Some("weekly"));
        assert_eq!(back.moved_bytes, 4096);
        assert_eq!(back.timestamp, r.timestamp);
    }

    #[test]
    fn format_ts_is_readable() {
        let s = format_ts(0);
        assert!(s.contains('-') && s.contains(':'), "got {s}");
    }
}
