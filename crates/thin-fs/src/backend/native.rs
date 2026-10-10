//! macOS `getattrlistbulk(2)` 批量后端（feature `native`）。
//!
//! 与 [`crate::backend::RustBackend`] 语义一致：不跨卷、不跟符号链接、深度限制、
//! `Visit::{Continue,Skip,Stop}`、取消与进度。差别只是**一次 syscall 批量拿一批**
//! 目录项属性，减少逐条 `stat`。
//!
//! 缓冲区布局（已用 C 程序对 `lstat` 实测校准）：
//! 每个条目以 `u32 length` 开头（8 字节对齐），随后是 `attribute_set_t returned`
//! （5×u32），再按 **getattrlist(2) 文档顺序** 依次排列“`returned` 位图中置位”的属性。
//! 文件大小类属性只在 `returned.fileattr` 置位时存在（目录没有）。

use crate::backend::{Backend, RustBackend};
use crate::kind::{Entry, Kind, Meta};
use crate::mount::{device_of, is_mount_point};
use crate::progress::Control;
use crate::walk::{Visit, WalkOptions, WalkStats};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const ATTR_BIT_MAP_COUNT: u16 = 5;

const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_FILE_TOTALSIZE: u32 = 0x0000_0002;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;

const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// `struct attrlist`（`sys/attr.h`）。
#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: libc::c_int,
        alist: *mut AttrList,
        attr_buf: *mut libc::c_void,
        attr_buf_size: libc::size_t,
        options: u64,
    ) -> libc::c_int;
}

/// 缓冲区大小：单条记录约 80–100 字节，256 KiB 足够容纳数千条。
const BUF_SIZE: usize = 256 * 1024;

pub struct NativeBackend;

impl Backend for NativeBackend {
    fn name(&self) -> &'static str {
        "native"
    }

    fn walk(
        &self,
        roots: &[std::path::PathBuf],
        opts: &WalkOptions,
        ctl: &Control<'_>,
        emit: &mut (dyn for<'e> FnMut(Entry<'e>) -> Visit + '_),
    ) -> WalkStats {
        // 跟随符号链接的语义交给 Rust 后端（native 只做 no-follow 快路径）。
        if opts.follow_links {
            return RustBackend.walk(roots, opts, ctl, emit);
        }
        let mut stats = WalkStats::default();
        let mut stop = false;
        for root in roots {
            if ctl.is_cancelled() {
                break;
            }
            let root_dev = device_of(root);
            walk_dir(root, 0, root_dev, opts, ctl, emit, &mut stats, &mut stop);
            if stop {
                break;
            }
        }
        stats
    }
}

/// 打开目录（不跟随符号链接）。
fn open_dir(path: &Path) -> Option<libc::c_int> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    (fd >= 0).then_some(fd)
}

struct FdGuard(libc::c_int);
impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    dir: &Path,
    depth: usize,
    root_dev: Option<u64>,
    opts: &WalkOptions,
    ctl: &Control<'_>,
    emit: &mut (dyn for<'e> FnMut(Entry<'e>) -> Visit + '_),
    stats: &mut WalkStats,
    stop: &mut bool,
) {
    let child_depth = depth + 1;
    if let Some(m) = opts.max_depth
        && child_depth > m
    {
        return;
    }

    let Some(fd) = open_dir(dir) else {
        stats.denied += 1;
        return;
    };
    let _guard = FdGuard(fd);

    let mut al = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_DEVID
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_MODTIME
            | ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };
    let mut buf = vec![0u8; BUF_SIZE];

    loop {
        if ctl.is_cancelled() {
            *stop = true;
            return;
        }
        let n = unsafe {
            getattrlistbulk(
                fd,
                &mut al,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if n <= 0 {
            // -1：出错（含 ERANGE）；0：目录读完。两者都结束该目录。
            break;
        }

        let mut offset = 0usize;
        for _ in 0..n {
            if offset + 4 > buf.len() {
                break;
            }
            let base = offset;
            let len = u32::from_ne_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
            if len < 4 || base + len > buf.len() {
                break;
            }
            let entry_bytes = &buf[base..base + len];

            let Some(rec) = parse_entry(entry_bytes) else {
                stats.denied += 1;
                offset = base + len;
                continue;
            };
            offset = base + len;

            let path = dir.join(&rec.name);
            let kind = match rec.objtype {
                VREG => Kind::File,
                VDIR => Kind::Dir,
                VLNK => Kind::Symlink,
                _ => Kind::Other,
            };
            let mut final_kind = kind;
            let mut prune = false;
            if kind == Kind::Dir {
                if !opts.cross_mount && is_mount_point(&path) {
                    final_kind = Kind::Mount;
                    prune = true;
                } else if opts.same_dev_only
                    && let Some(rd) = root_dev
                    && rec.dev != rd
                {
                    final_kind = Kind::Volume;
                    prune = true;
                }
            }

            let meta = Meta {
                kind: final_kind,
                size: rec.total,
                alloc: rec.alloc,
                dev: rec.dev,
                ino: rec.fileid,
                mtime: rec.mtime,
            };
            stats.entries += 1;
            if prune {
                stats.pruned += 1;
            }

            let v = emit(Entry {
                path: &path,
                depth: child_depth,
                meta: Some(meta),
            });
            ctl.touch();
            if v == Visit::Stop {
                *stop = true;
                return;
            }
            if kind == Kind::Dir && !prune && v != Visit::Skip {
                walk_dir(&path, child_depth, root_dev, opts, ctl, emit, stats, stop);
                if *stop {
                    return;
                }
            }
        }
    }
}

struct Rec {
    name: String,
    objtype: u32,
    dev: u64,
    fileid: u64,
    mtime: i64,
    total: u64,
    alloc: u64,
}

/// 解析单个条目（`entry_bytes` 含开头的 `length` 字段）。
fn parse_entry(entry_bytes: &[u8]) -> Option<Rec> {
    let mut r = Reader::new(entry_bytes);
    r.skip(4)?; // length（已由外层读取）

    let common = r.u32()?;
    let _vol = r.u32()?;
    let _dir = r.u32()?;
    let file = r.u32()?;
    let _fork = r.u32()?;

    let mut name = String::new();
    if common & ATTR_CMN_NAME != 0 {
        let (abs, nlen) = r.attrref()?;
        let end = abs.checked_add(nlen as usize)?;
        if end > entry_bytes.len() {
            return None;
        }
        let mut bytes = &entry_bytes[abs..end];
        if bytes.last() == Some(&0) {
            bytes = &bytes[..bytes.len() - 1];
        }
        name = String::from_utf8_lossy(bytes).into_owned();
    }
    if name.is_empty() {
        return None;
    }

    let mut dev = 0u64;
    if common & ATTR_CMN_DEVID != 0 {
        dev = r.u32()? as u64;
    }
    let mut objtype = 0u32;
    if common & ATTR_CMN_OBJTYPE != 0 {
        objtype = r.u32()?;
    }
    let mut mtime = 0i64;
    if common & ATTR_CMN_MODTIME != 0 {
        mtime = r.i64()?;
        r.skip(8)?; // tv_nsec
    }
    let mut fileid = 0u64;
    if common & ATTR_CMN_FILEID != 0 {
        fileid = r.u64()?;
    }
    let mut total = 0u64;
    if file & ATTR_FILE_TOTALSIZE != 0 {
        total = r.i64()?.max(0) as u64;
    }
    let mut alloc = 0u64;
    if file & ATTR_FILE_ALLOCSIZE != 0 {
        alloc = r.i64()?.max(0) as u64;
    }

    Some(Rec {
        name,
        objtype,
        dev,
        fileid,
        mtime,
        total,
        alloc,
    })
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(())
    }
    fn u32(&mut self) -> Option<u32> {
        let v = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_ne_bytes(v.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        let v = self.b.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(u64::from_ne_bytes(v.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        let v = self.b.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(i64::from_ne_bytes(v.try_into().ok()?))
    }
    /// 读取 `attrreference_t`，返回（数据在条目内的绝对偏移，长度）。
    fn attrref(&mut self) -> Option<(usize, u32)> {
        let at = self.p;
        let dataoff = i32::from_ne_bytes(self.b.get(self.p..self.p + 4)?.try_into().ok()?);
        self.p += 4;
        let len = self.u32()?;
        let abs = (at as i64 + dataoff as i64) as usize;
        Some((abs, len))
    }
}
