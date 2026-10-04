//! 手动验证 `thin_sys::dir_usage` 的小工具：
//! `cargo run -p thin-sys --example usage -- <路径>...`
//!
//! 与 `du -sk <路径>` 对照可确认实际占用是否一致。

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

fn main() {
    println!("backend_available = {}", thin_sys::backend_available());
    for arg in std::env::args().skip(1) {
        let path = std::path::Path::new(&arg);
        match thin_sys::dir_usage(path) {
            Some(u) => println!(
                "{arg}\n  allocated={} ({})  logical={}  dataless={}  files={}",
                human(u.allocated),
                u.allocated,
                human(u.logical),
                human(u.dataless),
                u.files
            ),
            None => println!("{arg}: 无后端或路径不存在"),
        }
    }
}
