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
//! 3. **近邻候选生成（进程内）**：对候选空间内的向量做两两比较；条目数超过
//!    `EXHAUSTIVE_MAX_ENTRIES`（256）时改用多表 LSH 分桶，避免 O(n²) 爆炸。
//!
//!    ⚠️ 本实现**不调用** `MemoryStore::similar`：候选空间已经被调用方按
//!    账户 + 命名空间 + 状态收窄到内存里，再走一次存储层 ANN 查询既绕不开
//!    权限边界，也会重复计算。此前注释声称走 ANN，与实现不符，已按实际行为修正。

use crate::candidates::rules::RulesCandidateFinder;
use crate::candidates::*;
use async_trait::async_trait;
use common::Result;
use mem_core::{cosine_similarity, embedding_usable, Embedder, MemoryEntry, MemoryKind};
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

        // ===== 通道 1：复用已存向量（零出站） =====
        let usable: Vec<usize> = ctx
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| embedding_usable(e, self.embedder.model_id()))
            .map(|(i, _)| i)
            .collect();
        let usable_mask: Vec<bool> = {
            let mut mask = vec![false; ctx.entries.len()];
            for &i in &usable {
                mask[i] = true;
            }
            mask
        };

        // ===== 通道 2：缺失向量时按需嵌入（有界） =====
        let missing: Vec<usize> = ctx
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| !usable_mask[*i] && !e.content.trim().is_empty())
            .map(|(i, _)| i)
            .collect();
        let take = missing.len().min(ctx.limits.max_embed_entries);
        let mut embedding_calls = 0usize;
        let mut embedded_entries = 0usize;
        let mut overrides: HashMap<usize, Vec<f32>> = HashMap::new();
        if take > 0 {
            let selected: Vec<usize> = missing.into_iter().take(take).collect();
            let (fresh, calls) = self.embed_missing(ctx, &selected).await?;
            embedding_calls += calls;
            embedded_entries = fresh.len();
            overrides = fresh.into_iter().collect();
        }

        // 向量比对只跑一轮：候选集 = 已存向量 ∪ 新算向量（新算的带 overrides）。
        // 此前分两轮各做一遍全量两两比对——已存向量对被重复计算，重复候选
        // 靠 dedup_pairs 丢弃；单轮的候选集覆盖两轮的并集，结果一致。
        if overrides.is_empty() {
            pairs.extend(vector_candidates(
                ctx.entries,
                &usable,
                ctx.thresholds,
                Some(self.embedder.model_id().to_string()),
            ));
        } else {
            let mut all_usable = usable;
            all_usable.extend(overrides.keys().copied());
            pairs.extend(vector_candidates_with(
                ctx.entries,
                &all_usable,
                &overrides,
                ctx.thresholds,
                Some(self.embedder.model_id().to_string()),
            ));
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

    // ===== 重复带：同 kind 比对（>256 时走 LSH，避免 O(n²) 爆炸） =====
    let mut by_kind: HashMap<MemoryKind, Vec<usize>> = HashMap::new();
    for &i in usable {
        by_kind.entry(entries[i].kind).or_default().push(i);
    }
    for (_kind, indices) in by_kind {
        // candidate_pairs 内部已实现 LSH 分桶：≤256 条穷举，>256 走 LSH 召回
        for (i, j) in candidate_pairs(entries, &indices, overrides) {
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
    for (i, j) in candidate_pairs(entries, &unkeyed, overrides) {
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
///
/// `overrides` 与 [`vector_candidates_with`] 的语义一致：新算出的向量
/// 覆盖条目自带向量参与分桶，保证 LSH 与重复带用的向量完全同源。
fn candidate_pairs(
    entries: &[MemoryEntry],
    indices: &[usize],
    overrides: &HashMap<usize, Vec<f32>>,
) -> Vec<(usize, usize)> {
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

    let buckets = lsh_buckets(entries, indices, overrides);
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
///
/// 分桶向量优先取 `overrides`（通道 2 新算出的），回落到条目自带向量——
/// 与 [`vector_candidates_with`] 的 `vector_of` 完全同源，两路行为一致。
fn lsh_buckets(
    entries: &[MemoryEntry],
    indices: &[usize],
    overrides: &HashMap<usize, Vec<f32>>,
) -> HashMap<(usize, u64), Vec<usize>> {
    let mut buckets: HashMap<(usize, u64), Vec<usize>> = HashMap::new();
    let projections = deterministic_projections();
    for &idx in indices {
        let embedding = overrides
            .get(&idx)
            .or_else(|| entries[idx].embedding.as_ref())
            .filter(|v| !v.is_empty());
        let Some(embedding) = embedding else {
            continue;
        };
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
        assert!(!mem_core::embedding_usable(&e, "bge-m3"));
        e.embedding_model = Some("bge-m3".to_string());
        assert!(mem_core::embedding_usable(&e, "bge-m3"));
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

    #[test]
    fn test_lsh_buckets_honors_overrides() {
        // 两条条目自带"指向完全相反方向"的旧向量；overrides 提供的新向量
        // 才是真实语义。LSH 分桶必须用 overrides——否则这对条目永远不落进
        // 同一桶，冲突带在大规模下（>256 条）漏检。
        let mut entries: Vec<MemoryEntry> = (0..300)
            .map(|i| entry(&format!("噪声 {}", i), Some(vec![0.0, 1.0, 0.0])))
            .collect();
        let idx_a = entries.len();
        entries.push(entry("主库部署在上海可用区 A", Some(vec![1.0, 0.0, 0.0])));
        let idx_b = entries.len();
        entries.push(entry(
            "主库部署在深圳且无只读副本",
            Some(vec![0.0, 1.0, 0.0]),
        ));

        // 候选空间必须超过穷举规模（256）才真正走 LSH 分桶
        let indices: Vec<usize> = (0..entries.len()).collect();
        assert!(indices.len() > EXHAUSTIVE_MAX_ENTRIES);

        // 不带 overrides（修复前的行为）：a 与 b 的旧向量方向相反，
        // 不可能落进同一桶，这对冲突在大规模下被漏检
        let plain = lsh_buckets(&entries, &indices, &HashMap::new());
        assert!(
            !buckets_colocate(&plain, idx_a, idx_b),
            "自带相反向量的两条条目不应在 LSH 下同桶"
        );

        // 带 overrides（embed_missing 新算的向量）：两向量同向近邻，
        // 必须被分进同一桶
        let mut overrides = HashMap::new();
        overrides.insert(idx_a, vec![1.0, 0.0, 0.0]);
        overrides.insert(idx_b, vec![0.9, 0.43589, 0.0]);
        let overridden = lsh_buckets(&entries, &indices, &overrides);
        assert!(
            buckets_colocate(&overridden, idx_a, idx_b),
            "overrides 向量必须参与 LSH 分桶"
        );

        // vector_candidates_with 全链路（冲突带）：噪声条目与 a/b 不同桶，
        // a/b 同桶且相似度落在冲突带 → 必须产出冲突候选对
        let thresholds = CandidateThresholds::default();
        let usable: Vec<usize> = indices.clone();
        let pairs = vector_candidates_with(&entries, &usable, &overrides, thresholds, None);
        let pair_ids = (
            entries[idx_a].id.min(entries[idx_b].id),
            entries[idx_a].id.max(entries[idx_b].id),
        );
        assert!(
            pairs
                .iter()
                .any(|p| p.relation == CandidateRelation::Conflict && (p.a, p.b) == pair_ids),
            "LSH + overrides 全链路必须产出 a/b 冲突候选：{pairs:#?}"
        );
    }

    /// 两条目是否在任一 LSH 表中落进同一桶
    fn buckets_colocate(buckets: &HashMap<(usize, u64), Vec<usize>>, a: usize, b: usize) -> bool {
        buckets
            .values()
            .any(|members| members.contains(&a) && members.contains(&b))
    }
}
