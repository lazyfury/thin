//! 系统基本信息与实时状态（macOS）。
//!
//! 基本信息走 `sysctl`（型号/芯片/系统版本/内存）；实时状态走 Mach
//! `host_statistics`（CPU 占用、内存页）+ `getloadavg` + `statfs` + `pmset`（电池）。
//! 外部命令均带超时。

use std::ffi::CString;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct SystemInfo {
    pub model: String,
    pub chip: String,
    pub physical_cores: u64,
    pub logical_cores: u64,
    pub os_version: String,
    pub os_build: String,
    pub hostname: String,
    pub mem_total: u64,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LiveStats {
    pub cpu_usage: f64,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub mem_wired: u64,
    pub mem_compressed: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub disk_avail: u64,
    pub battery: Option<Battery>,
}

#[derive(Debug, Clone)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

// ---------------------------------------------------------------------------
// sysctl 辅助
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut size: usize = 0;
    unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        );
    }
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let r = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    buf.truncate(size);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(String::from_utf8_lossy(&buf).to_string())
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let cname = CString::new(name).ok()?;
    let mut val: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let r = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut val as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r == 0 { Some(val) } else { None }
}

fn boot_time() -> Option<u64> {
    let cname = CString::new("kern.boottime").ok()?;
    let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::timeval>();
    let r = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut tv as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r == 0 { Some(tv.tv_sec as u64) } else { None }
}

// ---------------------------------------------------------------------------
// 基本信息
// ---------------------------------------------------------------------------

pub fn collect_info() -> SystemInfo {
    let boot = boot_time().unwrap_or(0);
    let now = now_secs();
    SystemInfo {
        model: sysctl_string("hw.model").unwrap_or_default(),
        chip: sysctl_string("machdep.cpu.brand_string").unwrap_or_default(),
        physical_cores: sysctl_u64("hw.physicalcpu").unwrap_or(0),
        logical_cores: sysctl_u64("hw.logicalcpu").unwrap_or(0),
        os_version: sysctl_string("kern.osproductversion").unwrap_or_default(),
        os_build: sysctl_string("kern.osversion").unwrap_or_default(),
        hostname: sysctl_string("kern.hostname").unwrap_or_default(),
        mem_total: sysctl_u64("hw.memsize").unwrap_or(0),
        uptime_secs: if boot > 0 && now >= boot {
            now - boot
        } else {
            0
        },
    }
}

/// 把秒数格式化为「N 天 HH:MM」
pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    if days > 0 {
        format!("{days} 天 {h:02}:{m:02}")
    } else {
        format!("{h:02}:{m:02}")
    }
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// CPU 采样器：两次采样的 tick 差值算出占用率
#[derive(Default)]
pub struct CpuSampler {
    prev: Option<(u64, u64, u64, u64)>,
}

impl CpuSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self) -> f64 {
        let Some(cur) = cpu_ticks() else {
            return 0.0;
        };
        let usage = match self.prev {
            None => 0.0,
            Some(prev) => {
                let user = cur.0.saturating_sub(prev.0);
                let sys = cur.1.saturating_sub(prev.1);
                let idle = cur.2.saturating_sub(prev.2);
                let nice = cur.3.saturating_sub(prev.3);
                let total = user + sys + idle + nice;
                if total == 0 {
                    0.0
                } else {
                    (user + sys + nice) as f64 / total as f64 * 100.0
                }
            }
        };
        self.prev = Some(cur);
        usage
    }
}

#[allow(deprecated)] // libc::mach_host_self 已弃用，但为避免新增 mach2 依赖继续使用
fn cpu_ticks() -> Option<(u64, u64, u64, u64)> {
    unsafe {
        let mut info: libc::host_cpu_load_info_data_t = std::mem::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let kr = libc::host_statistics(
            libc::mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            &mut info as *mut _ as libc::host_info_t,
            &mut count,
        );
        if kr != libc::KERN_SUCCESS {
            return None;
        }
        Some((
            info.cpu_ticks[libc::CPU_STATE_USER as usize] as u64,
            info.cpu_ticks[libc::CPU_STATE_SYSTEM as usize] as u64,
            info.cpu_ticks[libc::CPU_STATE_IDLE as usize] as u64,
            info.cpu_ticks[libc::CPU_STATE_NICE as usize] as u64,
        ))
    }
}

// ---------------------------------------------------------------------------
// 内存 / 交换 / 电池 / 实时汇总
// ---------------------------------------------------------------------------

struct MemPages {
    free: u64,
    inactive: u64,
    speculative: u64,
    wired: u64,
    compressed: u64,
}

#[allow(deprecated)]
fn mem_pages() -> Option<MemPages> {
    unsafe {
        let mut vm: libc::vm_statistics64_data_t = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let kr = libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            &mut vm as *mut _ as libc::host_info64_t,
            &mut count,
        );
        if kr != libc::KERN_SUCCESS {
            return None;
        }
        let page = libc::sysconf(libc::_SC_PAGESIZE) as u64;
        Some(MemPages {
            free: vm.free_count as u64 * page,
            inactive: vm.inactive_count as u64 * page,
            speculative: vm.speculative_count as u64 * page,
            wired: vm.wire_count as u64 * page,
            compressed: vm.compressor_page_count as u64 * page,
        })
    }
}

#[repr(C)]
struct XswUsage {
    total: u64,
    avail: u64,
    used: u64,
    page_size: u32,
    encrypted: u8,
}

fn swap_usage() -> Option<(u64, u64)> {
    let cname = CString::new("vm.swapusage").ok()?;
    let mut x: XswUsage = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<XswUsage>();
    let r = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut x as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r == 0 {
        Some((x.used, x.total))
    } else {
        None
    }
}

fn battery() -> Option<Battery> {
    let out = crate::proc::output_with_timeout("pmset", &["-g", "batt"], Duration::from_secs(3))?;
    let s = String::from_utf8_lossy(&out.stdout);
    // 形如: -InternalBattery-0 (id=...)\t85%; discharging; 4:12 remaining present: true
    let line = s.lines().find(|l| l.contains('%'))?;
    let before = line.split('%').next()?;
    let pct: u8 = before
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let charging = !line.contains("discharging")
        && (line.contains("charging") || line.contains("charged") || line.contains("AC"));
    Some(Battery {
        percent: pct,
        charging,
    })
}

/// 采集一次实时状态（CPU 占用需结合上一次采样）
pub fn collect_live(cpu: &mut CpuSampler) -> LiveStats {
    let mem_total = sysctl_u64("hw.memsize").unwrap_or(0);
    let mut s = LiveStats {
        cpu_usage: cpu.sample(),
        mem_total,
        ..Default::default()
    };

    let mut loads = [0f64; 3];
    unsafe {
        libc::getloadavg(loads.as_mut_ptr(), 3);
    }
    s.load1 = loads[0];
    s.load5 = loads[1];
    s.load15 = loads[2];

    if let Some(p) = mem_pages() {
        s.mem_wired = p.wired;
        s.mem_compressed = p.compressed;
        let reclaimable = p.free + p.inactive + p.speculative;
        s.mem_used = mem_total.saturating_sub(reclaimable);
    }
    if let Some((used, total)) = swap_usage() {
        s.swap_used = used;
        s.swap_total = total;
    }
    if let Some(v) = crate::probe::statfs("/System/Volumes/Data") {
        s.disk_used = v.used;
        s.disk_total = v.total;
        s.disk_avail = v.avail;
    }
    s.battery = battery();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_populated() {
        let i = collect_info();
        assert!(i.mem_total > 0, "内存总量应可读");
        assert!(!i.os_version.is_empty(), "应能读到系统版本");
    }

    #[test]
    fn cpu_sampler_second_sample_in_range() {
        let mut s = CpuSampler::new();
        let _ = s.sample();
        std::thread::sleep(Duration::from_millis(60));
        let usage = s.sample();
        assert!(
            (0.0..=100.0).contains(&usage),
            "占用率应在 0..100，得到 {usage}"
        );
    }

    #[test]
    fn live_stats_plausible() {
        let mut s = CpuSampler::new();
        let _ = s.sample();
        std::thread::sleep(Duration::from_millis(60));
        let l = collect_live(&mut s);
        assert!(l.mem_total > 0);
        assert!(l.mem_used <= l.mem_total);
        assert!(l.disk_total > 0);
    }

    #[test]
    fn uptime_format() {
        assert_eq!(format_uptime(0), "00:00");
        assert_eq!(format_uptime(3661), "01:01");
        assert_eq!(format_uptime(90_000), "1 天 01:00");
    }
}
