//! 挂载点与卷边界。
//!
//! macOS APFS 各卷**共享同一个 `st_dev`**，因此不能用设备号区分卷，必须靠挂载点，
//! 否则 `/System` 会把 `/System/Volumes/Data`（数据卷）整个算进去、与 `/Users` 重复。
//!
//! 挂载点集合进程内缓存（只取一次），供 [`crate::walk`] 做跨卷剪枝。

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 系统挂载点集合（进程内缓存，只取一次）。
fn mount_points() -> &'static HashSet<PathBuf> {
    static MOUNTS: OnceLock<HashSet<PathBuf>> = OnceLock::new();
    MOUNTS.get_or_init(|| {
        let mut set = HashSet::new();
        // SAFETY: `getmntinfo` 返回静态缓冲区指针，`MNT_NOWAIT` 不阻塞；
        // 指针在进程生命周期内有效，仅读取。
        unsafe {
            let mut buf: *mut libc::statfs = std::ptr::null_mut();
            let n = libc::getmntinfo(&mut buf, libc::MNT_NOWAIT);
            if n > 0 && !buf.is_null() {
                for i in 0..n as isize {
                    let fs = &*buf.offset(i);
                    let mp = std::ffi::CStr::from_ptr(fs.f_mntonname.as_ptr());
                    if let Ok(s) = mp.to_str() {
                        set.insert(PathBuf::from(s));
                    }
                }
            }
        }
        set
    })
}

/// 路径是否是挂载点（另一卷的挂载根）。用于避免跨卷统计/遍历。
pub fn is_mount_point(path: &Path) -> bool {
    mount_points().contains(path)
}

/// 取路径所在设备号（跟随符号链接），用于判断是否跨设备。
pub fn device_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.dev())
}

/// 当前系统所有挂载点（只读快照）。
pub fn mount_point_list() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = mount_points().iter().cloned().collect();
    v.sort();
    v
}
