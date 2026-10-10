//! 遍历进度与取消。
//!
//! `thin-fs` 不依赖 `thin-core`，因此这里只定义最小的 [`ProgressSink`] trait，
//! 由上层（`thin-core::progress::Progress`）实现适配，避免 crate 之间成环。

use std::sync::atomic::{AtomicBool, Ordering};

/// 遍历进度上报接口（全部有默认空实现）。
pub trait ProgressSink: Send + Sync {
    /// 设置当前阶段标签（如「统计目录占用」）。
    fn set_label(&self, _label: &str) {}
    /// 设置总量（未知时可略过）。
    fn set_total(&self, _total: u64) {}
    /// 前进一格。
    fn inc(&self) {}
    /// 有活动但不推进总量（用于流式遍历）。
    fn touch(&self) {}
}

/// 遍历控制：进度上报 + 取消。
#[derive(Clone, Copy, Default)]
pub struct Control<'a> {
    /// 进度回调；`None` 表示不需要上报。
    pub progress: Option<&'a dyn ProgressSink>,
    /// 取消标志；`None` 表示不可取消。
    pub cancel: Option<&'a AtomicBool>,
}

impl<'a> Control<'a> {
    /// 无进度、不可取消。
    pub fn none() -> Self {
        Self::default()
    }

    /// 仅带进度。
    pub fn with_progress(progress: &'a dyn ProgressSink) -> Self {
        Self {
            progress: Some(progress),
            cancel: None,
        }
    }

    /// 是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub(crate) fn touch(&self) {
        if let Some(p) = self.progress {
            p.touch();
        }
    }
}
