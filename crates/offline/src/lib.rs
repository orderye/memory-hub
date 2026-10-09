#![forbid(unsafe_code)]

//! 离线作业与分析模块（offline）
//!
//! 提供：
//! - `DailyConsolidationJob`：每日衰减与过期清理任务
//! - `ConflictClusterer`：未覆盖冲突聚类分析与规则候选提炼

pub mod clustering;
pub mod consolidation;

pub use clustering::{CandidateRuleProposal, ConflictClusterer, RawConflictEvent};
pub use consolidation::DailyConsolidationJob;
