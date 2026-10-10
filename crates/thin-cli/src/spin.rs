//! 纯文本 CLI 的轻量进度：在 stderr 上实时渲染 [`thin_core::progress::Progress`]。
//!
//! 与 TUI 的进度渲染共用同一个 `Progress`（原子计数），但这里只画一行 spinner +
//! 计数，不引入额外依赖。命令结束后清行，避免污染正常输出。

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thin_core::progress::Progress;

const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// 运行 `f`，其间在 stderr 上把 `Progress` 快照渲染成 spinner。
///
/// `f` 收到同一个 `Progress`，应把它传给 `*_progress` 版本的扫描/统计函数。
pub(crate) fn with_progress<T>(label: &str, f: impl FnOnce(&Progress) -> T) -> T {
    // 非终端（管道 / 重定向）：不画 spinner，避免污染日志与脚本输出。
    if !std::io::stderr().is_terminal() {
        return f(&Progress::new());
    }

    let progress = Arc::new(Progress::new());
    let done = Arc::new(AtomicBool::new(false));
    let (p, d) = (progress.clone(), done.clone());
    let label = label.to_string();

    let handle = std::thread::spawn(move || {
        let mut i = 0usize;
        while !d.load(Ordering::Relaxed) {
            let snap = p.snapshot();
            let frame = FRAMES[i % FRAMES.len()];
            if snap.total > 0 {
                eprint!(
                    "\r{frame} {label} {}/{}  {}\x1b[K",
                    snap.done, snap.total, snap.label
                );
            } else {
                eprint!("\r{frame} {label} {}  {}\x1b[K", snap.count, snap.label);
            }
            i += 1;
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
        eprint!("\r\x1b[K");
    });

    let out = f(&progress);
    done.store(true, Ordering::Relaxed);
    let _ = handle.join();
    out
}
