//! 构建并链接 Swift Package（POC）。
//!
//! 思路：`cargo build` 时调 `swift build` 产出静态库 `libThinKit.a`，
//! 再把它的搜索路径和 Swift 运行时 rpath 交给 rustc。
//!
//! 若 Swift 工具链缺失，构建直接报错（这个 demo 本就依赖它）；正式版
//! `crates/thin-sys` 会退化为「可选 feature + 编译期探测」。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let pkg = manifest.parent().unwrap().join("ThinKit");
    let build = pkg.join(".build");

    // 1) 调 Swift 构建（release）
    let status = Command::new("swift")
        .args(["build", "-c", "release", "--package-path"])
        .arg(&pkg)
        .status()
        .expect("无法执行 `swift`，请确认已安装 Xcode / Command Line Tools");
    assert!(status.success(), "swift build 失败");

    // 2) 问 SwiftPM 产出目录（不同版本布局不同：.build/release 或 .build/out/Products/Release）
    let lib_dir = bin_path(&pkg).unwrap_or_else(|| build.join("release"));
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ThinKit");

    // 3) 静态 Swift 库依赖 Swift 运行时（libswiftCore 等）。
    //    ld64 会读取 .o 里的 LC_LINKER_OPTION 自动补 -lswiftCore，
    //    这里补上搜索路径与运行期 rpath 即可。
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    println!("cargo:rerun-if-changed={}", pkg.join("Sources").display());
    println!("cargo:rerun-if-changed={}", pkg.join("Package.swift").display());
}

/// `swift build --show-bin-path`：拿到确切产出目录，避开布局差异。
fn bin_path(pkg: &std::path::Path) -> Option<PathBuf> {
    let out = Command::new("swift")
        .args(["build", "-c", "release", "--show-bin-path", "--package-path"])
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
