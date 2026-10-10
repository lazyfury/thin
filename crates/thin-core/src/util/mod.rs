//! 通用工具：外部命令超时封装、人类可读格式化、轻量进度上报。
//!
//! 由 `proc` / `fmt` / `progress` 组成，对外仍以 `crate::proc` 等路径访问
//! （见 `lib.rs` 的 re-export）。

pub mod fmt;
pub mod proc;
pub mod progress;
