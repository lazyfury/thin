//! 错误类型：遍历/元数据失败不 panic，统一收敛到这里。

use std::path::PathBuf;

/// 文件系统层错误（只读操作）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// 路径不存在。
    NotFound(PathBuf),
    /// 权限不足 / TCC 拒绝（Full Disk Access 未授予等）。
    Denied(PathBuf),
    /// 其他 IO 错误（附原始 `ErrorKind` 描述）。
    Io(PathBuf, String),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::NotFound(p) => write!(f, "路径不存在: {}", p.display()),
            FsError::Denied(p) => write!(f, "无权限读取: {}", p.display()),
            FsError::Io(p, e) => write!(f, "读取失败 {}: {e}", p.display()),
        }
    }
}

impl std::error::Error for FsError {}
