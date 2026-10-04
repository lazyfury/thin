//! POC：验证 Rust ↔ Swift(Foundation) FFI 全链路。
//!
//! 同时打印 Swift（NSURL 可用容量）与 libc（statfs）两套数据，直观看到
//! purgeable 空间带来的差异。

use std::ffi::{CStr, CString};

extern "C" {
    fn thin_available_capacity(
        path: *const libc::c_char,
        total: *mut u64,
        available: *mut u64,
        important: *mut u64,
        opportunistic: *mut u64,
    ) -> i32;

    fn thin_version() -> *mut libc::c_char;
    fn thin_string_free(ptr: *mut libc::c_char);
}

/// Swift 侧返回的卷容量
struct SwiftCapacity {
    total: u64,
    available: u64,
    important: u64,
    opportunistic: u64,
}

fn swift_capacity(path: &str) -> Option<SwiftCapacity> {
    let c = CString::new(path).ok()?;
    let (mut total, mut available, mut important, mut opportunistic) = (0u64, 0, 0, 0);
    let rc = unsafe {
        thin_available_capacity(
            c.as_ptr(),
            &mut total,
            &mut available,
            &mut important,
            &mut opportunistic,
        )
    };
    (rc == 0).then_some(SwiftCapacity {
        total,
        available,
        important,
        opportunistic,
    })
}

/// 现有实现：libc::statfs
fn statfs_available(path: &str) -> Option<u64> {
    let c = CString::new(path).ok()?;
    unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        Some(st.f_bavail as u64 * st.f_bsize as u64)
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

fn main() {
    // 1) 字符串跨语言链路：Swift strdup → Rust 读取 → Rust free
    let version = unsafe {
        let p = thin_version();
        assert!(!p.is_null(), "thin_version 返回空指针");
        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
        thin_string_free(p);
        s
    };
    println!("Swift 库: {version}\n");

    // 2) 容量对比
    let path = std::env::args().nth(1).unwrap_or_else(|| "/".to_string());
    let swift = swift_capacity(&path).expect("Swift 调用失败");
    let statfs = statfs_available(&path).expect("statfs 失败");
    let purgeable = swift.important.saturating_sub(swift.available);

    println!("路径: {path}");
    println!("NSURL  total={}", human(swift.total));
    println!(
        "NSURL  available={}  important(含 purgeable)={}  opportunistic={}",
        human(swift.available),
        human(swift.important),
        human(swift.opportunistic)
    );
    println!("statfs bavail={}", human(statfs));
    println!("purgeable(important-available)≈{}", human(purgeable));
    println!(
        "\n结论: {}",
        if swift.available != statfs {
            "NSURL 与 statfs 不一致 —— 正是需要 Swift 底层能力的场景 ✅"
        } else {
            "两者恰好一致（无 purgeable 或有权限差异时可能发生）"
        }
    );
}
