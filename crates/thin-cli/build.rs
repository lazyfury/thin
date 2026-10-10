//! 把「构建时间 + 目标平台」编译进二进制，供 `thin --version` 展示。
//!
//! `cargo install --git …` 是本地构建，`CARGO_PKG_VERSION` 不随构建变化，
//! 因此额外注入构建时间与目标三元组，便于区分实际运行的是哪次构建。

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // SOURCE_DATE_EPOCH 可覆盖（可复现构建）；否则取当前时间。
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    let host = std::env::var("HOST").unwrap_or_else(|_| "unknown".into());
    let host_note = if target == host {
        String::new()
    } else {
        format!(" (host {host})")
    };
    let info = format!("built {} · target {target}{host_note}", fmt_utc(secs));
    println!("cargo:rustc-env=THIN_BUILD_INFO={info}");
}

/// Unix 秒 → `YYYY-MM-DD HH:MM UTC`（Howard Hinnant 的 civil_from_days）。
fn fmt_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {h:02}:{m:02} UTC")
}
