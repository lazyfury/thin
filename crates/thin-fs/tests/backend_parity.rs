//! Rust 后端 vs native(`getattrlistbulk`) 后端对拍。
//!
//! 仅在 macOS + `--features native` 下编译运行：
//! `cargo test -p thin-fs --features native`

#![cfg(all(target_os = "macos", feature = "native"))]

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use thin_fs::backend::{Backend, NativeBackend, RustBackend};
use thin_fs::{Control, Entry, Kind, Visit, WalkOptions};

fn tmp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("thin-fs-parity-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    base
}

fn sample_tree(tag: &str) -> PathBuf {
    let base = tmp(tag);
    fs::create_dir_all(base.join("a/b/c")).unwrap();
    fs::create_dir_all(base.join("empty")).unwrap();
    fs::write(base.join("a/one.txt"), vec![1u8; 4096]).unwrap();
    fs::write(base.join("a/b/two.bin"), vec![2u8; 100_000]).unwrap();
    fs::write(base.join("a/b/c/three.dmg"), vec![3u8; 250_000]).unwrap();
    fs::hard_link(base.join("a/one.txt"), base.join("a/one-link.txt")).unwrap();
    std::os::unix::fs::symlink(base.join("a"), base.join("a-link")).unwrap();
    base
}

/// 收集 (相对路径, kind, size, alloc)，目录的 size/alloc 归零以便两后端比较。
fn collect(
    backend: &dyn Backend,
    root: &Path,
    opts: &WalkOptions,
) -> Vec<(String, Kind, u64, u64)> {
    let mut out = Vec::new();
    {
        let mut f = |e: Entry<'_>| -> Visit {
            if let Some(m) = e.meta {
                let rel = e
                    .path
                    .strip_prefix(root)
                    .unwrap_or(e.path)
                    .to_string_lossy()
                    .into_owned();
                let (size, alloc) = if m.kind == Kind::Dir {
                    (0, 0)
                } else {
                    (m.size, m.alloc)
                };
                out.push((rel, m.kind, size, alloc));
            }
            Visit::Continue
        };
        backend.walk(&[root.to_path_buf()], opts, &Control::none(), &mut f);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 通过某后端计算 (allocated, logical, files)，验证用量聚合一致。
fn usage_via(backend: &dyn Backend, root: &Path) -> (u64, u64, u64) {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut allocated = 0u64;
    let mut logical = 0u64;
    let mut files = 0u64;
    {
        let mut f = |e: Entry<'_>| -> Visit {
            if let Some(m) = e.meta
                && m.kind == Kind::File
            {
                logical = logical.saturating_add(m.size);
                if seen.insert((m.dev, m.ino)) {
                    allocated = allocated.saturating_add(m.alloc);
                    files += 1;
                }
            }
            Visit::Continue
        };
        backend.walk(
            &[root.to_path_buf()],
            &WalkOptions::default(),
            &Control::none(),
            &mut f,
        );
    }
    (allocated, logical, files)
}

#[test]
fn rust_and_native_agree_on_entries() {
    let root = sample_tree("entries");
    let opts = WalkOptions::default();

    let rust = collect(&RustBackend, &root, &opts);
    let native = collect(&NativeBackend, &root, &opts);

    assert_eq!(rust, native, "两后端条目应完全一致");
    assert!(rust.iter().any(|(_, k, _, _)| *k == Kind::File));
    assert!(rust.iter().any(|(_, k, _, _)| *k == Kind::Dir));
    assert!(rust.iter().any(|(_, k, _, _)| *k == Kind::Symlink));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rust_and_native_agree_on_usage() {
    let root = sample_tree("usage");
    assert_eq!(
        usage_via(&RustBackend, &root),
        usage_via(&NativeBackend, &root)
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rust_and_native_agree_with_max_depth() {
    let root = sample_tree("depth");
    let opts = WalkOptions {
        max_depth: Some(2),
        ..Default::default()
    };
    let rust = collect(&RustBackend, &root, &opts);
    let native = collect(&NativeBackend, &root, &opts);
    assert_eq!(rust, native);
    // 深度 3 的 three.dmg 不应出现
    assert!(!rust.iter().any(|(p, _, _, _)| p.ends_with("three.dmg")));
    let _ = fs::remove_dir_all(&root);
}
