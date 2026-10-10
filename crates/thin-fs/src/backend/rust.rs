//! 纯 Rust 遍历后端（`walkdir` + `std::fs`）。
//!
//! 这里集中实现所有遍历不变量；`native` 后端必须复刻同样的语义。

use crate::backend::Backend;
use crate::kind::{Entry, Kind, Meta};
use crate::mount::{device_of, is_mount_point};
use crate::progress::Control;
use crate::walk::{Visit, WalkOptions, WalkStats};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct RustBackend;

impl Backend for RustBackend {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn walk(
        &self,
        roots: &[PathBuf],
        opts: &WalkOptions,
        ctl: &Control<'_>,
        emit: &mut (dyn for<'e> FnMut(Entry<'e>) -> Visit + '_),
    ) -> WalkStats {
        let mut stats = WalkStats::default();
        for root in roots {
            if ctl.is_cancelled() {
                break;
            }
            // 根设备号（跟随符号链接，与既有 `dir_size` 一致）。
            let root_dev = device_of(root);
            let mut it = WalkDir::new(root)
                .follow_links(opts.follow_links)
                .max_depth(opts.max_depth.unwrap_or(usize::MAX))
                .into_iter();

            while let Some(next) = it.next() {
                if ctl.is_cancelled() {
                    break;
                }
                let entry = match next {
                    Ok(e) => e,
                    Err(_) => {
                        stats.denied += 1;
                        continue;
                    }
                };
                // 根条目不回调，由调用方自行处理。
                if entry.depth() == 0 {
                    continue;
                }

                let meta = match entry.metadata() {
                    Ok(md) => {
                        let mut kind = if md.is_dir() {
                            Kind::Dir
                        } else if md.file_type().is_file() {
                            Kind::File
                        } else if md.file_type().is_symlink() {
                            Kind::Symlink
                        } else {
                            Kind::Other
                        };
                        let mut prune = false;
                        if kind == Kind::Dir {
                            if !opts.cross_mount && is_mount_point(entry.path()) {
                                kind = Kind::Mount;
                                prune = true;
                            } else if opts.same_dev_only
                                && let Some(rd) = root_dev
                                && md.dev() != rd
                            {
                                kind = Kind::Volume;
                                prune = true;
                            }
                        }
                        if prune {
                            stats.pruned += 1;
                        }
                        Some(Meta {
                            kind,
                            size: md.len(),
                            alloc: md.blocks().saturating_mul(512),
                            dev: md.dev(),
                            ino: md.ino(),
                            mtime: md.mtime(),
                        })
                    }
                    Err(_) => {
                        stats.denied += 1;
                        None
                    }
                };

                stats.entries += 1;
                let v = emit(Entry {
                    path: entry.path(),
                    depth: entry.depth(),
                    meta,
                });
                ctl.touch();

                // 卷边界强制剪枝（无论 visitor 是否要求）。
                if meta.map(|m| m.kind) == Some(Kind::Mount)
                    || meta.map(|m| m.kind) == Some(Kind::Volume)
                {
                    it.skip_current_dir();
                }
                match v {
                    Visit::Continue => {}
                    Visit::Skip => it.skip_current_dir(),
                    Visit::Stop => return stats,
                }
            }
        }
        stats
    }
}
