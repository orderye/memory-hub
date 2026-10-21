//! 整合自动化策略引擎

use crate::candidates::CandidateRelation;
use serde::{Deserialize, Serialize};

/// 自动化整合策略模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// 全手动：仅生成提案，绝不自动裁决合并（默认安全模式）
    #[default]
    Manual,
    /// 半自动：超高相似度（>= auto_merge_threshold）且成员数较少时自动合并，其余生成提案
    SemiAuto,
    /// 全自动：重复组自动合并，冲突保留提案
    FullAuto,
}

/// 候选关系的合并资格
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeEligibility {
    /// 满足确定性规则，可进入自动合并门槛判定
    AutoMergeable,
    /// 必须人工复核（只生成提案，不允许自动合并）
    ReviewRequired,
}

/// 关系类型 → 合并资格
///
/// **自动合并的安全门槛由关系类型决定，而不是由相似度分数决定。**
///
/// 相似度高只能说明"这两条值得一起看"，不能说明"这两条相同"：
/// `PostgreSQL 15` 与 `PostgreSQL 16` 语义高度相似，但它们是不同事实。
/// 因此只有**内容完全相同**与**保守规范化后相同**这两类关系可自动合并；
/// 语义近重复、词法近匹配、版本更新与冲突一律降级为待审提案。
pub fn merge_eligibility(relation: CandidateRelation) -> MergeEligibility {
    match relation {
        CandidateRelation::ExactDuplicate | CandidateRelation::CanonicalDuplicate => {
            MergeEligibility::AutoMergeable
        }
        _ => MergeEligibility::ReviewRequired,
    }
}

/// 整合策略配置
///
/// **阈值梯度（单一事实来源）**：
/// `语义冲突 0.82` < `去重/整合 integrate_sim_threshold 0.92` < `自动合并 auto_merge_threshold 0.98`。
/// 语义冲突检测器通过 `with_duplicate_threshold` 接收去重阈值，保证"相似到 0.92 以上算重复、
/// 而不是冲突"；自动合并只对 ≥ 0.98 且成员数受控的重复组生效。
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

    /// 判断候选组是否符合自动合并准则
    ///
    /// 双重门槛：
    /// 1. **关系门槛**：组的合并资格必须是 `AutoMergeable`
    ///    （即组内所有成员对都是精确/规范化重复）；
    /// 2. **数值门槛**：相似度 ≥ `auto_merge_threshold` 且成员数受控。
    ///
    /// 语义近重复即使相似度 0.99 也**不会**自动合并——它只能生成待审提案。
    pub fn should_auto_merge(
        &self,
        eligibility: MergeEligibility,
        similarity: f32,
        member_count: usize,
    ) -> bool {
        if eligibility != MergeEligibility::AutoMergeable {
            return false;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_exact_and_canonical_relations_are_auto_mergeable() {
        assert_eq!(
            merge_eligibility(CandidateRelation::ExactDuplicate),
            MergeEligibility::AutoMergeable
        );
        assert_eq!(
            merge_eligibility(CandidateRelation::CanonicalDuplicate),
            MergeEligibility::AutoMergeable
        );
        for relation in [
            CandidateRelation::SemanticNearDuplicate,
            CandidateRelation::LexicalNearMatch,
            CandidateRelation::SupersedingVersion,
            CandidateRelation::Conflict,
            CandidateRelation::RelatedOnly,
            CandidateRelation::Uncertain,
        ] {
            assert_eq!(
                merge_eligibility(relation),
                MergeEligibility::ReviewRequired,
                "{relation:?} 必须只出待审提案"
            );
        }
    }

    #[test]
    fn test_semantic_candidate_never_auto_merges_even_at_max_similarity() {
        let policy = AutoPolicy::new(PolicyMode::FullAuto);
        assert!(!policy.should_auto_merge(MergeEligibility::ReviewRequired, 1.0, 2));
    }

    #[test]
    fn test_exact_duplicate_auto_merges_under_semi_auto() {
        let policy = AutoPolicy::new(PolicyMode::SemiAuto);
        assert!(policy.should_auto_merge(MergeEligibility::AutoMergeable, 1.0, 2));
    }

    #[test]
    fn test_manual_mode_never_auto_merges() {
        let policy = AutoPolicy::default();
        assert!(!policy.should_auto_merge(MergeEligibility::AutoMergeable, 1.0, 2));
    }
}
