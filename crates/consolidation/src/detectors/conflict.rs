//! 冲突记忆检测器：Key 冲突与语义冲突识别

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
}

impl Default for ConflictDetector {
    fn default() -> Self {
        Self {
            semantic_sim_threshold: 0.82,
            divergence_threshold: 0.45,
        }
    }
}

impl ConflictDetector {
    pub fn new(semantic_sim_threshold: f32, divergence_threshold: f32) -> Self {
        Self {
            semantic_sim_threshold: semantic_sim_threshold.clamp(0.0, 1.0),
            divergence_threshold: divergence_threshold.clamp(0.0, 1.0),
        }
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
                sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
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
                    description: format!("业务键 '{}' 下存在 {} 条不同内容的记忆条目", k, members.len()),
                    suggested_survivor: sorted[0].id,
                });
            }
        }

        // ===== 2. 隐式语义冲突 (Semantic Conflict) 检测 =====
        // 对尚未发生 key 冲突的条目，检测高主题相似但事实表述差异的情况
        let active_unkeyed: Vec<&MemoryEntry> = entries
            .iter()
            .filter(|e| !conflict_member_set.contains(&e.id) && e.embedding.is_some())
            .collect();

        for i in 0..active_unkeyed.len() {
            let e1 = active_unkeyed[i];
            let emb1 = e1.embedding.as_ref().unwrap();

            for j in (i + 1)..active_unkeyed.len() {
                let e2 = active_unkeyed[j];
                // 仅限相同类型
                if e1.kind != e2.kind {
                    continue;
                }
                let emb2 = e2.embedding.as_ref().unwrap();
                let sim = cosine_similarity(emb1, emb2);

                if sim >= self.semantic_sim_threshold && sim < 0.98 {
                    let div = Self::calculate_divergence(&e1.content, &e2.content);
                    if div >= self.divergence_threshold {
                        let survivor = if e1.updated_at >= e2.updated_at {
                            e1.id
                        } else {
                            e2.id
                        };
                        conflicts.push(ConflictGroup {
                            id: Uuid::new_v4(),
                            kind: ConflictKind::Semantic,
                            key: None,
                            member_ids: vec![e1.id, e2.id],
                            divergence: div,
                            description: format!(
                                "语义主题相似度高 ({:.2}) 但事实内容分歧显著 ({:.2})",
                                sim, div
                            ),
                            suggested_survivor: survivor,
                        });
                    }
                }
            }
        }

        conflicts
    }

    /// 基于词项分词的 Jaccard 距离 (1.0 - 词重叠率)
    fn calculate_divergence(c1: &str, c2: &str) -> f32 {
        let t1: HashSet<&str> = c1.split_whitespace().collect();
        let t2: HashSet<&str> = c2.split_whitespace().collect();
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
}
