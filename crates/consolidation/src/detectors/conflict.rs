//! 冲突记忆分组器：把候选对聚成冲突组
//!
//! 与重复分组器一样，本模块只做**分组与归类**，不产生候选。
//! 候选由 `candidates` 下的模式实现产出，两种模式共用这一层。
//!
//! 冲突判定由两个条件组成：主题相似度落在
//! `[semantic_sim_threshold, duplicate_sim_threshold)` 区间，且**分歧度**超过阈值。
//!
//! - 分歧度使用与检索同源的**分词词元 Jaccard 距离**（CJK bigram + ASCII 词）；
//! - 阈值梯度显式统一：语义冲突 0.82 < 去重/整合 0.92 < 自动合并 0.98；
//!   `duplicate_sim_threshold` 由整合配置注入（此前硬编码 0.98，与去重阈值打架）。

use crate::candidates::{
    CandidatePair, CandidateRelation, REASON_ENVIRONMENT_MISMATCH, REASON_KEY_CONFLICT,
    REASON_SEMANTIC_TOPIC_CONFLICT, REASON_SUPERSEDING_VERSION_CANDIDATE,
};
use crate::detectors::duplicate::UnionFind;
use crate::types::{ConflictGroup, ConflictKind};
use mem_core::MemoryEntry;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ConflictDetector {
    /// 语义冲突的主题相似度下限
    pub semantic_sim_threshold: f32,
    /// 语义冲突的分歧度阈值 (Jaccard 距离)
    pub divergence_threshold: f32,
    /// 语义重复上限：相似度 ≥ 该值视为重复（去重）而非冲突
    pub duplicate_sim_threshold: f32,
    /// 不超过该规模时直接全量两两比较，超过则启用 LSH 候选生成
    pub exhaustive_max_entries: usize,
}

impl Default for ConflictDetector {
    fn default() -> Self {
        Self {
            semantic_sim_threshold: 0.82,
            divergence_threshold: 0.45,
            duplicate_sim_threshold: 0.92,
            exhaustive_max_entries: 256,
        }
    }
}

impl ConflictDetector {
    pub fn new(semantic_sim_threshold: f32, divergence_threshold: f32) -> Self {
        Self {
            semantic_sim_threshold: semantic_sim_threshold.clamp(0.0, 1.0),
            divergence_threshold: divergence_threshold.clamp(0.0, 1.0),
            ..Default::default()
        }
    }

    /// 注入去重/整合阈值（须严格大于语义冲突阈值，保证 0.82 → 0.92 → 0.98 梯度单调）
    pub fn with_duplicate_threshold(mut self, duplicate_sim_threshold: f32) -> Self {
        self.duplicate_sim_threshold = duplicate_sim_threshold
            .clamp(0.0, 1.0)
            .max(self.semantic_sim_threshold + 0.01);
        self
    }

    /// 依据候选对聚类冲突组
    ///
    /// 候选来源：业务键冲突、版本更新候选、语义主题冲突、环境冲突。
    /// 同一业务键下的多个成员会聚成一个组（与既有行为一致）。
    pub fn group_candidates(
        &self,
        entries: &[MemoryEntry],
        candidates: &[CandidatePair],
    ) -> Vec<ConflictGroup> {
        if entries.len() < 2 {
            return Vec::new();
        }
        let index: HashMap<Uuid, usize> =
            entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();

        let mut uf = UnionFind::new(entries.len());
        let mut group_pairs: Vec<&CandidatePair> = Vec::new();
        for c in candidates {
            if !matches!(
                c.relation,
                CandidateRelation::Conflict | CandidateRelation::SupersedingVersion
            ) {
                continue;
            }
            let (Some(&ia), Some(&ib)) = (index.get(&c.a), index.get(&c.b)) else {
                continue;
            };
            uf.union(ia, ib);
            group_pairs.push(c);
        }

        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..entries.len() {
            let root = uf.find(i);
            clusters.entry(root).or_default().push(i);
        }

        let mut conflicts = Vec::new();
        for (_root, mut members) in clusters {
            if members.len() < 2 {
                continue;
            }
            members.sort_by(|&a, &b| entries[b].updated_at.cmp(&entries[a].updated_at));
            let member_set: HashSet<usize> = members.iter().copied().collect();
            let pairs: Vec<&CandidatePair> = group_pairs
                .iter()
                .copied()
                .filter(|c| {
                    let ia = index[&c.a];
                    let ib = index[&c.b];
                    member_set.contains(&ia) && member_set.contains(&ib)
                })
                .collect();
            if pairs.is_empty() {
                continue;
            }

            // 组内共享的业务键（业务键冲突组的成员共享同一个 key）
            let key = members
                .iter()
                .filter_map(|&i| entries[i].key.clone())
                .find(|k| !k.trim().is_empty());

            let kind = classify_kind(&pairs, key.is_some());
            let divergence = if kind == ConflictKind::Key {
                1.0
            } else {
                // 预缓存每成员的分词 HashSet，避免 O(k²) 重复调用 content_divergence
                // 导致的重复分词（每次调用都重新建 HashSet）。50 人冲突组 = 1225 对 × 2 次分词。
                let tokenized: Vec<HashSet<String>> = members
                    .iter()
                    .map(|&i| {
                        mem_core::fts_doc_tokens(&entries[i].content)
                            .into_iter()
                            .collect::<HashSet<String>>()
                    })
                    .collect();
                let mut max_div = 0.0f32;
                for a_pos in 0..tokenized.len() {
                    for b_pos in (a_pos + 1)..tokenized.len() {
                        let t1 = &tokenized[a_pos];
                        let t2 = &tokenized[b_pos];
                        if t1.is_empty() && t2.is_empty() {
                            continue;
                        }
                        let inter = t1.intersection(t2).count() as f32;
                        let union = t1.union(t2).count() as f32;
                        if union > 0.0 {
                            let div = 1.0 - (inter / union);
                            max_div = max_div.max(div);
                        }
                    }
                }
                max_div
            };

            let similarity = pairs.iter().map(|c| c.confidence).fold(0.0f32, f32::max);
            let reason_code = strongest(&pairs).reason_code.clone();
            let description = describe(
                kind,
                key.as_deref(),
                members.len(),
                similarity,
                divergence,
                &reason_code,
            );

            conflicts.push(ConflictGroup {
                id: Uuid::new_v4(),
                kind,
                key,
                member_ids: members.iter().map(|&i| entries[i].id).collect(),
                divergence,
                description,
                suggested_survivor: entries[members[0]].id,
                relation: strongest(&pairs).relation,
                source: strongest(&pairs).source,
                reason_code,
            });
        }
        conflicts
    }

    /// 判断两段事实正文是否呈现事实演化/取代模式 (supersedes)
    pub fn is_fact_evolution(c1: &str, c2: &str) -> bool {
        let markers = [
            "改用",
            "决定改用",
            "切换至",
            "不再使用",
            "过去使用",
            "以前使用",
            "supersede",
            "replace",
            "now use",
            "deprecated",
        ];
        markers.iter().any(|m| c1.contains(m) || c2.contains(m))
    }
}

/// 组内最强的候选（关系优先，其次置信度）
fn strongest<'a>(pairs: &[&'a CandidatePair]) -> &'a CandidatePair {
    let mut best: &CandidatePair = pairs[0];
    for p in pairs.iter().skip(1) {
        let better = p.relation.strength() > best.relation.strength()
            || (p.relation.strength() == best.relation.strength()
                && p.confidence > best.confidence);
        if better {
            best = p;
        }
    }
    best
}

/// 判定冲突子类型
fn classify_kind(pairs: &[&CandidatePair], has_key: bool) -> ConflictKind {
    if has_key && pairs.iter().all(|c| c.reason_code == REASON_KEY_CONFLICT) {
        return ConflictKind::Key;
    }
    if pairs
        .iter()
        .any(|c| c.reason_code == REASON_SUPERSEDING_VERSION_CANDIDATE)
    {
        return ConflictKind::Evolution;
    }
    ConflictKind::Semantic
}

/// 生成冲突描述（沿用既有措辞，便于既有使用者继续阅读）
fn describe(
    kind: ConflictKind,
    key: Option<&str>,
    member_count: usize,
    similarity: f32,
    divergence: f32,
    reason_code: &str,
) -> String {
    match kind {
        ConflictKind::Key => format!(
            "业务键 '{}' 下存在 {} 条不同内容的记忆条目",
            key.unwrap_or("<unknown>"),
            member_count
        ),
        ConflictKind::Evolution => format!(
            "事实版本演进：新事实取代旧事实 (supersedes, 语义主题相似度 {:.2})",
            similarity
        ),
        ConflictKind::Semantic => {
            if reason_code == REASON_ENVIRONMENT_MISMATCH {
                format!(
                    "适用环境不同但内容高度相似 ({:.2})，不可跨环境合并",
                    similarity
                )
            } else if reason_code == REASON_SEMANTIC_TOPIC_CONFLICT {
                format!(
                    "语义主题相似度高 ({:.2}) 但事实内容分歧显著 ({:.2})",
                    similarity, divergence
                )
            } else {
                format!(
                    "结构化字段一致但取值不同 (相似度 {:.2}, 分歧度 {:.2})",
                    similarity, divergence
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::{
        CandidatePair, CandidateRelation, CandidateSource, REASON_ENVIRONMENT_MISMATCH,
        REASON_KEY_CONFLICT, REASON_SEMANTIC_TOPIC_CONFLICT,
    };
    use mem_core::{MemoryKind, MemoryStatus};
    use serde_json::json;
    use uuid::Uuid;

    fn entry(content: &str) -> MemoryEntry {
        let now = chrono::Utc::now();
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
            scene: None,
            layer: None,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        }
    }

    fn pair(a: &MemoryEntry, b: &MemoryEntry, reason: &'static str) -> CandidatePair {
        CandidatePair::new(
            a.id,
            b.id,
            CandidateRelation::Conflict,
            CandidateSource::Structured,
            0.88,
            reason,
        )
    }

    #[test]
    fn test_divergence_uses_tokenization_for_chinese() {
        // 回归：旧实现用 split_whitespace，中文长句整体成一个 token → 分歧度恒为 1.0
        let a = "生产数据库连接数上限为两百";
        let b = "生产数据库连接数上限为两百并预留百分之十余量";
        let div = crate::candidates::hybrid::content_divergence(a, b);
        assert!(div < 0.9, "相关中文句子的分歧度不应被判定为完全分歧：{div}");
        assert!(div > 0.0, "不同文本的分歧度应大于 0：{div}");
        assert_eq!(crate::candidates::hybrid::content_divergence(a, a), 0.0);
    }

    #[test]
    fn test_threshold_ladder_is_monotonic() {
        let d = ConflictDetector::new(0.95, 0.45).with_duplicate_threshold(0.92);
        assert!(
            d.duplicate_sim_threshold > d.semantic_sim_threshold,
            "去重阈值必须严格大于冲突阈值：{} vs {}",
            d.duplicate_sim_threshold,
            d.semantic_sim_threshold
        );
    }

    #[test]
    fn test_key_conflict_is_grouped_into_one_group() {
        let d = ConflictDetector::default();
        let mut a = entry("数据库用 MySQL");
        a.key = Some("db:engine".to_string());
        let mut b = entry("数据库用 PostgreSQL");
        b.key = Some("db:engine".to_string());
        let mut c = entry("数据库用 TiDB");
        c.key = Some("db:engine".to_string());
        let entries = vec![a.clone(), b.clone(), c.clone()];
        let candidates = vec![
            pair(&a, &b, REASON_KEY_CONFLICT),
            pair(&b, &c, REASON_KEY_CONFLICT),
        ];
        let groups = d.group_candidates(&entries, &candidates);
        assert_eq!(groups.len(), 1, "同 key 的成员应聚成一个冲突组");
        assert_eq!(groups[0].member_ids.len(), 3);
        assert_eq!(groups[0].kind, ConflictKind::Key);
        assert_eq!(groups[0].key.as_deref(), Some("db:engine"));
    }

    #[test]
    fn test_environment_mismatch_uses_its_own_description() {
        let d = ConflictDetector::default();
        let a = entry("缓存预热 60 秒");
        let b = entry("缓存预热 60 秒");
        let entries = vec![a.clone(), b.clone()];
        let candidates = vec![pair(&a, &b, REASON_ENVIRONMENT_MISMATCH)];
        let groups = d.group_candidates(&entries, &candidates);
        assert_eq!(groups.len(), 1);
        assert!(
            groups[0].description.contains("适用环境不同"),
            "环境冲突应有专属描述：{}",
            groups[0].description
        );
    }

    #[test]
    fn test_semantic_conflict_keeps_its_reason_code() {
        let d = ConflictDetector::default();
        let a = entry("数据库主库部署在上海可用区 A");
        let b = entry("数据库主库部署在深圳且无只读副本");
        let entries = vec![a.clone(), b.clone()];
        let candidates = vec![pair(&a, &b, REASON_SEMANTIC_TOPIC_CONFLICT)];
        let groups = d.group_candidates(&entries, &candidates);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].reason_code, REASON_SEMANTIC_TOPIC_CONFLICT);
        assert_eq!(groups[0].kind, ConflictKind::Semantic);
    }

    #[test]
    fn test_duplicate_candidates_do_not_produce_conflicts() {
        // 相似度 ≥ 去重阈值的对属于重复链路，不应报成冲突
        let d = ConflictDetector::default();
        let a = entry("服务部署在上海可用区 A");
        let b = entry("服务部署在上海可用区 B");
        let entries = vec![a.clone(), b.clone()];
        let candidates = vec![CandidatePair::new(
            a.id,
            b.id,
            CandidateRelation::SemanticNearDuplicate,
            CandidateSource::Embedding,
            1.0,
            crate::candidates::REASON_SEMANTIC_CANDIDATE_REQUIRES_REVIEW,
        )];
        assert!(d.group_candidates(&entries, &candidates).is_empty());
    }
}
