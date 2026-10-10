//! `thin-fs` 遍历不变量、用量与查找的集成测试。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use thin_fs::{Control, Kind, ProgressSink, Visit, Walk, WalkOptions};

fn tmp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("thin-fs-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    base
}

fn run_paths(root: &Path, opts: WalkOptions) -> Vec<PathBuf> {
    let mut seen = Vec::new();
    Walk::new(opts).run(&[root.to_path_buf()], &Control::none(), |e| {
        seen.push(e.path.to_path_buf());
        Visit::Continue
    });
    seen
}

#[test]
fn root_is_not_emitted_and_depth_is_bounded() {
    let base = tmp("depth");
    fs::create_dir_all(base.join("a/b/c")).unwrap();
    fs::write(base.join("a/f.txt"), b"x").unwrap();
    fs::write(base.join("a/b/g.txt"), b"y").unwrap();

    // 不限深度：a、a/b、a/b/c、a/f.txt、a/b/g.txt
    let all = run_paths(&base, WalkOptions::default());
    assert!(!all.contains(&base), "根不应被回调");
    assert!(all.contains(&base.join("a/b/c")), "应到最深目录");

    // max_depth=2：只到 a、a/b
    let shallow = run_paths(
        &base,
        WalkOptions {
            max_depth: Some(2),
            ..Default::default()
        },
    );
    assert!(shallow.contains(&base.join("a")));
    assert!(shallow.contains(&base.join("a/b")));
    assert!(!shallow.contains(&base.join("a/b/c")));
    assert!(
        shallow.contains(&base.join("a/f.txt")),
        "f.txt 在深度 2，应包含"
    );
}

#[test]
fn symlinks_are_reported_but_not_followed() {
    let base = tmp("symlink");
    fs::create_dir_all(base.join("real")).unwrap();
    fs::write(base.join("real/inside.txt"), b"data").unwrap();
    std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();

    let mut link_kind = None;
    let mut reached_inside = false;
    Walk::new(WalkOptions::default()).run(std::slice::from_ref(&base), &Control::none(), |e| {
        if e.path == base.join("link") {
            link_kind = Some(e.kind());
        }
        if e.path == base.join("real/inside.txt") {
            reached_inside = true;
        }
        Visit::Continue
    });
    assert_eq!(link_kind, Some(Kind::Symlink));
    assert!(reached_inside, "真实目录内容应被遍历（经 real，而非 link）");
    // link 下不应产生重复条目
    let paths = run_paths(&base, WalkOptions::default());
    assert!(!paths.iter().any(|p| p.ends_with("link/inside.txt")));
}

#[test]
fn visit_skip_prunes_and_stop_halts() {
    let base = tmp("visit");
    fs::create_dir_all(base.join("skipme")).unwrap();
    fs::write(base.join("skipme/hidden.txt"), b"x").unwrap();
    fs::write(base.join("top.txt"), b"y").unwrap();

    // Skip：不下钻 skipme
    let mut seen = Vec::new();
    Walk::new(WalkOptions::default()).run(std::slice::from_ref(&base), &Control::none(), |e| {
        if e.path.ends_with("skipme") {
            return Visit::Skip;
        }
        seen.push(e.path.to_path_buf());
        Visit::Continue
    });
    assert!(!seen.iter().any(|p| p.ends_with("hidden.txt")));
    assert!(seen.iter().any(|p| p.ends_with("top.txt")));

    // Stop：立即停止
    let mut count = 0;
    Walk::new(WalkOptions::default()).run(std::slice::from_ref(&base), &Control::none(), |_| {
        count += 1;
        Visit::Stop
    });
    assert_eq!(count, 1);
}

#[test]
fn cancellation_stops_immediately() {
    let base = tmp("cancel");
    fs::write(base.join("a.txt"), b"x").unwrap();
    let cancel = AtomicBool::new(true);
    let ctl = Control {
        progress: None,
        cancel: Some(&cancel),
    };
    let mut count = 0;
    Walk::new(WalkOptions::default()).run(std::slice::from_ref(&base), &ctl, |_| {
        count += 1;
        Visit::Continue
    });
    assert_eq!(count, 0);
}

#[test]
fn progress_sink_gets_touched() {
    struct Counter(AtomicU64);
    impl ProgressSink for Counter {
        fn touch(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let base = tmp("progress");
    fs::write(base.join("a.txt"), b"x").unwrap();
    fs::write(base.join("b.txt"), b"y").unwrap();
    let sink = Counter(AtomicU64::new(0));
    Walk::new(WalkOptions::default()).run(
        std::slice::from_ref(&base),
        &Control::with_progress(&sink),
        |_| Visit::Continue,
    );
    assert!(sink.0.load(Ordering::Relaxed) >= 2);
}

#[test]
fn usage_dedupes_hardlinks_for_alloc_but_sums_logical() {
    use std::os::unix::fs::MetadataExt;
    let base = tmp("usage");
    fs::create_dir_all(base.join("sub")).unwrap();
    fs::write(base.join("a"), vec![0u8; 10_000]).unwrap();
    fs::write(base.join("sub/b"), vec![0u8; 20_000]).unwrap();
    fs::hard_link(base.join("a"), base.join("a2")).unwrap();

    let u = thin_fs::usage(&base, &WalkOptions::default(), &Control::none());
    let unique_alloc: u64 = [base.join("a"), base.join("sub/b")]
        .iter()
        .map(|p| fs::metadata(p).unwrap().blocks().saturating_mul(512))
        .sum();
    assert_eq!(u.allocated, unique_alloc);
    assert_eq!(u.logical, 40_000); // a + a2 + b
    assert_eq!(u.files, 2); // a 与 a2 同 inode，只计一次

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn find_files_and_find_dir() {
    let base = tmp("query");
    fs::create_dir_all(base.join("proj/target")).unwrap();
    fs::create_dir_all(base.join("other")).unwrap();
    fs::write(base.join("proj/target/out.bin"), b"x").unwrap();
    fs::write(base.join("proj/Cargo.toml"), b"").unwrap();
    fs::write(base.join("archive.ZIP"), b"x").unwrap();
    fs::write(base.join("notes.txt"), b"x").unwrap();

    let opts = WalkOptions {
        max_depth: Some(4),
        ..Default::default()
    };
    let files = thin_fs::query::find_files(
        std::slice::from_ref(&base),
        &["zip".to_string()],
        opts.clone(),
        0,
        &Control::none(),
    );
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with("archive.ZIP"), "后缀比较应忽略大小写");

    let dirs = thin_fs::query::find_dir(
        std::slice::from_ref(&base),
        "target",
        Some("Cargo.toml"),
        opts,
        &Control::none(),
    );
    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].ends_with("proj/target"));

    let _ = fs::remove_dir_all(&base);
}
