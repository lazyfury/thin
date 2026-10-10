//! thin 核心库。
//!
//! 提供与 UI 无关的能力：磁盘探测、规则目录、扫描与「可回收空间」核算。
//!
//! 设计原则见仓库根目录 `DESIGN.md`：还原真实路径、诚实核算、风险分级。

pub mod analysis;
pub mod app_conditions;
pub mod apps;
pub mod clean;
pub mod finder;
pub mod fsutil;
pub mod model;
pub mod orphans;
pub mod rules;
pub mod scan;
pub mod script;
pub mod state;
pub mod sys;
pub mod tree;
pub mod util;

// 保持既有路径：`crate::probe` / `crate::discover` / `crate::proc` 等。
pub use analysis::{catalog, discover, recognize, spotlight};
pub use state::{history, preset, protect, schedule};
pub use sys::{platform, probe, status};
pub use util::{fmt, proc, progress};

pub use model::{Category, CleanItem, Explain, Matcher, ReclaimSummary, Risk, Rule, ScriptReview};
