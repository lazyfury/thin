//! 手动验证 `thin_fs::usage` 的耗时与结果：
//! `cargo run --release -p thin-fs --example fs-usage -- <路径>...`
//!
//! 与 `thin-sys` 的 Swift 示例对照：
//! `cargo run --release -p thin-sys --example fs-usage -- <路径>`
//! 以及内核口径 `du -sk <路径>`。

use std::path::Path;
use std::time::Instant;
use thin_fs::{Control, WalkOptions};

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
    println!("backend = {}", thin_fs::backend::backend_name());
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        let t = Instant::now();
        let u = thin_fs::usage(path, &WalkOptions::default(), &Control::none());
        let dt = t.elapsed();
        println!(
            "{arg}\n  allocated={} ({})  logical={}  files={}  in {dt:?}",
            human(u.allocated),
            u.allocated,
            human(u.logical),
            u.files
        );
    }
}
