//! 重复记忆检测器：向量近邻与聚类
//!
//! 结合近邻搜索与并查集聚类，把高余弦相似度条目划分为重复组。

use crate::types::DuplicateGroup;
use mem_core::{cosine_similarity, MemoryEntry, MemoryKind};
use std::collections::HashMap;
use uuid::Uuid;

/// 并查集
pub struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    pub fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    pub fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
}

/// 重复记忆检测器
#[derive(Debug, Clone)]
pub struct DuplicateDetector {
    pub sim_threshold: f32,
    pub probe_k: usize,
}

impl DuplicateDetector {
    pub fn new(sim_threshold: f32) -> Self {
        Self {
            sim_threshold: sim_threshold.clamp(0.0, 1.0),
            probe_k: 20,
        }
    }

    pub fn with_probe_k(mut self, k: usize) -> Self {
        self.probe_k = k.max(1);
        self
    }

    /// 检测条目集合中的重复组
    pub fn detect_in_entries(&self, entries: &[MemoryEntry]) -> Vec<DuplicateGroup> {
        let n = entries.len();
        if n < 2 {
            return Vec::new();
        }

        let mut uf = UnionFind::new(n);
        let mut best_sim = HashMap::<(usize, usize), f32>::new();

        // 按类型分类索引，同 kind 之间才做重复检测
        let mut by_kind: HashMap<MemoryKind, Vec<usize>> = HashMap::new();
        for (idx, e) in entries.iter().enumerate() {
            if e.embedding.is_some() {
                by_kind.entry(e.kind).or_default().push(idx);
            }
        }

        for (_kind, indices) in by_kind {
            if indices.len() < 2 {
                continue;
            }

            // 比较同类型内各条目相似度
            for i_idx in 0..indices.len() {
                let i = indices[i_idx];
                let emb_a = entries[i].embedding.as_ref().unwrap();

                for j_idx in (i_idx + 1)..indices.len() {
                    let j = indices[j_idx];
                    let emb_b = entries[j].embedding.as_ref().unwrap();

                    let sim = cosine_similarity(emb_a, emb_b);
                    if sim >= self.sim_threshold {
                        uf.union(i, j);
                        let pair = if i < j { (i, j) } else { (j, i) };
                        best_sim
                            .entry(pair)
                            .and_modify(|s| *s = s.max(sim))
                            .or_insert(sim);
                    }
                }
            }
        }

        // 按 root 聚合分组
        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..n {
            let root = uf.find(i);
            clusters.entry(root).or_default().push(i);
        }

        let mut groups = Vec::new();
        for (_root, mut members) in clusters {
            if members.len() < 2 {
                continue;
            }

            // 组内排序：按 updated_at 降序（最新条目优先作为幸存者）
            members.sort_by(|&a, &b| entries[b].updated_at.cmp(&entries[a].updated_at));

            let mut max_sim = 0.0f32;
            for i in 0..members.len() {
                for j in (i + 1)..members.len() {
                    let pair = if members[i] < members[j] {
                        (members[i], members[j])
                    } else {
                        (members[j], members[i])
                    };
                    if let Some(&s) = best_sim.get(&pair) {
                        max_sim = max_sim.max(s);
                    }
                }
            }

            let member_ids: Vec<Uuid> = members.iter().map(|&idx| entries[idx].id).collect();
            let suggested_survivor = entries[members[0]].id;
            let kind = entries[members[0]].kind;

            groups.push(DuplicateGroup {
                id: Uuid::new_v4(),
                kind,
                member_ids,
                max_similarity: max_sim,
                suggested_survivor,
            });
        }

        groups
    }
}
