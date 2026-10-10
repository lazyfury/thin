//! 只读分析与归因：App 识别、规则目录、异常大目录发现、Spotlight 深扫。
//!
//! 由 `recognize` / `catalog` / `discover` / `spotlight` 组成，对外仍以
//! `crate::discover` 等路径访问（见 `lib.rs` 的 re-export）。

pub mod catalog;
pub mod discover;
pub mod recognize;
pub mod spotlight;
