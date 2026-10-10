//! thin 的只读文件系统遍历、用量聚合与查找。
//!
//! 设计目标（见 `docs/thin-fs-plan.md`）：
//!
//! - **只读**：本 crate 不做任何删除 / 移动 / 提权；清理语义与安全门仍归 `thin-core`。
//! - **不变量只实现一次**：不跨卷、不跟符号链接、深度限制、错误计数都集中在
//!   [`walk::Walk`] / [`backend`]。
//! - **双后端**：默认纯 Rust（`walkdir` + `std::fs`），macOS 可选 `native`
//!   （`getattrlistbulk`）后端，两后端结果应一致。
//!
//! 对 `thin-core` 的依赖方向是单向的：`thin-core → thin-fs`，本 crate 不认识规则、
//! 受保护路径、隔离区等概念。

pub mod backend;
pub mod kind;
pub mod mount;
pub mod progress;
pub mod query;
pub mod usage;
pub mod walk;

pub use kind::{Entry, Kind, Meta};
pub use mount::{device_of, is_mount_point};
pub use progress::{Control, ProgressSink};
pub use query::{FindSpec, Predicate, find};
pub use usage::{Usage, usage};
pub use walk::{Visit, Walk, WalkOptions, WalkStats};
