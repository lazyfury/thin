//! 构建并链接 Swift Package `swift/ThinKit`。
//!
//! 策略：**能连就连，连不上就降级**（绝不 panic）。
//! - 非 macOS：什么都不做，`thin-sys` 走空实现。
//! - 找不到 `swift` 或 `swift build` 失败：打印 warning，不定义 `thin_sys_swift`，
//!   上层据此回退到纯 Rust 的 `libc` 实现。
//!
//! 链接细节（POC 已验证）：静态 Swift 库靠 `.o` 的 `LC_LINKER_OPTION` 自动补
//! `-lswiftCore`，Rust 侧补 `/usr/lib/swift` 搜索路径与 rpath 即可。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(thin_sys_swift)");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/thin-sys → crates → <repo> → swift/ThinKit
    let Some(pkg) = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|r| r.join("swift/ThinKit"))
    else {
        println!("cargo:warning=未能定位 swift/ThinKit，thin-sys 降级");
        return;
    };
    if !pkg.join("Package.swift").exists() {
        println!("cargo:warning=未找到 {}，thin-sys 降级", pkg.display());
        return;
    }

    if Command::new("swift").arg("--version").output().is_err() {
        println!("cargo:warning=未找到 swift 工具链，thin-sys 降级为纯 Rust 实现");
        return;
    }

    let status = Command::new("swift")
        .args(["build", "-c", "release", "--package-path"])
        .arg(&pkg)
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => {
            println!("cargo:warning=`swift build` 失败，thin-sys 降级为纯 Rust 实现");
            return;
        }
    }

    let Some(bin_dir) = bin_path(&pkg) else {
        println!("cargo:warning=未定位 Swift 产出目录，thin-sys 降级");
        return;
    };
    println!("cargo:rustc-link-search=native={}", bin_dir.display());
    println!("cargo:rustc-link-lib=static=ThinKit");
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-cfg=thin_sys_swift");

    println!("cargo:rerun-if-changed={}", pkg.join("Sources").display());
    println!(
        "cargo:rerun-if-changed={}",
        pkg.join("Package.swift").display()
    );
}

/// `swift build --show-bin-path`：拿确切产出目录，避开 SwiftPM 布局差异
/// （`.build/release` vs `.build/out/Products/Release`）。
fn bin_path(pkg: &Path) -> Option<PathBuf> {
    let out = Command::new("swift")
        .args([
            "build",
            "-c",
            "release",
            "--show-bin-path",
            "--package-path",
        ])
        .arg(pkg)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let dir = PathBuf::from(s.lines().last()?.trim());
    dir.join("libThinKit.a").exists().then_some(dir)
}
