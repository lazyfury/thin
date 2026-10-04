//! thin 核心库。
//!
//! 提供与 UI 无关的能力：磁盘探测、规则目录、扫描与「可回收空间」核算。
//!
//! 设计原则见仓库根目录 `DESIGN.md`：还原真实路径、诚实核算、风险分级。

pub mod apps;
pub mod clean;
pub mod discover;
pub mod finder;
pub mod fmt;
pub mod fsutil;
pub mod history;
pub mod model;
pub mod preset;
pub mod probe;
pub mod proc;
pub mod progress;
pub mod protect;
pub mod rules;
pub mod scan;
pub mod schedule;
pub mod status;

pub use model::{Category, CleanItem, Explain, Matcher, ReclaimSummary, Risk, Rule};
