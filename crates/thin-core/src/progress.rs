//! 轻量进度上报（无 UI 依赖）。
//!
//! 后台线程更新，UI 线程读取快照即可渲染 Gauge / spinner。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Progress {
    total: AtomicU64,
    done: AtomicU64,
    /// 不确定进度时的已处理计数
    count: AtomicU64,
    label: Mutex<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProgressSnapshot {
    pub done: u64,
    pub total: u64,
    pub count: u64,
    pub label: String,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_total(&self, n: u64) {
        self.total.store(n, Ordering::Relaxed);
    }

    pub fn set_label(&self, s: impl Into<String>) {
        if let Ok(mut l) = self.label.lock() {
            *l = s.into();
        }
    }

    /// 完成一个单位
    pub fn inc(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    /// 不确定进度：仅计入「已处理数量」
    pub fn touch(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// 进入下一个阶段（清零 done/count，保留 label 由调用方重设）
    pub fn reset(&self) {
        self.done.store(0, Ordering::Relaxed);
        self.count.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            count: self.count.load(Ordering::Relaxed),
            label: self.label.lock().map(|l| l.clone()).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determinate_and_reset() {
        let p = Progress::new();
        p.set_label("阶段一");
        p.set_total(3);
        p.inc();
        p.inc();
        let s = p.snapshot();
        assert_eq!(s.done, 2);
        assert_eq!(s.total, 3);
        assert_eq!(s.label, "阶段一");

        p.reset();
        p.set_label("阶段二");
        p.set_total(1);
        let s = p.snapshot();
        assert_eq!(s.done, 0);
        assert_eq!(s.total, 1);
        assert_eq!(s.label, "阶段二");
    }

    #[test]
    fn indeterminate_count() {
        let p = Progress::new();
        for _ in 0..5 {
            p.touch();
        }
        assert_eq!(p.snapshot().count, 5);
        assert_eq!(p.snapshot().total, 0);
    }
}
