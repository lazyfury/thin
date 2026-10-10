//! `~/.thin` 下的持久状态：清理账本、预设、定时任务、保护名单。
//!
//! 由 `history` / `preset` / `schedule` / `protect` 四个子模块组成，对外仍以
//! `crate::history` 等路径访问（见 `lib.rs` 的 re-export）。

pub mod history;
pub mod preset;
pub mod protect;
pub mod schedule;
