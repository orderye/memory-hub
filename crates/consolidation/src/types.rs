//! 整合中枢领域类型定义

use crate::candidates::{CandidateRelation, CandidateSource};
use common::ConsolidationModeStatus;
use mem_core::{IntegrationProposal, MemoryKind};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 语义重复组
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub id: Uuid,
    pub kind: MemoryKind,
    pub member_ids: Vec<Uuid>,
    pub max_similarity: f32,
    pub suggested_survivor: Uuid,
    /// 组内最强的候选关系（决定该组是否允许自动合并）
    pub relation: CandidateRelation,
    /// 候选来源
    pub source: CandidateSource,
    /// 稳定原因码（不含原始文本）
    pub reason_code: String,
}

/// 冲突类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// 业务键相同但正文不一致
    Key,
    /// 语义主题高度相近但断言事实矛盾
    Semantic,
    /// 事实演化（新事实取代旧事实，演化取代 supersedes）
    Evolution,
}

/// 冲突组
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictGroup {
    pub id: Uuid,
    pub kind: ConflictKind,
    pub key: Option<String>,
    pub member_ids: Vec<Uuid>,
    pub divergence: f32,
    pub description: String,
    pub suggested_survivor: Uuid,
    /// 候选关系
    pub relation: CandidateRelation,
    /// 候选来源
    pub source: CandidateSource,
    /// 稳定原因码（不含原始文本）
    pub reason_code: String,
}

/// 陈旧记忆评估
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleEntry {
    pub id: Uuid,
    pub age_days: f32,
    pub confidence: f32,
    pub version: i64,
    pub staleness_score: f32,
}

/// 命名空间整合健康度统计报告
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsolidationStats {
    pub namespace: String,
    /// 活跃记忆总数
    pub total_active: usize,
    /// 识别出的重复条目数
    pub duplicate_entries_count: usize,
    /// 识别出的重复提案组数
    pub duplicate_groups_count: usize,
    /// 冲突条目数
    pub conflict_entries_count: usize,
    /// 冲突组数
    pub conflict_groups_count: usize,
    /// 陈旧条目数
    pub stale_entries_count: usize,
    /// 陈旧度达到自动归档阈值的条目数（对应 AutoPolicy::auto_archive_stale_score，
    /// 仅作为建议计数，不会静默归档）
    pub stale_archive_candidates: usize,
    /// 记忆冗余率 (0.0 ~ 1.0)
    pub redundancy_ratio: f32,
    /// 冲突率 (0.0 ~ 1.0)
    pub conflict_ratio: f32,
    /// 记忆健康度评分 (0.0 ~ 1.0，越接近 1.0 记忆库越整洁健康)
    pub health_score: f32,
}

/// 一键整合运行报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationReport {
    pub namespace: String,
    pub scanned: usize,
    pub dry_run: bool,
    pub proposals_created: usize,
    pub skipped_existing: usize,
    pub auto_merged: usize,
    pub proposals: Vec<IntegrationProposal>,
    pub stats: ConsolidationStats,
    /// 本次运行的候选策略状态（请求模式 / 实际模式 / 降级 / embedding 调用计数）
    pub mode: ConsolidationModeStatus,
}
