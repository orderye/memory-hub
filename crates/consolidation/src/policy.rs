//! 整合自动化策略引擎

use serde::{Deserialize, Serialize};

/// 自动化整合策略模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// 全手动：仅生成提案，绝不自动裁决合并（默认安全模式）
    Manual,
    /// 半自动：超高相似度（>= auto_merge_threshold）且成员数较少时自动合并，其余生成提案
    SemiAuto,
    /// 全自动：重复组自动合并，冲突保留提案
    FullAuto,
}

impl Default for PolicyMode {
    fn default() -> Self {
        PolicyMode::Manual
    }
}

/// 整合策略配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoPolicy {
    pub mode: PolicyMode,
    /// 自动合并的相似度阈值（仅在 SemiAuto/FullAuto 下生效）
    pub auto_merge_threshold: f32,
    /// 自动合并支持的最大成员数（超过则必须人工审查）
    pub auto_merge_max_members: usize,
    /// 自动归档陈旧条目的阈值评分
    pub auto_archive_stale_score: f32,
}

impl Default for AutoPolicy {
    fn default() -> Self {
        Self {
            mode: PolicyMode::Manual,
            auto_merge_threshold: 0.98,
            auto_merge_max_members: 3,
            auto_archive_stale_score: 0.85,
        }
    }
}

impl AutoPolicy {
    pub fn new(mode: PolicyMode) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }

    /// 判断重复组是否符合自动合并准则
    pub fn should_auto_merge(&self, similarity: f32, member_count: usize) -> bool {
        match self.mode {
            PolicyMode::Manual => false,
            PolicyMode::SemiAuto => {
                similarity >= self.auto_merge_threshold
                    && member_count <= self.auto_merge_max_members
            }
            PolicyMode::FullAuto => similarity >= self.auto_merge_threshold,
        }
    }
}
