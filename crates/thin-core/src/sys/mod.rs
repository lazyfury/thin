//! 系统能力收拢：磁盘探测、平台抽象（Swift/Rust）、CPU·内存·电池。
//!
//! 由 `probe` / `platform` / `status` 三个子模块组成，对外仍以
//! `crate::probe` / `crate::platform` / `crate::status` 路径访问（见 `lib.rs` 的 re-export）。

pub mod platform;
pub mod probe;
pub mod status;
