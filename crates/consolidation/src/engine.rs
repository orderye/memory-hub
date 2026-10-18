//! 整合中枢核心引擎：统一协调检测、融合与策略调度

use crate::detectors::{ConflictDetector, DuplicateDetector, StalenessDetector};
use crate::merger::{ContentMerger, TieredMerger};
use crate::policy::AutoPolicy;
use crate::types::{ConsolidationReport, ConsolidationStats};
use chrono::Utc;
use common::{AgentId, HubError, Result};
use mem_core::{
    Embedder, IntegrationKind, IntegrationProposal, MemoryEntry, MemoryHubStore, MemoryStatus,
    ProposalStatus, ResolveAction, ResolveRequest, VersionChange,
};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

/// 整合中枢引擎
#[derive(Clone)]
pub struct ConsolidationEngine {
    store: Arc<dyn MemoryHubStore>,
    duplicate_detector: DuplicateDetector,
    conflict_detector: ConflictDetector,
    staleness_detector: StalenessDetector,
    merger: Arc<dyn ContentMerger>,
    policy: AutoPolicy,
    max_proposals: usize,
    /// 合并后重算幸存者向量用；未配置时合并会清空幸存者向量而不是留下与正文不符的旧向量
    embedder: Option<Arc<dyn Embedder>>,
}

impl ConsolidationEngine {
    pub fn new(store: Arc<dyn MemoryHubStore>, sim_threshold: f32, max_proposals: usize) -> Self {
        Self {
            store,
            duplicate_detector: DuplicateDetector::new(sim_threshold),
            conflict_detector: ConflictDetector::default(),
            staleness_detector: StalenessDetector::default(),
            merger: Arc::new(TieredMerger::default()),
            policy: AutoPolicy::default(),
            max_proposals: max_proposals.max(1),
            embedder: None,
        }
    }

    /// 注入嵌入器：合并后重算幸存者向量
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    pub fn with_policy(mut self, policy: AutoPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_merger(mut self, merger: Arc<dyn ContentMerger>) -> Self {
        self.merger = merger;
        self
    }

    /// 切换底层存储（例如切换为特定请求的账户隔离存储 AccountScopedStore）
    pub fn with_store(&self, store: Arc<dyn MemoryHubStore>) -> Self {
        let mut cloned = self.clone();
        cloned.store = store;
        cloned
    }

    /// 计算命名空间的记忆整合健康度统计报告
    pub async fn calculate_stats(&self, namespace: &str) -> Result<ConsolidationStats> {
        let entries = self
            .store
            .all_in_namespace(namespace, false)
            .await?
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .collect::<Vec<_>>();

        let total_active = entries.len();
        if total_active == 0 {
            return Ok(ConsolidationStats {
                namespace: namespace.to_string(),
                health_score: 1.0,
                ..Default::default()
            });
        }

        let now = Utc::now();
        let duplicates = self.duplicate_detector.detect_in_entries(&entries);
        let conflicts = self.conflict_detector.detect_in_entries(&entries);
        let stale = self.staleness_detector.detect_in_entries(&entries, now);

        let mut dup_entries = HashSet::new();
        for d in &duplicates {
            for id in &d.member_ids {
                dup_entries.insert(*id);
            }
        }

        let mut conf_entries = HashSet::new();
        for c in &conflicts {
            for id in &c.member_ids {
                conf_entries.insert(*id);
            }
        }

        let duplicate_entries_count = dup_entries.len();
        let conflict_entries_count = conf_entries.len();
        let stale_entries_count = stale.len();

        let redundancy_ratio =
            (duplicate_entries_count as f32 / total_active as f32).clamp(0.0, 1.0);
        let conflict_ratio = (conflict_entries_count as f32 / total_active as f32).clamp(0.0, 1.0);

        // 健康度：1.0 减去冗余惩罚、冲突惩罚和陈旧惩罚
        let health_score = (1.0
            - (0.45 * redundancy_ratio
                + 0.40 * conflict_ratio
                + 0.15 * (stale_entries_count as f32 / total_active as f32)))
            .clamp(0.0, 1.0);

        Ok(ConsolidationStats {
            namespace: namespace.to_string(),
            total_active,
            duplicate_entries_count,
            duplicate_groups_count: duplicates.len(),
            conflict_entries_count,
            conflict_groups_count: conflicts.len(),
            stale_entries_count,
            redundancy_ratio,
            conflict_ratio,
            health_score,
        })
    }

    /// 执行一键整合扫描与提案建立（根据策略处理自动合并与提案持久化）
    pub async fn consolidate_namespace(
        &self,
        namespace: &str,
        dry_run: bool,
        sim_threshold: Option<f32>,
        operator_agent: &AgentId,
    ) -> Result<ConsolidationReport> {
        let entries = self
            .store
            .all_in_namespace(namespace, false)
            .await?
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .collect::<Vec<_>>();

        let stats = self.calculate_stats(namespace).await?;
        let mut report = ConsolidationReport {
            namespace: namespace.to_string(),
            scanned: entries.len(),
            dry_run,
            proposals_created: 0,
            skipped_existing: 0,
            auto_merged: 0,
            proposals: Vec::new(),
            stats,
        };

        if entries.len() < 2 {
            return Ok(report);
        }

        // 1. 识别冲突组
        let conflicts = self.conflict_detector.detect_in_entries(&entries);
        let mut conflict_members: HashSet<Uuid> = HashSet::new();
        let mut new_proposals = Vec::new();

        for c in conflicts {
            for id in &c.member_ids {
                conflict_members.insert(*id);
            }
            let mut ids = c.member_ids.clone();
            ids.sort(); // 规范化有序
            new_proposals.push(IntegrationProposal {
                id: Uuid::new_v4(),
                namespace: namespace.to_string(),
                kind: IntegrationKind::Conflict,
                member_ids: ids,
                suggested_survivor: Some(c.suggested_survivor),
                similarity: 0.0,
                status: ProposalStatus::Open,
                resolution_note: Some(c.description),
                created_at: Utc::now(),
                resolved_at: None,
            });
        }

        // 2. 识别重复组（剔除已被包含进冲突的成员，避免同一个条目双重提案）
        let detector = if let Some(thresh) = sim_threshold {
            DuplicateDetector::new(thresh)
        } else {
            self.duplicate_detector.clone()
        };

        let duplicates = detector.detect_in_entries(&entries);
        for d in duplicates {
            let filtered_members: Vec<Uuid> = d
                .member_ids
                .into_iter()
                .filter(|id| !conflict_members.contains(id))
                .collect();

            if filtered_members.len() < 2 {
                continue;
            }

            // 检查自动合并策略
            if !dry_run
                && self
                    .policy
                    .should_auto_merge(d.max_similarity, filtered_members.len())
                && self
                    .execute_merge(
                        namespace,
                        d.suggested_survivor,
                        &filtered_members,
                        Some("整合中枢自动合并策略触发"),
                        operator_agent,
                    )
                    .await
                    .is_ok()
            {
                report.auto_merged += 1;
                continue;
            }

            let mut ids = filtered_members;
            ids.sort();
            new_proposals.push(IntegrationProposal {
                id: Uuid::new_v4(),
                namespace: namespace.to_string(),
                kind: IntegrationKind::Duplicate,
                member_ids: ids,
                suggested_survivor: Some(d.suggested_survivor),
                similarity: d.max_similarity,
                status: ProposalStatus::Open,
                resolution_note: None,
                created_at: Utc::now(),
                resolved_at: None,
            });
        }

        new_proposals.truncate(self.max_proposals);

        if dry_run {
            report.proposals = new_proposals;
            return Ok(report);
        }

        // 落库前调用 open_exists 进行幂等查重
        for proposal in new_proposals {
            let exists = self
                .store
                .open_exists(
                    namespace,
                    &[(
                        proposal.kind.as_str().to_string(),
                        proposal.member_ids.clone(),
                    )],
                )
                .await?;
            if exists {
                report.skipped_existing += 1;
                continue;
            }
            self.store.save_proposal(&proposal).await?;
            report.proposals_created += 1;
            report.proposals.push(proposal);
        }

        // 重新刷新统计
        report.stats = self
            .calculate_stats(namespace)
            .await
            .unwrap_or(report.stats);
        Ok(report)
    }

    /// 裁决整合提案
    pub async fn resolve_proposal(
        &self,
        req: ResolveRequest,
        auth_agent: &AgentId,
    ) -> Result<MemoryEntry> {
        let mut proposal = self.store.get_proposal(req.proposal_id).await?;
        if proposal.status != ProposalStatus::Open {
            return Err(HubError::Conflict);
        }

        let now = Utc::now();
        match req.action {
            ResolveAction::Dismiss => {
                proposal.status = ProposalStatus::Dismissed;
                proposal.resolution_note = req.note.clone();
                proposal.resolved_at = Some(now);
                self.store.save_proposal(&proposal).await?;

                let first_id = proposal.member_ids.first().ok_or(HubError::NotFound)?;
                self.store.get(&proposal.namespace, *first_id).await
            }
            ResolveAction::Merge => {
                let survivor_id =
                    req.survivor_id
                        .or(proposal.suggested_survivor)
                        .ok_or_else(|| {
                            HubError::InvalidArgument(
                                "提案无建议幸存者，必须显式提供 survivor_id".to_string(),
                            )
                        })?;

                if !proposal.member_ids.contains(&survivor_id) {
                    return Err(HubError::InvalidArgument(
                        "survivor_id 必须是提案成员之一".to_string(),
                    ));
                }

                let survivor = self
                    .execute_merge(
                        &proposal.namespace,
                        survivor_id,
                        &proposal.member_ids,
                        req.note.as_deref(),
                        auth_agent,
                    )
                    .await?;

                proposal.status = ProposalStatus::Merged;
                proposal.resolution_note = req.note;
                proposal.resolved_at = Some(now);
                self.store.save_proposal(&proposal).await?;

                Ok(survivor)
            }
        }
    }

    /// 核心执行合并动作
    async fn execute_merge(
        &self,
        namespace: &str,
        survivor_id: Uuid,
        member_ids: &[Uuid],
        _note: Option<&str>,
        operator_agent: &AgentId,
    ) -> Result<MemoryEntry> {
        let mut members = Vec::new();
        for id in member_ids {
            if let Ok(e) = self.store.get(namespace, *id).await {
                if e.status == MemoryStatus::Active {
                    members.push(e);
                }
            }
        }

        let survivor_entry = members
            .iter()
            .find(|m| m.id == survivor_id)
            .cloned()
            .ok_or(HubError::NotFound)?;

        let now = Utc::now();
        let merged_output = self.merger.merge(&survivor_entry, &members).await?;

        // 1. 更新幸存者
        let mut updated_survivor = survivor_entry;
        updated_survivor.content = merged_output.content;
        updated_survivor.tags = merged_output.tags;
        updated_survivor.metadata = merged_output.metadata;
        updated_survivor.confidence = merged_output.confidence;

        // 融合后正文已变，幸存者身上的旧向量不再代表新内容：留着它会污染
        // 检索的相似度维度，并让下一轮扫描把同一批条目继续判成重复。
        // 配了嵌入器就重算，没配则清空（宁可少一路召回，也不留错的向量）。
        match &self.embedder {
            Some(em) => match em.embed(&[updated_survivor.content.clone()]).await {
                Ok(mut vectors) if !vectors.is_empty() => {
                    let mut v = vectors.remove(0);
                    mem_core::l2_normalize(&mut v);
                    updated_survivor.embedding = Some(v);
                    updated_survivor.embedding_model = Some(em.model_id().to_string());
                }
                other => {
                    tracing::warn!(
                        memory_id = %updated_survivor.id,
                        embedding_empty = other.is_ok(),
                        "合并后重嵌入失败，已清空该条目向量（下次写入时重建）"
                    );
                    updated_survivor.embedding = None;
                }
            },
            None => {
                updated_survivor.embedding = None;
            }
        }

        let expected_survivor = updated_survivor.version;
        updated_survivor.version += 1;
        updated_survivor.updated_at = now;
        updated_survivor.source_agent = operator_agent.clone();

        let saved = self
            .store
            .revise(&updated_survivor, expected_survivor, VersionChange::Updated)
            .await?;

        // 2. 归并其余成员
        for m in members.into_iter().filter(|m| m.id != survivor_id) {
            let mut archived = m;
            archived.status = MemoryStatus::Merged;
            archived.merged_into = Some(survivor_id);
            let expected_member = archived.version;
            archived.version += 1;
            archived.updated_at = now;
            archived.source_agent = operator_agent.clone();

            // 若尚未标记失效时间，将其有效截止时间限制为合并裁决时间，支撑时间有效性过滤
            if archived.valid_until.is_none() {
                archived.valid_until = Some(now);
            }

            // 建立事实演化关系 (survivor supersedes archived)
            let _ = self
                .store
                .add_relation(
                    namespace,
                    &mem_core::NewMemoryRelation {
                        from_memory_id: survivor_id,
                        to_memory_id: archived.id,
                        relation_kind: mem_core::MemoryRelationKind::Supersedes,
                        confidence: 1.0,
                        source_evidence_id: None,
                    },
                )
                .await;

            self.store
                .revise(&archived, expected_member, VersionChange::Merged)
                .await?;
        }

        Ok(saved)
    }
}
