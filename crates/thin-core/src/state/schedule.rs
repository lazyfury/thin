//! 定时任务：生成并加载 launchd LaunchAgent（macOS）。
//!
//! 纯 CLI 无需 App：把 plist 写入 `~/Library/LaunchAgents/`，再 `launchctl bootstrap`。
//! 定时任务只运行 `thin schedule run --preset <id>`，因此只会处理**用户自定义预设**。

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const LABEL: &str = "dev.thin.clean";

/// 触发频率
#[derive(Debug, Clone)]
pub enum Schedule {
    Daily {
        hour: u32,
        minute: u32,
    },
    Weekly {
        weekday: u32,
        hour: u32,
        minute: u32,
    },
    Interval {
        seconds: u64,
    },
}

fn user_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// 生成 plist 内容（纯函数，便于测试）
pub fn render_plist(bin: &Path, preset: &str, schedule: &Schedule) -> String {
    let bin = xml_escape(&bin.to_string_lossy());
    let preset = xml_escape(preset);
    let home = xml_escape(&user_home().to_string_lossy());
    let timing = match schedule {
        Schedule::Daily { hour, minute } => format!(
            "    <key>StartCalendarInterval</key>\n    <dict><key>Hour</key><integer>{hour}</integer><key>Minute</key><integer>{minute}</integer></dict>\n"
        ),
        Schedule::Weekly {
            weekday,
            hour,
            minute,
        } => format!(
            "    <key>StartCalendarInterval</key>\n    <dict><key>Weekday</key><integer>{weekday}</integer><key>Hour</key><integer>{hour}</integer><key>Minute</key><integer>{minute}</integer></dict>\n"
        ),
        Schedule::Interval { seconds } => {
            format!("    <key>StartInterval</key><integer>{seconds}</integer>\n")
        }
    };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n    <string>{LABEL}</string>\n\
    <key>ProgramArguments</key>\n    <array>\n        <string>{bin}</string>\n        <string>schedule</string>\n        <string>run</string>\n        <string>--preset</string>\n        <string>{preset}</string>\n    </array>\n\
{timing}\
    <key>RunAtLoad</key>\n    <false/>\n\
    <key>ProcessType</key>\n    <string>Background</string>\n\
    <key>LowPriorityIO</key>\n    <true/>\n\
    <key>EnvironmentVariables</key>\n    <dict><key>HOME</key><string>{home}</string></dict>\n\
    <key>StandardOutPath</key>\n    <string>{home}/.thin/schedule.log</string>\n\
    <key>StandardErrorPath</key>\n    <string>{home}/.thin/schedule.err</string>\n\
</dict>\n\
</plist>\n"
    )
}

/// plist 路径（可指定 home 便于测试）
pub fn plist_path_in(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

pub fn plist_path() -> PathBuf {
    plist_path_in(&user_home())
}

/// 写入 plist 并 bootstrap 到当前用户的 launchd 会话
pub fn install(bin: &Path, preset: &str, schedule: &Schedule) -> Result<PathBuf> {
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, render_plist(bin, preset, schedule))
        .with_context(|| format!("写入 plist 失败: {}", path.display()))?;

    // 先卸载可能已存在的旧 job（忽略错误）
    let _ = crate::proc::output_with_timeout(
        "launchctl",
        &["bootout", &format!("gui/{}", uid())],
        std::time::Duration::from_secs(5),
    );
    let out = crate::proc::output_with_timeout(
        "launchctl",
        &[
            "bootstrap",
            &format!("gui/{}", uid()),
            &path.to_string_lossy(),
        ],
        std::time::Duration::from_secs(10),
    );
    match out {
        Some(o) if o.status.success() => Ok(path),
        Some(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            bail!("launchctl bootstrap 失败: {}", err.trim())
        }
        None => bail!("launchctl bootstrap 超时"),
    }
}

/// 卸载并删除 plist；返回是否删除了文件
pub fn uninstall() -> Result<bool> {
    let path = plist_path();
    let _ = crate::proc::output_with_timeout(
        "launchctl",
        &[
            "bootout",
            &format!("gui/{}", uid()),
            &path.to_string_lossy(),
        ],
        std::time::Duration::from_secs(5),
    );
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("删除 plist 失败: {}", path.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// job 是否已加载
pub fn is_loaded() -> bool {
    crate::proc::output_with_timeout(
        "launchctl",
        &["print", &format!("gui/{}/{}", uid(), LABEL)],
        std::time::Duration::from_secs(5),
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_args_and_timing() {
        let p = render_plist(
            Path::new("/Users/me/.local/bin/thin"),
            "weekly-safe",
            &Schedule::Weekly {
                weekday: 0,
                hour: 10,
                minute: 30,
            },
        );
        assert!(p.contains("<string>schedule</string>"));
        assert!(p.contains("<string>run</string>"));
        assert!(p.contains("<string>--preset</string>"));
        assert!(p.contains("<string>weekly-safe</string>"));
        assert!(p.contains("<key>Weekday</key><integer>0</integer>"));
        assert!(p.contains("<key>Hour</key><integer>10</integer>"));
        assert!(p.contains("<key>Minute</key><integer>30</integer>"));
        assert!(p.contains(LABEL));
    }

    #[test]
    fn plist_interval_and_escape() {
        let p = render_plist(
            Path::new("/tmp/a&b/thin"),
            "x",
            &Schedule::Interval { seconds: 3600 },
        );
        assert!(p.contains("<key>StartInterval</key><integer>3600</integer>"));
        assert!(p.contains("/tmp/a&amp;b/thin"));
    }

    #[test]
    fn plist_path_under_launch_agents() {
        let home = Path::new("/Users/me");
        assert_eq!(
            plist_path_in(home),
            PathBuf::from("/Users/me/Library/LaunchAgents/dev.thin.clean.plist")
        );
    }
}
