//! 候选发现抽象：模式差异只存在于"候选从哪来"，决策层完全共享
//!
//! 整合中枢的流水线被拆成两层：
//!
//! - **候选层**（本模块 + `candidates::rules` / `candidates::hybrid`）：按模式产出
//!   `CandidatePair` 列表。规则模式只看本地数据，向量模式额外用嵌入向量召回。
//! - **决策层**（`engine` / `detectors` / `policy`）：把候选归类、聚类、判冲突、
//!   决定自动合并或生成提案。**两种模式共用同一份实现。**
//!
//! 这样才能保证"A 与 B 共用同一套确定性分类、冲突检查、提案治理和自动合并安全门槛"
//! 不是靠约定，而是结构上就只有一份决策代码。

use async_trait::async_trait;
use common::Result;
use mem_core::{MemoryEntry, MemoryHubStore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod hybrid;
pub mod rules;

/// 候选关系分类
///
/// 决定候选进入哪条处理链路，以及**是否允许自动合并**
/// （见 `policy::merge_eligibility`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateRelation {
    /// 原文完全相同
    ExactDuplicate,
    /// 保守规范化后相同（空白/换行/Unicode 规范形式差异）
    CanonicalDuplicate,
    /// 语义相近但**不能仅凭向量分数证明**相同
    SemanticNearDuplicate,
    /// 本地词法高度重叠，但缺少结构化佐证
    LexicalNearMatch,
    /// 同一事实的时间更新版本
    SupersedingVersion,
    /// 内容、值、有效时间或适用环境相冲突
    Conflict,
    /// 主题相关但应保持两条独立
    RelatedOnly,
    /// 证据不足，保留分立或生成审核提案
    Uncertain,
}

impl CandidateRelation {
    /// 关系的稳定字符串标识（写入提案溯源，不含原始文本）
    pub fn as_str(&self) -> &'static str {
        match self {
            CandidateRelation::ExactDuplicate => "exact_duplicate",
            CandidateRelation::CanonicalDuplicate => "canonical_duplicate",
            CandidateRelation::SemanticNearDuplicate => "semantic_near_duplicate",
            CandidateRelation::LexicalNearMatch => "lexical_near_match",
            CandidateRelation::SupersedingVersion => "superseding_version",
            CandidateRelation::Conflict => "conflict",
            CandidateRelation::RelatedOnly => "related_only",
            CandidateRelation::Uncertain => "uncertain",
        }
    }

    /// 组级关系取"最强"关系时的优先级（数值越大越强）
    ///
    /// **冲突优先于重复**：两条内容完全相同但适用环境不同的记忆，
    /// 绝不能因为"内容一样"就被当成可自动合并的重复项——环境不同意味着
    /// 它们是两条各自成立的事实，而不是同一条事实的重复表述。
    pub fn strength(&self) -> u8 {
        match self {
            CandidateRelation::Conflict => 8,
            CandidateRelation::ExactDuplicate => 7,
            CandidateRelation::CanonicalDuplicate => 6,
            CandidateRelation::SupersedingVersion => 5,
            CandidateRelation::SemanticNearDuplicate => 4,
            CandidateRelation::LexicalNearMatch => 3,
            CandidateRelation::Uncertain => 2,
            CandidateRelation::RelatedOnly => 1,
        }
    }
}

/// 候选来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// 原文逐字节比较
    ExactContent,
    /// 保守规范化指纹比较
    CanonicalFingerprint,
    /// 结构化字段（业务键、场景、标签、环境）
    Structured,
    /// 本地词法索引
    Lexical,
    /// 嵌入向量
    Embedding,
}

impl CandidateSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            CandidateSource::ExactContent => "exact_content",
            CandidateSource::CanonicalFingerprint => "canonical_fingerprint",
            CandidateSource::Structured => "structured",
            CandidateSource::Lexical => "lexical",
            CandidateSource::Embedding => "embedding",
        }
    }
}

// ===== 稳定原因码（可审计，不含任何原始敏感文本） =====

/// 原文完全相同
pub const REASON_EXACT_CONTENT_MATCH: &str = "exact_content_match";
/// 保守规范化指纹相同
pub const REASON_NORMALIZED_FINGERPRINT_MATCH: &str = "normalized_fingerprint_match";
/// 同一业务实体的取值不同
pub const REASON_SAME_ENTITY_CONFLICTING_VALUE: &str = "same_entity_conflicting_value";
/// 语义主题相近但事实矛盾
pub const REASON_SEMANTIC_TOPIC_CONFLICT: &str = "semantic_topic_conflict";
/// 有效期区间冲突
pub const REASON_VALIDITY_PERIOD_CONFLICT: &str = "validity_period_conflict";
/// 适用环境不同
pub const REASON_ENVIRONMENT_MISMATCH: &str = "environment_mismatch";
/// 语义候选（仅向量召回），必须人工复核
pub const REASON_SEMANTIC_CANDIDATE_REQUIRES_REVIEW: &str = "semantic_candidate_requires_review";
/// 词法候选，必须人工复核
pub const REASON_LEXICAL_CANDIDATE_REQUIRES_REVIEW: &str = "lexical_candidate_requires_review";
/// 新版本取代旧版本
pub const REASON_SUPERSEDING_VERSION_CANDIDATE: &str = "superseding_version_candidate";
/// 仅主题相关，保持分立
pub const REASON_RELATED_ONLY: &str = "related_only";
/// 业务键相同但正文不同
pub const REASON_KEY_CONFLICT: &str = "key_conflict";

/// 一个候选对
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidatePair {
    /// 成员 A 的条目 ID
    pub a: Uuid,
    /// 成员 B 的条目 ID
    pub b: Uuid,
    /// 关系分类
    pub relation: CandidateRelation,
    /// 候选来源
    pub source: CandidateSource,
    /// 置信度（0~1）；仅用于排序与展示，**不作为合并依据**
    pub confidence: f32,
    /// 稳定原因码
    pub reason_code: String,
    /// 产生该候选时使用的嵌入模型（向量候选才有）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
}

impl CandidatePair {
    /// 构造候选对（自动按 ID 排序，保证同一对候选的表示唯一）
    pub fn new(
        a: Uuid,
        b: Uuid,
        relation: CandidateRelation,
        source: CandidateSource,
        confidence: f32,
        reason_code: &'static str,
    ) -> Self {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        Self {
            a,
            b,
            relation,
            source,
            confidence: confidence.clamp(0.0, 1.0),
            reason_code: reason_code.to_string(),
            embedding_model: None,
        }
    }

    /// 附上嵌入模型标识（向量候选）
    pub fn with_embedding_model(mut self, model: Option<String>) -> Self {
        self.embedding_model = model;
        self
    }
}

/// 候选判定阈值
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateThresholds {
    /// 向量候选的相似度下限；`None` 表示**未经真实模型标定**，
    /// 此时回落到去重/整合阈值并在响应中标记未标定
    pub embed_min_similarity: Option<f32>,
    /// 词法候选的 Jaccard 相似度下限
    pub lexical_min_similarity: f32,
    /// 去重/整合阈值（语义相似度 ≥ 该值视为重复候选）
    pub duplicate_sim_threshold: f32,
    /// 语义冲突阈值（相似度下界）
    pub semantic_sim_threshold: f32,
    /// 冲突分歧度阈值（Jaccard 距离）
    pub divergence_threshold: f32,
}

impl Default for CandidateThresholds {
    fn default() -> Self {
        Self {
            embed_min_similarity: None,
            lexical_min_similarity: 0.80,
            duplicate_sim_threshold: 0.92,
            semantic_sim_threshold: 0.82,
            divergence_threshold: 0.45,
        }
    }
}

impl CandidateThresholds {
    /// 实际生效的向量候选下限：未标定时回落到去重阈值
    pub fn effective_embed_min_similarity(&self) -> f32 {
        self.embed_min_similarity
            .unwrap_or(self.duplicate_sim_threshold)
            .clamp(0.0, 1.0)
    }

    /// 阈值是否经过真实模型标定
    pub fn embed_threshold_calibrated(&self) -> bool {
        self.embed_min_similarity.is_some()
    }
}

/// 候选发现的规模限制（防止大命名空间一次发出无界请求）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateLimits {
    /// 单次扫描最多新计算的向量条目数
    pub max_embed_entries: usize,
    /// 单次嵌入批大小
    pub embed_batch_size: usize,
    /// 单次扫描最多保留的候选对数量
    pub max_candidate_pairs: usize,
}

impl Default for CandidateLimits {
    fn default() -> Self {
        Self {
            max_embed_entries: 20,
            embed_batch_size: 32,
            max_candidate_pairs: 20_000,
        }
    }
}

/// 候选发现上下文
///
/// **存储在此处按调用时传入，而不是由 finder 构造时捕获**：
/// `ConsolidationEngine::with_store` 会在构造之后把存储换成 `AccountScopedStore`。
/// 若 finder 持有构造时的原始存储，向量召回就会绕过账户隔离在全库检索，
/// 变成"先全库语义搜索、再在应用层过滤权限"。
pub struct CandidateContext<'a> {
    /// 当前生效的存储（已含账户作用域包装）
    pub store: &'a dyn MemoryHubStore,
    /// 当前扫描的命名空间
    pub namespace: &'a str,
    /// 候选空间（调用方已过滤为活跃条目）
    pub entries: &'a [MemoryEntry],
    /// 判定阈值
    pub thresholds: CandidateThresholds,
    /// 规模限制
    pub limits: CandidateLimits,
}

/// 候选发现的产出
#[derive(Debug, Clone, Default)]
pub struct CandidateOutcome {
    /// 候选对
    pub pairs: Vec<CandidatePair>,
    /// 本次对嵌入提供方的调用次数（纯规则模式恒为 0）
    pub embedding_calls: usize,
    /// 本次新计算向量的条目数
    pub embedded_entries: usize,
}

/// 候选发现器
///
/// 实现者**不得**在候选发现阶段改写任何记忆内容，也不得调用生成式接口。
#[async_trait]
pub trait CandidateFinder: Send + Sync {
    /// 在给定候选空间中发现候选对
    async fn find_candidates(&self, ctx: &CandidateContext<'_>) -> Result<CandidateOutcome>;

    /// 候选策略标识（写入运行状态）
    fn strategy(&self) -> &'static str;
}

/// 候选对去重：同一对（a,b）只保留关系最强的一条
///
/// 关系相同时保留置信度更高者；再相同则保留先出现者（候选生成顺序确定，
/// 因此结果可复现）。
pub fn dedup_pairs(mut pairs: Vec<CandidatePair>) -> Vec<CandidatePair> {
    use std::collections::HashMap;
    pairs.sort_by(|x, y| {
        x.a.cmp(&y.a)
            .then(x.b.cmp(&y.b))
            .then(y.relation.strength().cmp(&x.relation.strength()))
            .then(
                y.confidence
                    .partial_cmp(&x.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    let mut seen: HashMap<(Uuid, Uuid), CandidatePair> = HashMap::new();
    for p in pairs {
        seen.entry((p.a, p.b)).or_insert(p);
    }
    let mut out: Vec<CandidatePair> = seen.into_values().collect();
    out.sort_by(|x, y| x.a.cmp(&y.a).then(x.b.cmp(&y.b)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(a: u8, b: u8, relation: CandidateRelation, confidence: f32) -> CandidatePair {
        CandidatePair::new(
            Uuid::from_u128(a as u128),
            Uuid::from_u128(b as u128),
            relation,
            CandidateSource::ExactContent,
            confidence,
            REASON_EXACT_CONTENT_MATCH,
        )
    }

    #[test]
    fn test_pair_is_normalized_by_id_order() {
        let p = pair(2, 1, CandidateRelation::ExactDuplicate, 1.0);
        assert!(p.a < p.b);
    }

    #[test]
    fn test_dedup_keeps_strongest_relation() {
        let weak = pair(1, 2, CandidateRelation::RelatedOnly, 0.9);
        let strong = pair(1, 2, CandidateRelation::ExactDuplicate, 0.5);
        let out = dedup_pairs(vec![weak, strong]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, CandidateRelation::ExactDuplicate);
    }

    #[test]
    fn test_dedup_keeps_higher_confidence_on_equal_relation() {
        let low = pair(1, 2, CandidateRelation::Uncertain, 0.4);
        let high = pair(1, 2, CandidateRelation::Uncertain, 0.8);
        let out = dedup_pairs(vec![low, high]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence, 0.8);
    }

    #[test]
    fn test_uncalibrated_threshold_falls_back_to_duplicate_threshold() {
        let t = CandidateThresholds::default();
        assert!(!t.embed_threshold_calibrated());
        assert_eq!(
            t.effective_embed_min_similarity(),
            t.duplicate_sim_threshold
        );
    }
}
