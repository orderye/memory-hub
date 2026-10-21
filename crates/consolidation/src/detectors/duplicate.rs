//! 重复记忆分组器：把候选对聚成重复组
//!
//! 本模块只做**分组与归类**，不产生候选——候选由 `candidates` 下的模式实现产出。
//! 这样规则模式（纯本地）与向量模式（语义辅助）产出的候选会走**完全相同**的
//! 分组、归类与自动合并门槛逻辑。

use crate::candidates::{CandidatePair, CandidateRelation, CandidateSource};
use crate::types::DuplicateGroup;
use mem_core::MemoryEntry;
use std::collections::{HashMap, HashSet};
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

/// 重复记忆分组器
#[derive(Debug, Clone)]
pub struct DuplicateDetector {
    /// 语义重复下限（仅用于记录与统计，实际判定在候选层完成）
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

    /// 依据候选对聚类重复组
    ///
    /// - **可聚类关系**（精确重复 / 规范化重复 / 语义近重复）走并查集聚类，
    ///   保持既有"传递闭包"语义：A~B 且 B~C 时三者同组。
    /// - **成对关系**（词法近匹配 / 证据不足）只生成两成员组：
    ///   词法相似只说明"这两条值得一起看"，不足以断言"这几条是同一事实"。
    pub fn group_candidates(
        &self,
        entries: &[MemoryEntry],
        candidates: &[CandidatePair],
    ) -> Vec<DuplicateGroup> {
        if entries.len() < 2 {
            return Vec::new();
        }
        let index: HashMap<Uuid, usize> =
            entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();

        let mut uf = UnionFind::new(entries.len());
        let mut cluster_pairs: Vec<&CandidatePair> = Vec::new();
        let mut pair_groups: Vec<&CandidatePair> = Vec::new();

        for c in candidates {
            let (Some(&ia), Some(&ib)) = (index.get(&c.a), index.get(&c.b)) else {
                continue;
            };
            // 沿用既有语义：只有同类型条目才互为重复候选
            if entries[ia].kind != entries[ib].kind {
                continue;
            }
            match c.relation {
                CandidateRelation::ExactDuplicate
                | CandidateRelation::CanonicalDuplicate
                | CandidateRelation::SemanticNearDuplicate => {
                    uf.union(ia, ib);
                    cluster_pairs.push(c);
                }
                CandidateRelation::Uncertain | CandidateRelation::LexicalNearMatch => {
                    pair_groups.push(c);
                }
                _ => {}
            }
        }

        // 按 root 聚合强聚类
        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..entries.len() {
            let root = uf.find(i);
            clusters.entry(root).or_default().push(i);
        }

        let mut groups: Vec<DuplicateGroup> = Vec::new();
        let mut clustered: HashSet<usize> = HashSet::new();

        for (_root, mut members) in clusters {
            if members.len() < 2 {
                continue;
            }
            // 组内排序：按 updated_at 降序（最新条目优先作为幸存者）
            members.sort_by(|&a, &b| entries[b].updated_at.cmp(&entries[a].updated_at));

            let member_set: HashSet<usize> = members.iter().copied().collect();
            let group_pairs: Vec<&CandidatePair> = cluster_pairs
                .iter()
                .copied()
                .filter(|c| {
                    let ia = index[&c.a];
                    let ib = index[&c.b];
                    member_set.contains(&ia) && member_set.contains(&ib)
                })
                .collect();

            let (relation, source, reason_code, max_similarity) = summarize(&group_pairs);
            let member_ids: Vec<Uuid> = members.iter().map(|&idx| entries[idx].id).collect();
            let suggested_survivor = entries[members[0]].id;
            let kind = entries[members[0]].kind;

            for &m in &members {
                clustered.insert(m);
            }

            groups.push(DuplicateGroup {
                id: Uuid::new_v4(),
                kind,
                member_ids,
                max_similarity,
                suggested_survivor,
                relation,
                source,
                reason_code,
            });
        }

        // 成对关系：同组内已聚类的成员不再重复提案
        for c in pair_groups {
            let (Some(&ia), Some(&ib)) = (index.get(&c.a), index.get(&c.b)) else {
                continue;
            };
            if clustered.contains(&ia) && clustered.contains(&ib) {
                continue;
            }
            let mut members = [ia, ib];
            members.sort_by(|&a, &b| entries[b].updated_at.cmp(&entries[a].updated_at));
            groups.push(DuplicateGroup {
                id: Uuid::new_v4(),
                kind: entries[members[0]].kind,
                member_ids: members.iter().map(|&idx| entries[idx].id).collect(),
                max_similarity: c.confidence,
                suggested_survivor: entries[members[0]].id,
                relation: c.relation,
                source: c.source,
                reason_code: c.reason_code.clone(),
            });
        }

        groups
    }
}

/// 汇总一组候选的关系、来源、原因码与最高置信度
///
/// 关系取"最强"（`CandidateRelation::strength`），置信度取最大值。
fn summarize(pairs: &[&CandidatePair]) -> (CandidateRelation, CandidateSource, String, f32) {
    let mut best: Option<&CandidatePair> = None;
    for p in pairs {
        best = Some(match best {
            None => p,
            Some(cur) => {
                // 关系更强者优先；关系相同则置信度更高者优先
                let better = p.relation.strength() > cur.relation.strength()
                    || (p.relation.strength() == cur.relation.strength()
                        && p.confidence > cur.confidence);
                if better {
                    p
                } else {
                    cur
                }
            }
        });
    }
    let max_similarity = pairs.iter().map(|p| p.confidence).fold(0.0f32, f32::max);
    match best {
        Some(p) => (p.relation, p.source, p.reason_code.clone(), max_similarity),
        None => (
            CandidateRelation::Uncertain,
            CandidateSource::Lexical,
            crate::candidates::REASON_LEXICAL_CANDIDATE_REQUIRES_REVIEW.to_string(),
            max_similarity,
        ),
    }
}
