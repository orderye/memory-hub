//! 规则候选发现器（模式 B：纯本地规则整合）
//!
//! **本实现不持有、也无法持有 `Embedder` 或存储引用**：B 模式的 embedding
//! 调用次数因此在类型层面就是 0，而不是"约定上不调用"。
//!
//! 候选来源：
//!
//! | 规则 | 条件 | 关系 | 自动合并 |
//! |---|---|---|---|
//! | R1 精确重复 | 同 kind 且原文逐字节相同 | `ExactDuplicate` | 可 |
//! | R2 规范化指纹 | 同 kind 且保守规范化指纹相同 | `CanonicalDuplicate` | 可 |
//! | R3/R4 业务键 | 同 key 且正文不同 | `Conflict` / `SupersedingVersion` | 否 |
//! | R5 词法+结构 | 同 kind、场景一致、标签/键有交集、词法高度重叠 | `Uncertain` | 否 |
//! | R6 词法相关 | 同 kind 且词法高度重叠但缺结构佐证 | `RelatedOnly` | 否（不提案） |
//! | R7 环境冲突 | 词法高度重叠且适用环境标签不相交 | `Conflict` | 否 |
//!
//! 所有关系判定都只使用本地数据：正文、规范化指纹、结构化字段与本地词法索引。

use crate::candidates::*;
use async_trait::async_trait;
use common::Result;
use mem_core::{MemoryEntry, MemoryKind};
use std::collections::{HashMap, HashSet};

/// 单个同质桶最多产出的候选对数（防止超大同质桶产生组合爆炸）
const MAX_PAIRS_PER_BUCKET: usize = 1000;
/// 参与倒排索引的最大文档频（过于常见的词元不参与候选生成）
const MAX_TOKEN_DOC_FREQ: usize = 64;
/// 每个条目参与候选生成的最大词元数（取文档频最低者，保证确定性）
const MAX_TOKENS_PER_ENTRY: usize = 24;

/// 纯本地规则候选发现器
#[derive(Debug, Clone, Default)]
pub struct RulesCandidateFinder;

impl RulesCandidateFinder {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl CandidateFinder for RulesCandidateFinder {
    async fn find_candidates(&self, ctx: &CandidateContext<'_>) -> Result<CandidateOutcome> {
        let entries = ctx.entries;
        if entries.len() < 2 {
            return Ok(CandidateOutcome::default());
        }

        let mut pairs: Vec<CandidatePair> = Vec::new();

        // ===== R1 精确重复 =====
        let mut by_content: HashMap<(MemoryKind, &str), Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_content
                .entry((e.kind, e.content.as_str()))
                .or_default()
                .push(i);
        }
        for ((_kind, _content), idxs) in by_content {
            if idxs.len() < 2 {
                continue;
            }
            'outer: for a_pos in 0..idxs.len() {
                for b_pos in (a_pos + 1)..idxs.len() {
                    pairs.push(CandidatePair::new(
                        entries[idxs[a_pos]].id,
                        entries[idxs[b_pos]].id,
                        CandidateRelation::ExactDuplicate,
                        CandidateSource::ExactContent,
                        1.0,
                        REASON_EXACT_CONTENT_MATCH,
                    ));
                    if pairs.len() >= MAX_PAIRS_PER_BUCKET * 8 {
                        tracing::warn!(
                            pairs = pairs.len(),
                            "同质正文桶过大，精确重复候选已截断；请分批清理该命名空间"
                        );
                        break 'outer;
                    }
                }
            }
        }

        // ===== R2 保守规范化指纹 =====
        let mut by_fingerprint: HashMap<(MemoryKind, String), Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_fingerprint
                .entry((e.kind, mem_core::content_fingerprint(&e.content)))
                .or_default()
                .push(i);
        }
        for ((_kind, _fp), idxs) in by_fingerprint {
            if idxs.len() < 2 {
                continue;
            }
            'outer_fp: for a_pos in 0..idxs.len() {
                for b_pos in (a_pos + 1)..idxs.len() {
                    let a = &entries[idxs[a_pos]];
                    let b = &entries[idxs[b_pos]];
                    // 原文已完全相同的对已由 R1 覆盖
                    if a.content == b.content {
                        continue;
                    }
                    pairs.push(CandidatePair::new(
                        a.id,
                        b.id,
                        CandidateRelation::CanonicalDuplicate,
                        CandidateSource::CanonicalFingerprint,
                        1.0,
                        REASON_NORMALIZED_FINGERPRINT_MATCH,
                    ));
                    if pairs.len() >= MAX_PAIRS_PER_BUCKET * 8 {
                        tracing::warn!(
                            pairs = pairs.len(),
                            "规范化指纹桶过大，重复候选已截断；请分批清理该命名空间"
                        );
                        break 'outer_fp;
                    }
                }
            }
        }

        // ===== R3 / R4 业务键分组 =====
        let mut by_key: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            if let Some(k) = e.key.as_deref() {
                if !k.trim().is_empty() {
                    by_key.entry(k).or_default().push(i);
                }
            }
        }
        for (_key, idxs) in by_key {
            if idxs.len() < 2 {
                continue;
            }
            let distinct: HashSet<&str> =
                idxs.iter().map(|&i| entries[i].content.as_str()).collect();
            if distinct.len() < 2 {
                continue;
            }
            for a_pos in 0..idxs.len() {
                for b_pos in (a_pos + 1)..idxs.len() {
                    let a = &entries[idxs[a_pos]];
                    let b = &entries[idxs[b_pos]];
                    if a.content == b.content {
                        continue;
                    }
                    // 时间可定序 → 版本更新候选；否则 → 冲突
                    let ordered = has_ordered_validity(a, b);
                    let evolving = crate::detectors::ConflictDetector::is_fact_evolution(
                        &a.content, &b.content,
                    );
                    let (relation, reason) = if ordered || evolving {
                        (
                            CandidateRelation::SupersedingVersion,
                            REASON_SUPERSEDING_VERSION_CANDIDATE,
                        )
                    } else {
                        (CandidateRelation::Conflict, REASON_KEY_CONFLICT)
                    };
                    pairs.push(CandidatePair::new(
                        a.id,
                        b.id,
                        relation,
                        CandidateSource::Structured,
                        1.0,
                        reason,
                    ));
                }
            }
        }

        // ===== R5 / R6 / R7 本地词法候选 =====
        let lexical = lexical_candidates(
            entries,
            ctx.thresholds.lexical_min_similarity,
            ctx.limits.max_candidate_pairs,
        );
        pairs.extend(lexical);

        // 纯规则模式：本实现不持有嵌入器，embedding 调用恒为 0
        Ok(CandidateOutcome {
            pairs: dedup_pairs(pairs),
            embedding_calls: 0,
            embedded_entries: 0,
        })
    }

    fn strategy(&self) -> &'static str {
        "local_rules"
    }
}

/// 两条事实是否可确定时间先后（用于区分"版本更新"与"事实冲突"）
fn has_ordered_validity(a: &MemoryEntry, b: &MemoryEntry) -> bool {
    match (a.valid_from, b.valid_from) {
        (Some(x), Some(y)) => x != y,
        _ => false,
    }
}

/// 提取条目的适用环境标签
///
/// 来源：`metadata.envs` / `metadata.boundary_envs` / `metadata.environments`
/// 数组字段，以及 `env:` 前缀的标签。
fn env_labels(entry: &MemoryEntry) -> HashSet<String> {
    let mut out = HashSet::new();
    for key in ["envs", "boundary_envs", "environments"] {
        if let Some(arr) = entry.metadata.get(key).and_then(|v| v.as_array()) {
            for v in arr {
                if let Some(s) = v.as_str() {
                    let s = s.trim().to_lowercase();
                    if !s.is_empty() {
                        out.insert(s);
                    }
                }
            }
        }
    }
    for tag in &entry.tags {
        if let Some(rest) = tag.strip_prefix("env:") {
            let s = rest.trim().to_lowercase();
            if !s.is_empty() {
                out.insert(s);
            }
        }
    }
    out
}

/// 词法候选：倒排索引生成候选对，再按 Jaccard 相似度与结构佐证分级
fn lexical_candidates(
    entries: &[MemoryEntry],
    min_similarity: f32,
    max_pairs: usize,
) -> Vec<CandidatePair> {
    let n = entries.len();
    if n < 2 {
        return Vec::new();
    }

    // 词元集合 + 倒排索引
    let mut token_sets: Vec<HashSet<String>> = Vec::with_capacity(n);
    let mut postings: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let tokens: HashSet<String> = mem_core::fts_doc_tokens(&e.content).into_iter().collect();
        for t in &tokens {
            postings.entry(t.clone()).or_default().push(i);
        }
        token_sets.push(tokens);
    }

    // 候选对生成：每条目的稀有词元（文档频最低）参与配对
    let mut candidate_pairs: HashSet<(usize, usize)> = HashSet::new();
    'pairs: for (i, entry_tokens) in token_sets.iter().enumerate() {
        let mut tokens: Vec<&String> = entry_tokens.iter().collect();
        tokens.sort_by(|a, b| {
            let da = postings.get(*a).map(|p| p.len()).unwrap_or(0);
            let db = postings.get(*b).map(|p| p.len()).unwrap_or(0);
            da.cmp(&db).then_with(|| a.cmp(b))
        });
        for token in tokens.into_iter().take(MAX_TOKENS_PER_ENTRY) {
            let Some(list) = postings.get(token) else {
                continue;
            };
            if list.len() > MAX_TOKEN_DOC_FREQ {
                continue;
            }
            for &j in list {
                if j == i {
                    continue;
                }
                candidate_pairs.insert((i.min(j), i.max(j)));
                if candidate_pairs.len() >= max_pairs {
                    tracing::warn!(
                        max_pairs,
                        "词法候选对数量达到上限，候选已截断；请分批清理该命名空间"
                    );
                    break 'pairs;
                }
            }
        }
    }

    let mut out = Vec::new();
    for (i, j) in candidate_pairs {
        let a = &entries[i];
        let b = &entries[j];
        // 沿用既有语义：只有同类型条目才互为候选
        if a.kind != b.kind {
            continue;
        }
        let similarity = jaccard(&token_sets[i], &token_sets[j]);
        if similarity < min_similarity {
            continue;
        }

        let env_a = env_labels(a);
        let env_b = env_labels(b);
        if !env_a.is_empty() && !env_b.is_empty() && env_a.is_disjoint(&env_b) {
            // R7 同一经验但适用环境不同：不可跨环境合并
            out.push(CandidatePair::new(
                a.id,
                b.id,
                CandidateRelation::Conflict,
                CandidateSource::Structured,
                similarity,
                REASON_ENVIRONMENT_MISMATCH,
            ));
            continue;
        }

        let scene_agrees = a.scene == b.scene;
        let tag_overlap = a
            .tags
            .iter()
            .any(|t| !t.is_empty() && b.tags.iter().any(|x| x == t));
        let key_agrees = match (&a.key, &b.key) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        };

        if scene_agrees && (tag_overlap || key_agrees) {
            // R5 词法高度重叠且结构化字段一致：生成可审核提案，不自动合并
            out.push(CandidatePair::new(
                a.id,
                b.id,
                CandidateRelation::Uncertain,
                CandidateSource::Lexical,
                similarity,
                REASON_LEXICAL_CANDIDATE_REQUIRES_REVIEW,
            ));
        } else {
            // R6 仅主题相关：保持分立
            out.push(CandidatePair::new(
                a.id,
                b.id,
                CandidateRelation::RelatedOnly,
                CandidateSource::Lexical,
                similarity,
                REASON_RELATED_ONLY,
            ));
        }
    }
    out
}

/// 词元 Jaccard 相似度
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use mem_core::{MemoryScene, MemoryStatus};
    use serde_json::json;
    use uuid::Uuid;

    fn entry(content: &str) -> MemoryEntry {
        let now = Utc::now();
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "ns".to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec![],
            metadata: json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "a".to_string(),
            confidence: 1.0,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: None,
            merged_into: None,
            created_at: now,
            updated_at: now,
            scene: Some(MemoryScene::Chat),
            layer: None,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        }
    }

    /// 构造候选上下文
    ///
    /// 规则候选发现**不访问存储**，但上下文仍要求一个存储引用——这里给一个真实的
    /// 空内存存储，确保"finder 拿不到可用数据"这件事不会在测试里被掩盖。
    fn ctx<'a>(
        store: &'a dyn mem_core::MemoryHubStore,
        entries: &'a [MemoryEntry],
    ) -> CandidateContext<'a> {
        CandidateContext {
            store,
            namespace: "ns",
            entries,
            thresholds: CandidateThresholds::default(),
            limits: CandidateLimits::default(),
        }
    }

    #[tokio::test]
    async fn test_exact_duplicate_detected() {
        let store = store::InMemoryHubStore::new();
        let entries = vec![entry("完全相同的事实"), entry("完全相同的事实")];
        let pairs = RulesCandidateFinder::new()
            .find_candidates(&ctx(&store, &entries))
            .await
            .unwrap()
            .pairs;
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].relation, CandidateRelation::ExactDuplicate);
        assert_eq!(pairs[0].reason_code, REASON_EXACT_CONTENT_MATCH);
    }

    #[tokio::test]
    async fn test_canonical_duplicate_detected() {
        let store = store::InMemoryHubStore::new();
        let entries = vec![entry("服务  已上线\n"), entry("服务 已上线")];
        let pairs = RulesCandidateFinder::new()
            .find_candidates(&ctx(&store, &entries))
            .await
            .unwrap()
            .pairs;
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].relation, CandidateRelation::CanonicalDuplicate);
    }

    #[tokio::test]
    async fn test_version_difference_is_not_duplicate() {
        let store = store::InMemoryHubStore::new();
        let entries = vec![entry("服务版本 v1"), entry("服务版本 v2")];
        let pairs = RulesCandidateFinder::new()
            .find_candidates(&ctx(&store, &entries))
            .await
            .unwrap()
            .pairs;
        assert!(
            !pairs.iter().any(|p| matches!(
                p.relation,
                CandidateRelation::ExactDuplicate | CandidateRelation::CanonicalDuplicate
            )),
            "版本号不同不得判为重复：{pairs:#?}"
        );
    }

    #[tokio::test]
    async fn test_key_conflict_detected() {
        let mut a = entry("数据库用 MySQL");
        a.key = Some("db:engine".to_string());
        let mut b = entry("数据库用 PostgreSQL");
        b.key = Some("db:engine".to_string());
        let store = store::InMemoryHubStore::new();
        let entries = vec![a, b];
        let pairs = RulesCandidateFinder::new()
            .find_candidates(&ctx(&store, &entries))
            .await
            .unwrap()
            .pairs;
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].relation, CandidateRelation::Conflict);
    }

    #[tokio::test]
    async fn test_lexical_without_structural_evidence_is_related_only() {
        let store = store::InMemoryHubStore::new();
        let entries = vec![
            entry("生产数据库部署在上海可用区"),
            entry("生产数据库部署在深圳可用区"),
        ];
        let pairs = RulesCandidateFinder::new()
            .find_candidates(&ctx(&store, &entries))
            .await
            .unwrap()
            .pairs;
        assert!(
            pairs
                .iter()
                .all(|p| p.relation == CandidateRelation::RelatedOnly),
            "缺结构佐证时只应保持分立：{pairs:#?}"
        );
    }

    #[tokio::test]
    async fn test_environment_mismatch_is_conflict() {
        let mut a = entry("缓存预热策略：启动后预热 60 秒");
        a.metadata = json!({"envs": ["prod"]});
        let mut b = entry("缓存预热策略：启动后预热 60 秒");
        b.metadata = json!({"envs": ["staging"]});
        let store = store::InMemoryHubStore::new();
        let entries = vec![a, b];
        let pairs = RulesCandidateFinder::new()
            .find_candidates(&ctx(&store, &entries))
            .await
            .unwrap()
            .pairs;
        assert!(
            pairs
                .iter()
                .any(|p| p.relation == CandidateRelation::Conflict
                    && p.reason_code == REASON_ENVIRONMENT_MISMATCH),
            "环境标签冲突必须是冲突候选：{pairs:#?}"
        );
    }
}
