//! 冲突记忆检测器：Key 冲突与语义冲突识别
//!
//! 语义冲突的判定由两个条件组成：主题相似度落在
//! `[semantic_sim_threshold, duplicate_sim_threshold)` 区间，且**分歧度**超过阈值。
//!
//! - 分歧度使用与检索同源的**分词词元 Jaccard 距离**（CJK bigram + ASCII 词），
//!   不再用 `split_whitespace`——中文长句没有空格时会被整体当作 token，使分歧度恒为 1.0；
//! - 阈值梯度显式统一：语义冲突 0.82 < 去重/整合 0.92 < 自动合并 0.98；
//!   `duplicate_sim_threshold` 由整合配置注入（此前硬编码 0.98，与去重阈值打架）；
//! - 候选对生成：小规模（≤ `exhaustive_max_entries`）直接两两比较；超过则用
//!   确定性随机投影的 LSH 分桶，只在同桶内比较——O(n²) 降为近似 O(n)，
//!   近重复向量必然同桶（符号投影对近似向量稳定），代价是跨桶的远距离对可能被跳过。

use crate::types::{ConflictGroup, ConflictKind};
use mem_core::{cosine_similarity, MemoryEntry};
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

/// LSH 表数：多表并联把"冲突带内夹角（最大约 35°）"的碰撞概率提到 90% 以上
const LSH_TABLES: usize = 8;
/// 每表桶位宽（2^6 = 64 桶）
const LSH_BITS_PER_TABLE: usize = 6;
/// 单条投影向量的维度（与嵌入维度按循环取模对齐，无需知道真实维度）
const LSH_PROJECTION_DIM: usize = 64;

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

    /// 在条目集合中检测 Key 冲突与语义冲突
    pub fn detect_in_entries(&self, entries: &[MemoryEntry]) -> Vec<ConflictGroup> {
        let mut conflicts = Vec::new();
        let mut conflict_member_set: HashSet<Uuid> = HashSet::new();

        // ===== 1. 显式业务键 (Key) 冲突检测 =====
        let mut by_key: HashMap<&str, Vec<&MemoryEntry>> = HashMap::new();
        for e in entries {
            if let Some(ref k) = e.key {
                by_key.entry(k.as_str()).or_default().push(e);
            }
        }

        for (k, members) in by_key {
            if members.len() < 2 {
                continue;
            }
            let distinct_contents: HashSet<&str> =
                members.iter().map(|m| m.content.as_str()).collect();
            if distinct_contents.len() >= 2 {
                let mut sorted = members.clone();
                sorted.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
                let ids: Vec<Uuid> = sorted.iter().map(|m| m.id).collect();
                for id in &ids {
                    conflict_member_set.insert(*id);
                }
                conflicts.push(ConflictGroup {
                    id: Uuid::new_v4(),
                    kind: ConflictKind::Key,
                    key: Some(k.to_string()),
                    member_ids: ids,
                    divergence: 1.0,
                    description: format!(
                        "业务键 '{}' 下存在 {} 条不同内容的记忆条目",
                        k,
                        members.len()
                    ),
                    suggested_survivor: sorted[0].id,
                });
            }
        }

        // ===== 2. 隐式语义冲突 (Semantic Conflict) 检测 =====
        // 对尚未发生 key 冲突且有嵌入向量的条目，检测高主题相似但事实表述差异的情况
        let active_unkeyed: Vec<&MemoryEntry> = entries
            .iter()
            .filter(|e| !conflict_member_set.contains(&e.id) && e.embedding.is_some())
            .collect();

        for (i, j) in self.candidate_pairs(&active_unkeyed) {
            let e1 = active_unkeyed[i];
            let e2 = active_unkeyed[j];

            // 仅限相同类型
            if e1.kind != e2.kind {
                continue;
            }
            let emb1 = e1.embedding.as_ref().unwrap();
            let emb2 = e2.embedding.as_ref().unwrap();
            let sim = cosine_similarity(emb1, emb2);

            if sim >= self.semantic_sim_threshold && sim < self.duplicate_sim_threshold {
                let div = Self::calculate_divergence(&e1.content, &e2.content);
                if div >= self.divergence_threshold {
                    let survivor = if e1.updated_at >= e2.updated_at {
                        e1.id
                    } else {
                        e2.id
                    };
                    let is_evolution = Self::is_fact_evolution(&e1.content, &e2.content);
                    let kind = if is_evolution {
                        ConflictKind::Evolution
                    } else {
                        ConflictKind::Semantic
                    };
                    let description = if is_evolution {
                        format!(
                            "事实版本演进：新事实取代旧事实 (supersedes, 语义主题相似度 {:.2})",
                            sim
                        )
                    } else {
                        format!(
                            "语义主题相似度高 ({:.2}) 但事实内容分歧显著 ({:.2})",
                            sim, div
                        )
                    };
                    conflicts.push(ConflictGroup {
                        id: Uuid::new_v4(),
                        kind,
                        key: None,
                        member_ids: vec![e1.id, e2.id],
                        divergence: div,
                        description,
                        suggested_survivor: survivor,
                    });
                }
            }
        }

        conflicts
    }

    /// 候选对生成：小规模全量两两比较，大规模走 LSH 分桶
    fn candidate_pairs(&self, entries: &[&MemoryEntry]) -> Vec<(usize, usize)> {
        let n = entries.len();
        if n <= self.exhaustive_max_entries {
            let mut pairs = Vec::with_capacity(n * (n.saturating_sub(1)) / 2);
            for i in 0..n {
                for j in (i + 1)..n {
                    pairs.push((i, j));
                }
            }
            return pairs;
        }

        let buckets = lsh_buckets(entries);
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

    /// 基于分词词元的分歧度（Jaccard 距离，1.0 - 词元重叠率）
    ///
    /// 与检索共用 `mem_core::fts_doc_tokens`：CJK 走 bigram、ASCII 走词元边界，
    /// 中文长句不再因为"没有空格被当成一个 token"而恒判为完全分歧。
    fn calculate_divergence(c1: &str, c2: &str) -> f32 {
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

/// 确定性随机投影的多表 LSH 分桶：以固定种子的 xorshift 生成投影向量，
/// 每表用 6 个符号位组成桶键，只要任一表与近邻同桶即成为候选对。
///
/// 选择 8 表 × 6 位而非单表 12 位：随机超平面 LSH 的碰撞概率随夹角下降很快，
/// 而"冲突带"（相似度 0.82~0.92）的最大夹角约 35°，单表方案会系统性漏掉带边缘的冲突；
/// 多表并联把任一表碰撞的概率提高到 90% 以上（代价是候选对数略增）。
fn lsh_buckets(entries: &[&MemoryEntry]) -> HashMap<(usize, u64), Vec<usize>> {
    let mut buckets: HashMap<(usize, u64), Vec<usize>> = HashMap::new();
    let projections = deterministic_projections();
    for (idx, entry) in entries.iter().enumerate() {
        let Some(embedding) = entry.embedding.as_ref() else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::{CognitiveLayer, MemoryKind, MemoryScene, MemoryStatus};
    use serde_json::json;

    fn entry(content: &str, embedding: Option<Vec<f32>>) -> MemoryEntry {
        let now = chrono::Utc::now();
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
            scene: Some(MemoryScene::Chat),
            layer: Some(CognitiveLayer::Fact),
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        }
    }

    #[test]
    fn test_divergence_uses_tokenization_for_chinese() {
        // 回归：旧实现用 split_whitespace，中文长句整体成一个 token → 分歧度恒为 1.0
        let a = "生产数据库连接数上限为两百";
        let b = "生产数据库连接数上限为两百并预留百分之十余量";
        let div = ConflictDetector::calculate_divergence(a, b);
        assert!(div < 0.9, "相关中文句子的分歧度不应被判定为完全分歧：{div}");
        assert!(div > 0.0, "不同文本的分歧度应大于 0：{div}");
        assert_eq!(ConflictDetector::calculate_divergence(a, a), 0.0);
    }

    #[test]
    fn test_duplicate_similarity_is_not_reported_as_conflict() {
        // 语义重复（相似度 ≥ 去重阈值）应交给去重链路，不应报成冲突
        let d = ConflictDetector::default().with_duplicate_threshold(0.92);
        let a = entry("服务部署在上海可用区 A", Some(vec![1.0, 0.0, 0.0]));
        let b = entry("服务部署在上海可用区 B", Some(vec![1.0, 0.0, 0.0]));
        let conflicts = d.detect_in_entries(&[a, b]);
        assert!(
            conflicts.is_empty(),
            "相似度 1.0 ≥ 去重阈值，不属于冲突：{conflicts:#?}"
        );
    }

    #[test]
    fn test_conflicting_facts_are_detected() {
        let d = ConflictDetector::default();
        // 相似度 0.88：落在冲突带 [0.82, 0.92) 内（1.0 属于重复，应由去重链路处理）
        let mut a = entry("数据库主库部署在上海可用区 A", Some(vec![1.0, 0.0, 0.0]));
        let mut b = entry(
            "数据库主库部署在深圳且无只读副本",
            Some(vec![0.88, 0.475, 0.0]),
        );
        a.updated_at = chrono::Utc::now();
        b.updated_at = chrono::Utc::now() - chrono::Duration::seconds(5);
        let conflicts = d.detect_in_entries(&[a.clone(), b]);
        assert_eq!(conflicts.len(), 1, "高相似 + 高分歧必须报为冲突");
        assert_eq!(conflicts[0].suggested_survivor, a.id);
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
    fn test_lsh_candidate_generation_finds_near_duplicates_at_scale() {
        // 超过 exhaustive 规模后走 LSH：近重复向量必须仍被发现
        let mut entries: Vec<MemoryEntry> = (0..400)
            .map(|i| entry(&format!("无关条目 {}", i), Some(vec![0.0, 1.0, 0.0])))
            .collect();
        let mut a = entry("主库部署在上海可用区 A", Some(vec![1.0, 0.0, 0.0]));
        a.updated_at = chrono::Utc::now();
        let b = entry("主库部署在深圳且无只读副本", Some(vec![0.9, 0.43589, 0.0]));
        entries.push(a);
        entries.push(b);

        let detector = ConflictDetector {
            exhaustive_max_entries: 100,
            ..ConflictDetector::default()
        };
        let conflicts = detector.detect_in_entries(&entries);
        assert!(
            conflicts
                .iter()
                .any(|c| c.member_ids.len() == 2 && c.divergence > 0.0),
            "大规模下 LSH 候选生成仍必须找到近重复冲突对：{conflicts:#?}"
        );
    }
}
