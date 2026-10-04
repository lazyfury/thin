use anyhow::Result;
use std::ffi::CString;

/// 单个卷的容量信息（来自 statfs）
#[derive(Debug, Clone)]
pub struct VolumeStat {
    #[allow(dead_code)]
    pub path: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

impl VolumeStat {
    pub fn used_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used as f64 / self.total as f64 * 100.0
        }
    }
}

/// 调用 statfs 获取路径所在卷的容量
pub fn statfs(path: &str) -> Option<VolumeStat> {
    let c = CString::new(path).ok()?;
    unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let bsize = st.f_bsize as u64;
        let total = (st.f_blocks as u64).saturating_mul(bsize);
        let free = (st.f_bfree as u64).saturating_mul(bsize);
        let avail = (st.f_bavail as u64).saturating_mul(bsize);
        Some(VolumeStat {
            path: path.to_string(),
            total,
            used: total.saturating_sub(free),
            avail,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Mount {
    #[allow(dead_code)]
    pub device: String,
    pub mount: String,
}

/// 解析 `mount` 输出，列出所有挂载点
pub fn list_mounts() -> Vec<Mount> {
    let mut v = Vec::new();
    if let Ok(out) = std::process::Command::new("mount").output() {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            // 形如: /dev/disk7s1 on /Volumes/数据 (apfs, ...)
            if let Some((dev, rest)) = line.split_once(" on ") {
                if let Some((mnt, _)) = rest.split_once(" (") {
                    v.push(Mount {
                        device: dev.to_string(),
                        mount: mnt.to_string(),
                    });
                }
            }
        }
    }
    v
}

/// 本地 Time Machine 快照列表
pub fn local_snapshots() -> Vec<String> {
    let mut out = Vec::new();
    for vol in ["/", "/System/Volumes/Data"] {
        if let Ok(o) = std::process::Command::new("tmutil")
            .args(["listlocalsnapshots", vol])
            .output()
        {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let l = line.trim();
                if l.starts_with("com.apple.TimeMachine") {
                    out.push(l.to_string());
                }
            }
        }
    }
    out
}

/// 判断挂载点是否是需要单独标注的外部/备份卷
pub fn classify_mount(mount: &str) -> MountKind {
    let base = mount.rsplit('/').next().unwrap_or("");
    if base.contains("时光机")
        || base.contains("Time Machine")
        || mount.contains("Backups.backupdb")
    {
        return MountKind::TimeMachine;
    }
    if mount.starts_with("/Volumes/") && base != "Macintosh HD" {
        // 进一步：若卷内有 Backups.backupdb 则视为 TM
        if std::path::Path::new(mount)
            .join("Backups.backupdb")
            .exists()
        {
            return MountKind::TimeMachine;
        }
        return MountKind::External;
    }
    MountKind::Internal
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    Internal,
    External,
    TimeMachine,
}

pub fn probe_summary() -> Result<()> {
    use crate::report::human;
    println!("\x1b[1m磁盘容量\x1b[0m");
    if let Some(v) = statfs("/System/Volumes/Data") {
        println!(
            "  APFS 容器    容量 {}  已用 {}  可用 {}  ({:.0}%)",
            human(v.total),
            human(v.used),
            human(v.avail),
            v.used_pct()
        );
        println!("  \x1b[90m（APFS 各卷共享同一容器可用空间，故只显示一次）\x1b[0m");
    }

    let mounts = list_mounts();
    let mut ext: Vec<&Mount> = mounts
        .iter()
        .filter(|m| classify_mount(&m.mount) != MountKind::Internal)
        .collect();
    ext.sort_by(|a, b| a.mount.cmp(&b.mount));

    println!("\n\x1b[1m外部 / 备份卷\x1b[0m（不计入系统缓存）");
    if ext.is_empty() {
        println!("  （无）");
    }
    for m in ext {
        let kind = match classify_mount(&m.mount) {
            MountKind::TimeMachine => "Time Machine",
            MountKind::External => "外部磁盘",
            MountKind::Internal => "",
        };
        let cap = statfs(&m.mount)
            .map(|v| format!("容量 {}", human(v.total)))
            .unwrap_or_default();
        println!("  {:<24} [{}]  {}", m.mount, kind, cap);
    }

    println!("\n\x1b[1m本地快照\x1b[0m（会虚占空间，删除后不立即释放）");
    let snaps = local_snapshots();
    if snaps.is_empty() {
        println!("  （无）");
    } else {
        for s in snaps {
            println!("  {}", s);
        }
    }
    Ok(())
}
