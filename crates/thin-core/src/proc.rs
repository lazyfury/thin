//! 带超时的外部命令调用。
//!
//! `tmutil` / `plutil` / `pgrep` / `mount` 等一旦挂死会把整个扫描拖住；
//! 这里统一加超时，超时即 kill 并返回 `None`（调用方按「未知」处理，不得当作「不存在」）。

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// 运行外部命令，超过 `timeout` 则 kill 并返回 `None`。
pub fn output_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_output_for_fast_command() {
        let out = output_with_timeout("echo", &["hi"], Duration::from_secs(5));
        let out = out.expect("echo 应成功");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn times_out_slow_command() {
        // 睡 5 秒但只等 100ms，应被 kill 并返回 None
        let out = output_with_timeout("sleep", &["5"], Duration::from_millis(100));
        assert!(out.is_none());
    }
}
