#![forbid(unsafe_code)]

//! 规范包管理、Skill 体系与原子热替换（hub-packs）
//!
//! 提供：
//! - `AtomicPackRegistry`：无锁热替换规范包，灰度分桶调度
//! - `SkillManifest`, `get_canonical_skills`：六 Skill 声明式规范与核心锁定义
//! - `RelationPipeline`：差序格局、人情面子与礼貌准则三阶段流水线

pub mod manifest;
pub mod registry;
pub mod relation;

pub use manifest::{
    get_canonical_skills, CoreLockConfig, RuleStrength, SkillHooks, SkillLayer, SkillManifest,
};
pub use registry::AtomicPackRegistry;
pub use relation::{RelationDecision, RelationInput, RelationPipeline, RelationRing, RelationType};
