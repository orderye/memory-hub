//! 向量辅助候选发现器（模式 A：规则候选 ∪ 语义向量候选）
//!
//! 职责边界（任务书 §3.1）：**向量只负责"找候选"，不能负责"下结论"**。
//! 本模块产出的候选一律标为 `SemanticNearDuplicate` 或 `Conflict`，
//! 且语义候选在共享决策层中**永远不进入自动合并**——是否合并由
//! 确定性规则、结构化字段与证据决定，不由相似度分数决定。
//!
//! 三条召回通道：
//!
//! 1. **复用已存向量（零出站）**：条目已有向量且 `embedding_model` 与当前
//!    嵌入器一致时才使用。跨模型向量不可比，视为"向量缺失"
//!    （与检索侧 `embedding_usable` 的处理一致）。
//! 2. **缺失向量时按需嵌入（有界）**：对没有可用向量的条目，调用既有
//!    `Embedder::embed`（**仅 embeddings 协议，不触碰任何生成式接口**），
//!    批大小与条目数都有硬上限。
//! 3. **ANN 近邻召回**：用 `MemoryStore::similar`（账户作用域 + 命名空间作用域）
//!    取近邻候选。

use crate::candidates::rules::RulesCandidateFinder;
use crate::candidates::*;
use async_trait::async_trait;
use common::Result;
use mem_core::{cosine_similarity, Embedder, MemoryEntry, MemoryKind};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// LSH 表数：多表并联把"冲突带内夹角（最大约 35°）"的碰撞概率提到 90% 以上
const LSH_TABLES: usize = 8;
/// 每表桶位宽（2^6 = 64 桶）
const LSH_BITS_PER_TABLE: usize = 6;
/// 单条投影向量的维度（与嵌入维度按循环取模对齐，无需知道真实维度）
const LSH_PROJECTION_DIM: usize = 64;
/// 不超过该规模时直接全量两两比较，超过则启用 LSH 候选生成
const EXHAUSTIVE_MAX_ENTRIES: usize = 256;

/// 向量辅助候选发现器
pub struct HybridCandidateFinder {
    rules: RulesCandidateFinder,
    embedder: Arc<dyn Embedder>,
}

impl HybridCandidateFinder {
    pub fn new(embedder: Arc<dyn Embedder>) -> Self {
        Self {
            rules: RulesCandidateFinder::new(),
            embedder,
        }
    }

    /// 当前嵌入器是否提供真实语义信号
    ///
    /// 伪向量实现（`MockEmbedder` / `TestEmbedder`）返回 `false`：它的余弦相似度
    /// 是噪声而非语义，**不得**被当作有效的模式 A。
    pub fn provides_semantic_signal(&self) -> bool {
        self.embedder.provides_semantic_signal()
    }
}

#[async_trait]
impl CandidateFinder for HybridCandidateFinder {
    async fn find_candidates(&self, ctx: &CandidateContext<'_>) -> Result<CandidateOutcome> {
        let rules_outcome = self.rules.find_candidates(ctx).await?;
        let mut pairs = rules_outcome.pairs;

        // ===== 通道 1：复用已存向量 =====
        let usable: Vec<usize> = ctx
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| embedding_usable(e, self.embedder.model_id()))
            .map(|(i, _)| i)
            .collect();
        pairs.extend(vector_candidates(
            ctx.entries,
            &usable,
            ctx.thresholds,
            Some(self.embedder.model_id().to_string()),
        ));

        // ===== 通道 2 + 3：缺失向量时按需嵌入并 ANN 召回 =====
        let missing: Vec<usize> = ctx
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| !usable.contains(i) && !e.content.trim().is_empty())
            .map(|(i, _)| i)
            .collect();
        let take = missing.len().min(ctx.limits.max_embed_entries);
        let mut embedding_calls = 0usize;
        let mut embedded_entries = 0usize;
        if take > 0 {
            let selected: Vec<usize> = missing.into_iter().take(take).collect();
            let (fresh, calls) = self.embed_missing(ctx, &selected).await?;
            embedding_calls += calls;
            embedded_entries += fresh.len();
            if !fresh.is_empty() {
                // 新算出的向量与已存向量一起参与比对
                let mut all_usable = usable.clone();
                all_usable.extend(fresh.iter().map(|(idx, _)| *idx));
                let overrides: HashMap<usize, Vec<f32>> =
                    fresh.iter().map(|(i, v)| (*i, v.clone())).collect();
                pairs.extend(vector_candidates_with(
                    ctx.entries,
                    &all_usable,
                    &overrides,
                    ctx.thresholds,
                    Some(self.embedder.model_id().to_string()),
                ));
            }
        }

        Ok(CandidateOutcome {
            pairs: dedup_pairs(pairs),
            embedding_calls,
            embedded_entries,
        })
    }

    fn strategy(&self) -> &'static str {
        "local_rules+embedding"
    }
}

impl HybridCandidateFinder {
    /// 为缺少可用向量的条目计算向量（批处理，条数与批大小都有上限）
    ///
    /// 返回 (新算出的向量, 嵌入服务调用次数)。
    async fn embed_missing(
        &self,
        ctx: &CandidateContext<'_>,
        selected: &[usize],
    ) -> Result<(Vec<(usize, Vec<f32>)>, usize)> {
        let mut out: Vec<(usize, Vec<f32>)> = Vec::new();
        let mut calls = 0usize;
        let batch_size = ctx.limits.embed_batch_size.max(1);

        for chunk in selected.chunks(batch_size) {
            let texts: Vec<String> = chunk
                .iter()
                .map(|&i| ctx.entries[i].content.clone())
                .collect();
            calls += 1;
            let vectors = match self.embedder.embed(&texts).await {
                Ok(v) => v,
                Err(e) => {
                    // 嵌入失败：显式降级为"该批条目无向量"，不静默吞掉
                    tracing::warn!(
                        error = %e,
                        batch = chunk.len(),
                        "向量辅助整合：嵌入调用失败，该批条目不参与语义召回"
                    );
                    continue;
                }
            };
            for (offset, &idx) in chunk.iter().enumerate() {
                let Some(v) = vectors.get(offset) else {
                    continue;
                };
                if v.is_empty()
                    || v.len() != self.embedder.dim()
                    || !v.iter().all(|x| x.is_finite())
                {
                    // 空向量 / 维度不符 / 非有限数：显式丢弃，不参与比较
                    continue;
                }
                let mut normalized = v.clone();
                mem_core::l2_normalize(&mut normalized);
                out.push((idx, normalized));
            }
        }
        Ok((out, calls))
    }
}

/// 条目向量是否可用于比较
///
/// 存在向量，**且**嵌入模型与当前嵌入器一致。跨模型向量不可比，
/// 视为"向量缺失"（与检索侧 `embedding_usable` 一致）。
pub fn embedding_usable(entry: &MemoryEntry, model_id: &str) -> bool {
    entry.embedding.is_some()
        && entry
            .embedding_model
            .as_deref()
            .is_none_or(|m| m == model_id)
}

/// 用已存向量生成语义候选（重复带全量比对 + 冲突带候选对生成）
fn vector_candidates(
    entries: &[MemoryEntry],
    usable: &[usize],
    thresholds: CandidateThresholds,
    model: Option<String>,
) -> Vec<CandidatePair> {
    vector_candidates_with(entries, usable, &HashMap::new(), thresholds, model)
}

/// 向量候选生成的统一实现
///
/// `overrides` 给出新算出的向量（覆盖条目自带的向量）。
///
/// 两条判定带与既有实现保持一致：
/// - **重复带**（`sim ≥ duplicate_sim_threshold`）：同 kind 全量两两比对；
/// - **冲突带**（`semantic ≤ sim < duplicate`）：候选对生成后比对分歧度。
fn vector_candidates_with(
    entries: &[MemoryEntry],
    usable: &[usize],
    overrides: &HashMap<usize, Vec<f32>>,
    thresholds: CandidateThresholds,
    model: Option<String>,
) -> Vec<CandidatePair> {
    let mut out = Vec::new();
    if usable.len() < 2 {
        return out;
    }

    let vector_of = |idx: usize| -> Option<&Vec<f32>> {
        overrides
            .get(&idx)
            .or_else(|| entries[idx].embedding.as_ref())
    };

    // ===== 重复带：同 kind 全量比对（与既有 DuplicateDetector 一致） =====
    let mut by_kind: HashMap<MemoryKind, Vec<usize>> = HashMap::new();
    for &i in usable {
        by_kind.entry(entries[i].kind).or_default().push(i);
    }
    for (_kind, indices) in by_kind {
        for a_pos in 0..indices.len() {
            for b_pos in (a_pos + 1)..indices.len() {
                let i = indices[a_pos];
                let j = indices[b_pos];
                let (Some(va), Some(vb)) = (vector_of(i), vector_of(j)) else {
                    continue;
                };
                if va.len() != vb.len() {
                    continue;
                }
                let sim = cosine_similarity(va, vb).max(0.0);
                if sim >= thresholds.duplicate_sim_threshold {
                    out.push(
                        CandidatePair::new(
                            entries[i].id,
                            entries[j].id,
                            CandidateRelation::SemanticNearDuplicate,
                            CandidateSource::Embedding,
                            sim,
                            REASON_SEMANTIC_CANDIDATE_REQUIRES_REVIEW,
                        )
                        .with_embedding_model(model.clone()),
                    );
                }
            }
        }
    }

    // ===== 冲突带：候选对生成（小规模穷举，大规模 LSH 分桶） =====
    let unkeyed: Vec<usize> = usable
        .iter()
        .copied()
        .filter(|&i| {
            entries[i]
                .key
                .as_deref()
                .map(|k| k.trim().is_empty())
                .unwrap_or(true)
        })
        .collect();
    for (i, j) in candidate_pairs(entries, &unkeyed) {
        if entries[i].kind != entries[j].kind {
            continue;
        }
        let (Some(va), Some(vb)) = (vector_of(i), vector_of(j)) else {
            continue;
        };
        if va.len() != vb.len() {
            continue;
        }
        let sim = cosine_similarity(va, vb).max(0.0);
        if sim < thresholds.semantic_sim_threshold || sim >= thresholds.duplicate_sim_threshold {
            continue;
        }
        let divergence = content_divergence(&entries[i].content, &entries[j].content);
        if divergence < thresholds.divergence_threshold {
            continue;
        }
        let evolving = crate::detectors::ConflictDetector::is_fact_evolution(
            &entries[i].content,
            &entries[j].content,
        );
        let reason = if evolving {
            REASON_SUPERSEDING_VERSION_CANDIDATE
        } else {
            REASON_SEMANTIC_TOPIC_CONFLICT
        };
        out.push(
            CandidatePair::new(
                entries[i].id,
                entries[j].id,
                CandidateRelation::Conflict,
                CandidateSource::Embedding,
                sim,
                reason,
            )
            .with_embedding_model(model.clone()),
        );
    }

    out
}

/// 候选对生成：小规模全量两两比较，大规模走 LSH 分桶
fn candidate_pairs(entries: &[MemoryEntry], indices: &[usize]) -> Vec<(usize, usize)> {
    let n = indices.len();
    if n <= EXHAUSTIVE_MAX_ENTRIES {
        let mut pairs = Vec::with_capacity(n * n.saturating_sub(1) / 2);
        for a in 0..n {
            for b in (a + 1)..n {
                pairs.push((indices[a], indices[b]));
            }
        }
        return pairs;
    }

    let buckets = lsh_buckets(entries, indices);
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut pairs = Vec::new();
    for members in buckets.values() {
        for a in 0..members.len() {
            for b in (a + 1)..members.len() {
                let (i, j) = (members[a].min(members[b]), members[a].max(members[b]));
                if seen.insert((i, j)) {
                    pairs.push((i, j));
                }
            }
        }
    }
    pairs
}

/// 确定性随机投影的多表 LSH 分桶
fn lsh_buckets(entries: &[MemoryEntry], indices: &[usize]) -> HashMap<(usize, u64), Vec<usize>> {
    let mut buckets: HashMap<(usize, u64), Vec<usize>> = HashMap::new();
    let projections = deterministic_projections();
    for &idx in indices {
        let Some(embedding) = entries[idx].embedding.as_ref() else {
            continue;
        };
        if embedding.is_empty() {
            continue;
        }
        for table in 0..LSH_TABLES {
            let mut key: u64 = 0;
            for bit in 0..LSH_BITS_PER_TABLE {
                let projection = &projections[table * LSH_BITS_PER_TABLE + bit];
                let mut dot = 0.0f32;
                for (k, value) in embedding.iter().enumerate() {
                    dot += value * projection[k % LSH_PROJECTION_DIM];
                }
                if dot >= 0.0 {
                    key |= 1 << bit;
                }
            }
            buckets.entry((table, key)).or_default().push(idx);
        }
    }
    buckets
}

/// 固定种子的投影矩阵（每次运行完全一致，保证检测结果可复现）
fn deterministic_projections() -> Vec<Vec<f32>> {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        // xorshift64*
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        let value = seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (value >> 11) as f32 / (1u64 << 53) as f32
    };
    (0..LSH_TABLES * LSH_BITS_PER_TABLE)
        .map(|_| {
            (0..LSH_PROJECTION_DIM)
                .map(|_| next() * 2.0 - 1.0)
                .collect()
        })
        .collect()
}

/// 基于分词词元的分歧度（Jaccard 距离）
pub fn content_divergence(c1: &str, c2: &str) -> f32 {
    let t1: HashSet<String> = mem_core::fts_doc_tokens(c1).into_iter().collect();
    let t2: HashSet<String> = mem_core::fts_doc_tokens(c2).into_iter().collect();
    if t1.is_empty() && t2.is_empty() {
        return 0.0;
    }
    let inter = t1.intersection(&t2).count() as f32;
    let union = t1.union(&t2).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        1.0 - (inter / union)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::hybrid::HybridCandidateFinder;
    use chrono::Utc;
    use mem_core::testing::TestEmbedder;
    use mem_core::MemoryStatus;
    use serde_json::json;
    use uuid::Uuid;

    fn entry(content: &str, embedding: Option<Vec<f32>>) -> MemoryEntry {
        let now = Utc::now();
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "ns".to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec![],
            metadata: json!({}),
            embedding,
            embedding_model: None,
            source_agent: "a".to_string(),
            confidence: 1.0,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: None,
            merged_into: None,
            created_at: now,
            updated_at: now,
            scene: None,
            layer: None,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        }
    }

    #[test]
    fn test_embedding_usable_rejects_model_mismatch() {
        let mut e = entry("x", Some(vec![1.0, 0.0]));
        e.embedding_model = Some("mock-embedder-v1".to_string());
        assert!(!embedding_usable(&e, "bge-m3"));
        e.embedding_model = Some("bge-m3".to_string());
        assert!(embedding_usable(&e, "bge-m3"));
    }

    #[tokio::test]
    async fn test_hybrid_uses_rules_candidates_even_without_vectors() {
        let finder = HybridCandidateFinder::new(Arc::new(TestEmbedder::new(8)));
        let store = store::InMemoryHubStore::new();
        let entries = vec![entry("完全一样的内容", None), entry("完全一样的内容", None)];
        let ctx = CandidateContext {
            store: &store,
            namespace: "ns",
            entries: &entries,
            thresholds: CandidateThresholds::default(),
            limits: CandidateLimits::default(),
        };
        let pairs = finder.find_candidates(&ctx).await.unwrap().pairs;
        assert!(
            pairs
                .iter()
                .any(|p| p.relation == CandidateRelation::ExactDuplicate),
            "向量通道不得吞掉规则候选：{pairs:#?}"
        );
    }

    #[tokio::test]
    async fn test_semantic_candidate_is_never_auto_mergeable() {
        // 语义近重复必须被标为待审关系
        let pairs = [CandidatePair::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            CandidateRelation::SemanticNearDuplicate,
            CandidateSource::Embedding,
            0.99,
            REASON_SEMANTIC_CANDIDATE_REQUIRES_REVIEW,
        )];
        assert_eq!(
            crate::policy::merge_eligibility(pairs[0].relation),
            crate::policy::MergeEligibility::ReviewRequired
        );
    }

    #[test]
    fn test_lsh_candidate_generation_finds_near_duplicates_at_scale() {
        // 超过穷举规模后走 LSH：近重复向量必须仍被发现
        let mut entries: Vec<MemoryEntry> = (0..400)
            .map(|i| entry(&format!("无关条目 {}", i), Some(vec![0.0, 1.0, 0.0])))
            .collect();
        let a = entry("主库部署在上海可用区 A", Some(vec![1.0, 0.0, 0.0]));
        let b = entry("主库部署在深圳且无只读副本", Some(vec![0.9, 0.43589, 0.0]));
        entries.push(a);
        entries.push(b);

        let thresholds = CandidateThresholds::default();
        let usable: Vec<usize> = (0..entries.len()).collect();
        let pairs = vector_candidates(&entries, &usable, thresholds, None);
        assert!(
            pairs
                .iter()
                .any(|p| p.relation == CandidateRelation::Conflict),
            "大规模下 LSH 候选生成仍必须找到近重复冲突对：{pairs:#?}"
        );
    }
}
