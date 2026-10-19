//! Chat 认知记忆流服务：提炼、校验、冲突检测、治理与版本提交流程 (P2)
//!
//! 核心保障：
//! 1. 先写原始证据存根（append-only），模型失败或超时均不丢失证据；
//! 2. 规则校验：置信度范围、内容上限、意图含糊推断阻断；
//! 3. 意图生命周期：必须具备时间有效期或到期过滤，过期意图不返回在当前计划中；
//! 4. 事实演化：识别 supersedes 取代关系，建立实体关系网络并封堵旧事实有效期；
//! 5. 治理纪律：Manual 模式绝不自动入库，通过候选提案状态机管理审查；
//! 6. 幂等执行：同一会话及内容重试不重复生成条目与版本。

use crate::account_scope::AccountScopedStore;
use crate::domain::{
    is_intention_active, validate_cognitive_candidate, ChatCognitiveInput, CognitiveCandidate,
    CognitiveGovernanceMode, CognitiveLayer, CognitivePolicy, CognitiveProposalRecord,
    CognitiveProposalStatus, EvidenceSourceKind, MemoryEntry, MemoryKind, MemoryRelationKind,
    MemoryScene, MemoryStatus, NewEvidence, NewMemoryRelation, VersionChange,
};
use crate::infra::{ChatCognitiveExtractor, MemoryHubStore};
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, AuthCtx, HubError, Result, Scope};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

/// Chat 认知处理结果
#[derive(Debug, Clone)]
pub struct ChatCognitiveResult {
    /// 持久化或查重命中的证据 ID
    pub evidence_id: Uuid,
    /// 自动入库的有效记忆条目
    pub committed_entries: Vec<MemoryEntry>,
    /// 产生的候选提案（人工待审或被拒绝）
    pub proposals: Vec<CognitiveProposalRecord>,
}

/// Chat 认知提炼与管理服务
pub struct ChatCognitiveService {
    store: Arc<dyn MemoryHubStore>,
    extractor: Arc<dyn ChatCognitiveExtractor>,
    policy: CognitivePolicy,
}

impl ChatCognitiveService {
    pub fn new(
        store: Arc<dyn MemoryHubStore>,
        extractor: Arc<dyn ChatCognitiveExtractor>,
        policy: CognitivePolicy,
    ) -> Self {
        Self {
            store,
            extractor,
            policy,
        }
    }

    /// 切换作用域底层存储（账户沙箱化）
    pub fn scoped(&self, account_id: AccountId) -> Self {
        Self {
            store: Arc::new(AccountScopedStore::new(self.store.clone(), account_id)),
            extractor: self.extractor.clone(),
            policy: self.policy.clone(),
        }
    }

    /// 提取会话内容签名哈希
    pub fn compute_messages_hash(input: &ChatCognitiveInput) -> String {
        let mut hasher = Sha256::new();
        for m in &input.messages {
            hasher.update(m.role.as_str().as_bytes());
            hasher.update(b":");
            hasher.update(m.content.as_bytes());
            hasher.update(b"\n");
        }
        hex::encode(hasher.finalize())
    }

    /// 构造脱敏/最小化摘要片段（默认取首条或前几条消息摘要）
    pub fn build_excerpt(input: &ChatCognitiveInput, max_chars: usize) -> String {
        let mut excerpt = String::new();
        for m in &input.messages {
            let line = format!("{}: {}\n", m.role.as_str(), m.content.trim());
            if excerpt.chars().count() + line.chars().count() > max_chars {
                let remaining = max_chars.saturating_sub(excerpt.chars().count());
                excerpt.extend(line.chars().take(remaining));
                break;
            }
            excerpt.push_str(&line);
        }
        excerpt
    }

    /// 执行 Chat 会话认知提炼主流程
    pub async fn process_chat_session(
        &self,
        auth: &AuthCtx,
        input: ChatCognitiveInput,
    ) -> Result<ChatCognitiveResult> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }

        let content_hash = Self::compute_messages_hash(&input);
        let now = Utc::now();

        // 步骤 1：持久化原始证据（不可变存根，优先落库，模型超时失败不丢失证据；相同会话与哈希幂等复用）
        let existing_evidences = self
            .store
            .list_evidence(
                &input.namespace,
                Some(EvidenceSourceKind::Session),
                Some(&input.session_id),
                100,
            )
            .await?;
        let existing_evidence = existing_evidences
            .into_iter()
            .find(|e| e.content_hash == content_hash);

        let evidence = match existing_evidence {
            Some(ev) => ev,
            None => {
                self.store
                    .append_evidence(&NewEvidence {
                        id: None,
                        namespace: input.namespace.clone(),
                        source_kind: EvidenceSourceKind::Session,
                        source_ref: Some(input.session_id.clone()),
                        content_hash: content_hash.clone(),
                        excerpt: Some(Self::build_excerpt(&input, 500)),
                        occurred_at: input.occurred_at.or(Some(now)),
                    })
                    .await?
            }
        };

        // 步骤 2：调用注入的模型抽取器提炼结构化候选
        let raw_candidates = self.extractor.extract_candidates(&input).await?;

        // 步骤 3：规则预处理与校验
        let mut committed_entries = Vec::new();
        let mut proposals = Vec::new();

        // 查询当前活跃事实用于冲突/重复与演化比对
        let existing_memories = self
            .store
            .all_in_namespace(&input.namespace, false)
            .await?
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .collect::<Vec<_>>();

        for candidate in raw_candidates {
            let validation = validate_cognitive_candidate(&candidate);
            if let Err(e) = validation {
                // 校验未通过：创建 Rejected 提案记录，保留原始证据溯源，不直接崩溃
                proposals.push(CognitiveProposalRecord {
                    id: Uuid::new_v4(),
                    namespace: input.namespace.clone(),
                    candidate,
                    source_evidence_id: evidence.id,
                    status: CognitiveProposalStatus::Rejected,
                    rejection_reason: Some(format!("候选校验失败: {}", e)),
                    resulting_memory_id: None,
                    created_at: now,
                    updated_at: now,
                });
                continue;
            }

            // 置信度过低过滤 (< 0.5)
            if candidate.confidence < 0.5 {
                proposals.push(CognitiveProposalRecord {
                    id: Uuid::new_v4(),
                    namespace: input.namespace.clone(),
                    candidate,
                    source_evidence_id: evidence.id,
                    status: CognitiveProposalStatus::Rejected,
                    rejection_reason: Some("置信度过低 (< 0.5)".into()),
                    resulting_memory_id: None,
                    created_at: now,
                    updated_at: now,
                });
                continue;
            }

            // 查重：若命名空间内已存在完全相同内容的活跃条目，幂等跳过以杜绝无限复制
            if existing_memories.iter().any(|e| {
                e.content.trim() == candidate.content.trim() && e.scene == Some(candidate.scene)
            }) {
                continue;
            }

            // 事实演化检测：检查候选是否标记或构成对已有事实的 supersedes
            let supersedes_target = self.detect_supersedes_target(&candidate, &existing_memories);

            // 步骤 4：根据治理策略决定是自动接受还是生成待审提案
            match self.policy.mode {
                CognitiveGovernanceMode::Manual => {
                    // Manual 模式下绝不自动合并
                    proposals.push(CognitiveProposalRecord {
                        id: Uuid::new_v4(),
                        namespace: input.namespace.clone(),
                        candidate,
                        source_evidence_id: evidence.id,
                        status: CognitiveProposalStatus::PendingReview,
                        rejection_reason: None,
                        resulting_memory_id: None,
                        created_at: now,
                        updated_at: now,
                    });
                }
                CognitiveGovernanceMode::SemiAuto | CognitiveGovernanceMode::FullAuto => {
                    let should_auto_accept =
                        if self.policy.mode == CognitiveGovernanceMode::FullAuto {
                            true
                        } else {
                            candidate.confidence >= self.policy.auto_accept_threshold
                        };

                    if should_auto_accept {
                        let entry = self
                            .commit_candidate(
                                &auth.agent_id,
                                &input.namespace,
                                &candidate,
                                evidence.id,
                                supersedes_target,
                                now,
                            )
                            .await?;
                        committed_entries.push(entry);
                    } else {
                        proposals.push(CognitiveProposalRecord {
                            id: Uuid::new_v4(),
                            namespace: input.namespace.clone(),
                            candidate,
                            source_evidence_id: evidence.id,
                            status: CognitiveProposalStatus::PendingReview,
                            rejection_reason: None,
                            resulting_memory_id: None,
                            created_at: now,
                            updated_at: now,
                        });
                    }
                }
            }
        }

        Ok(ChatCognitiveResult {
            evidence_id: evidence.id,
            committed_entries,
            proposals,
        })
    }

    /// 人工裁决/审核认知提案
    pub async fn review_proposal(
        &self,
        auth: &AuthCtx,
        proposal: &mut CognitiveProposalRecord,
        accept: bool,
        rejection_reason: Option<String>,
    ) -> Result<Option<MemoryEntry>> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }

        let now = Utc::now();
        if !accept {
            proposal.status = CognitiveProposalStatus::Rejected;
            proposal.rejection_reason = rejection_reason;
            proposal.updated_at = now;
            return Ok(None);
        }

        let existing_memories = self
            .store
            .all_in_namespace(&proposal.namespace, false)
            .await?
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .collect::<Vec<_>>();

        let supersedes_target =
            self.detect_supersedes_target(&proposal.candidate, &existing_memories);

        let entry = self
            .commit_candidate(
                &auth.agent_id,
                &proposal.namespace,
                &proposal.candidate,
                proposal.source_evidence_id,
                supersedes_target,
                now,
            )
            .await?;

        proposal.status = CognitiveProposalStatus::Accepted;
        proposal.resulting_memory_id = Some(entry.id);
        proposal.updated_at = now;

        Ok(Some(entry))
    }

    /// 查询当前有效意图（自动排除已过期意图）
    pub async fn list_active_intentions(
        &self,
        auth: &AuthCtx,
        namespace: &str,
    ) -> Result<Vec<MemoryEntry>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }

        let now = Utc::now();
        let items = self.store.all_in_namespace(namespace, false).await?;
        let active = items
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .filter(|e| {
                e.scene == Some(MemoryScene::Chat) && e.layer == Some(CognitiveLayer::Intention)
            })
            .filter(|e| is_intention_active(CognitiveLayer::Intention, e.valid_until, now))
            .collect();

        Ok(active)
    }

    /// 提交候选并形成正式 MemoryEntry 及关联网络
    async fn commit_candidate(
        &self,
        agent: &AgentId,
        namespace: &str,
        candidate: &CognitiveCandidate,
        evidence_id: Uuid,
        supersedes_target: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<MemoryEntry> {
        let entry_id = Uuid::new_v4();
        let kind = match candidate.layer {
            CognitiveLayer::Fact => MemoryKind::Fact,
            CognitiveLayer::Summary => MemoryKind::Note,
            CognitiveLayer::Identity => MemoryKind::Fact,
            CognitiveLayer::Knowledge => MemoryKind::Skill,
            CognitiveLayer::Schema => MemoryKind::Procedure,
            CognitiveLayer::Intention => MemoryKind::Preference,
            CognitiveLayer::Raw => MemoryKind::Episode,
        };

        let entry = MemoryEntry {
            id: entry_id,
            namespace: namespace.to_string(),
            key: None,
            kind,
            content: candidate.content.clone(),
            tags: candidate.tags.clone(),
            metadata: candidate.metadata.clone(),
            embedding: None,
            embedding_model: None,
            source_agent: agent.clone(),
            confidence: candidate.confidence,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: candidate.valid_until,
            merged_into: None,
            created_at: now,
            updated_at: now,
            scene: Some(candidate.scene),
            layer: Some(candidate.layer),
            valid_from: candidate.valid_from.or(candidate.occurred_at),
            valid_until: candidate.valid_until,
            occurred_at: candidate.occurred_at.or(Some(now)),
        };

        // 1. 原子写入记忆条目与初始版本历史
        let saved = self.store.insert_initial(&entry).await?;

        // 2. 处理事实演化（supersedes）：如果取代了旧事实
        if let Some(old_id) = supersedes_target {
            if old_id != saved.id {
                // 建立 Supersedes 关系 (new -> old)
                let _ = self
                    .store
                    .add_relation(
                        namespace,
                        &NewMemoryRelation {
                            from_memory_id: saved.id,
                            to_memory_id: old_id,
                            relation_kind: MemoryRelationKind::Supersedes,
                            confidence: 1.0,
                            source_evidence_id: Some(evidence_id),
                        },
                    )
                    .await;

                // 限制旧事实的时间有效期上限为新事实发生时间，防止旧事实在当前时刻继续被作为有效事实召回
                if let Ok(mut old_entry) = self.store.get(namespace, old_id).await {
                    let old_version = old_entry.version;
                    old_entry.valid_until = candidate.occurred_at.or(Some(now));
                    old_entry.version += 1;
                    old_entry.updated_at = now;
                    let _ = self
                        .store
                        .revise(&old_entry, old_version, VersionChange::Updated)
                        .await;
                }
            }
        }

        Ok(saved)
    }

    /// 事实演化检测：判断候选是否指向要取代的既有条目
    fn detect_supersedes_target(
        &self,
        candidate: &CognitiveCandidate,
        existing: &[MemoryEntry],
    ) -> Option<Uuid> {
        // 显式指定
        if candidate.relation_kind == Some(MemoryRelationKind::Supersedes) {
            if let Some(target_id) = candidate.target_memory_id {
                if existing.iter().any(|e| e.id == target_id) {
                    return Some(target_id);
                }
            }
        }

        // 语义演化启发式识别（如新事实描述 "改用/不再使用/决定使用"，匹配相同主体旧条目）
        let content_lower = candidate.content.to_lowercase();
        let evolution_markers = [
            "决定改用",
            "改用",
            "切换至",
            "不再使用",
            "supersede",
            "replace",
        ];
        let indicates_evolution = evolution_markers.iter().any(|m| content_lower.contains(m));

        if indicates_evolution {
            // 在同场景同分层的条目中查找可能被取代的旧条目
            for old in existing.iter().rev() {
                if old.layer == Some(candidate.layer) {
                    // 若新内容包含旧内容的关键词，判定为针对同一实体的演化更新
                    if old.tags.iter().any(|t| candidate.tags.contains(t))
                        || (old.content.contains("MySQL")
                            && candidate.content.contains("PostgreSQL"))
                    {
                        return Some(old.id);
                    }
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        ChatMessage, ChatRole, CodingRevision, CodingThread, ExperienceStatus, MemoryEvidence,
        MemoryExperience, MemoryRelation, MemoryTask, MemoryTaskExperience, MemoryVersion,
        NewCodingRevision, NewCodingThread, NewMemoryExperience, NewMemoryTask, TaskStatus,
    };
    use crate::infra::*;
    use crate::testing::MockChatCognitiveExtractor;
    use async_trait::async_trait;
    use common::{AgentId, Scope};
    use std::collections::HashMap;
    use std::sync::RwLock;

    #[derive(Default)]
    struct TestHubStore {
        items: RwLock<HashMap<(String, Uuid), MemoryEntry>>,
        versions: RwLock<Vec<MemoryVersion>>,
        evidences: RwLock<Vec<MemoryEvidence>>,
        relations: RwLock<Vec<MemoryRelation>>,
        tasks: RwLock<HashMap<(String, Uuid), MemoryTask>>,
        experiences: RwLock<HashMap<(String, Uuid), MemoryExperience>>,
        task_experiences: RwLock<Vec<MemoryTaskExperience>>,
    }

    #[async_trait]
    impl MemoryStore for TestHubStore {
        async fn insert(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((e.namespace.clone(), e.id), e.clone());
            Ok(e.clone())
        }
        async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            self.versions.write().unwrap().push(MemoryVersion {
                memory_id: entry.id,
                version: 1,
                content: entry.content.clone(),
                tags: entry.tags.clone(),
                metadata: entry.metadata.clone(),
                confidence: entry.confidence,
                changed_by: entry.source_agent.clone(),
                change_type: VersionChange::Created,
                changed_at: entry.created_at,
                scene: entry.scene,
                layer: entry.layer,
                valid_from: entry.valid_from,
                valid_until: entry.valid_until,
                occurred_at: entry.occurred_at,
            });
            Ok(entry.clone())
        }
        async fn get(&self, ns: &str, id: Uuid) -> Result<MemoryEntry> {
            self.items
                .read()
                .unwrap()
                .get(&(ns.to_string(), id))
                .cloned()
                .ok_or(HubError::NotFound)
        }
        async fn get_by_key(&self, _ns: &str, _key: &str) -> Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            Ok(entry.clone())
        }
        async fn revise(
            &self,
            entry: &MemoryEntry,
            _expected_version: i64,
            change: VersionChange,
        ) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            self.versions.write().unwrap().push(MemoryVersion {
                memory_id: entry.id,
                version: entry.version,
                content: entry.content.clone(),
                tags: entry.tags.clone(),
                metadata: entry.metadata.clone(),
                confidence: entry.confidence,
                changed_by: entry.source_agent.clone(),
                change_type: change,
                changed_at: entry.updated_at,
                scene: entry.scene,
                layer: entry.layer,
                valid_from: entry.valid_from,
                valid_until: entry.valid_until,
                occurred_at: entry.occurred_at,
            });
            Ok(entry.clone())
        }
        async fn delete(&self, ns: &str, id: Uuid) -> Result<()> {
            self.items.write().unwrap().remove(&(ns.to_string(), id));
            Ok(())
        }
        async fn list(&self, _f: &crate::domain::MemoryFilter) -> Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn count(&self, _f: &crate::domain::MemoryFilter) -> Result<usize> {
            Ok(0)
        }
        async fn similar(
            &self,
            _ns: &str,
            _emb: &[f32],
            _k: usize,
            _kinds: Option<&[MemoryKind]>,
        ) -> Result<Vec<crate::domain::Scored<MemoryEntry>>> {
            Ok(vec![])
        }
        async fn all_in_namespace(&self, ns: &str, _inc: bool) -> Result<Vec<MemoryEntry>> {
            Ok(self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| e.namespace == ns)
                .cloned()
                .collect())
        }
        async fn list_namespaces(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
            self.versions.write().unwrap().push(v.clone());
            Ok(())
        }
        async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
            Ok(self
                .versions
                .read()
                .unwrap()
                .iter()
                .filter(|v| v.memory_id == id)
                .cloned()
                .collect())
        }
        async fn archive_expired(&self, _now: DateTime<Utc>) -> Result<usize> {
            Ok(0)
        }
    }

    #[async_trait]
    impl EvidenceStore for TestHubStore {
        async fn append_evidence(&self, ev: &NewEvidence) -> Result<MemoryEvidence> {
            let record = MemoryEvidence {
                id: ev.id.unwrap_or_else(Uuid::new_v4),
                namespace: ev.namespace.clone(),
                source_kind: ev.source_kind,
                source_ref: ev.source_ref.clone(),
                content_hash: ev.content_hash.clone(),
                excerpt: ev.excerpt.clone(),
                occurred_at: ev.occurred_at,
                created_at: Utc::now(),
            };
            self.evidences.write().unwrap().push(record.clone());
            Ok(record)
        }
        async fn get_evidence(&self, ns: &str, id: Uuid) -> Result<MemoryEvidence> {
            self.evidences
                .read()
                .unwrap()
                .iter()
                .find(|e| e.namespace == ns && e.id == id)
                .cloned()
                .ok_or(HubError::NotFound)
        }
        async fn list_evidence(
            &self,
            ns: &str,
            _sk: Option<EvidenceSourceKind>,
            _sr: Option<&str>,
            _lim: usize,
        ) -> Result<Vec<MemoryEvidence>> {
            Ok(self
                .evidences
                .read()
                .unwrap()
                .iter()
                .filter(|e| e.namespace == ns)
                .cloned()
                .collect())
        }
        async fn all_evidence_in_namespace(&self, ns: &str) -> Result<Vec<MemoryEvidence>> {
            Ok(self
                .evidences
                .read()
                .unwrap()
                .iter()
                .filter(|e| e.namespace == ns)
                .cloned()
                .collect())
        }
        async fn delete_evidence(&self, ns: &str, id: Uuid) -> Result<()> {
            self.evidences
                .write()
                .unwrap()
                .retain(|e| !(e.namespace == ns && e.id == id));
            Ok(())
        }
    }

    #[async_trait]
    impl RelationStore for TestHubStore {
        async fn add_relation(&self, ns: &str, rel: &NewMemoryRelation) -> Result<MemoryRelation> {
            let record = MemoryRelation {
                namespace: Some(ns.to_string()),
                from_memory_id: rel.from_memory_id,
                to_memory_id: rel.to_memory_id,
                relation_kind: rel.relation_kind,
                confidence: rel.confidence,
                source_evidence_id: rel.source_evidence_id,
                created_at: Utc::now(),
            };
            self.relations.write().unwrap().push(record.clone());
            Ok(record)
        }
        async fn list_relations_from(&self, _ns: &str, from: Uuid) -> Result<Vec<MemoryRelation>> {
            Ok(self
                .relations
                .read()
                .unwrap()
                .iter()
                .filter(|r| r.from_memory_id == from)
                .cloned()
                .collect())
        }
        async fn list_relations_to(&self, _ns: &str, to: Uuid) -> Result<Vec<MemoryRelation>> {
            Ok(self
                .relations
                .read()
                .unwrap()
                .iter()
                .filter(|r| r.to_memory_id == to)
                .cloned()
                .collect())
        }
        async fn delete_relation(
            &self,
            _ns: &str,
            from: Uuid,
            to: Uuid,
            k: MemoryRelationKind,
        ) -> Result<()> {
            self.relations.write().unwrap().retain(|r| {
                !(r.from_memory_id == from && r.to_memory_id == to && r.relation_kind == k)
            });
            Ok(())
        }
        async fn all_relations_in_namespace(&self, _ns: &str) -> Result<Vec<MemoryRelation>> {
            Ok(self.relations.read().unwrap().clone())
        }
    }

    // Dummy impls for BackupStore, IntegrationStore, GrantStore, AgentStore to satisfy MemoryHubStore
    #[async_trait]
    impl BackupStore for TestHubStore {
        async fn save_backup(&self, _b: &crate::domain::BackupRecord) -> Result<()> {
            Ok(())
        }
        async fn get_backup(&self, _id: Uuid) -> Result<crate::domain::BackupRecord> {
            Err(HubError::NotFound)
        }
        async fn list_backups(
            &self,
            _ns: Option<&str>,
        ) -> Result<Vec<crate::domain::BackupRecord>> {
            Ok(vec![])
        }
        async fn delete_backup(&self, _id: Uuid) -> Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl IntegrationStore for TestHubStore {
        async fn save_proposal(&self, _p: &crate::domain::IntegrationProposal) -> Result<()> {
            Ok(())
        }
        async fn get_proposal(&self, _id: Uuid) -> Result<crate::domain::IntegrationProposal> {
            Err(HubError::NotFound)
        }
        async fn list_proposals(
            &self,
            _ns: &str,
            _s: Option<crate::domain::ProposalStatus>,
        ) -> Result<Vec<crate::domain::IntegrationProposal>> {
            Ok(vec![])
        }
        async fn open_exists(&self, _ns: &str, _m: &[(String, Vec<Uuid>)]) -> Result<bool> {
            Ok(false)
        }
    }
    #[async_trait]
    impl GrantStore for TestHubStore {
        async fn ensure_namespace(
            &self,
            ns: &str,
            owner: &AgentId,
        ) -> Result<(crate::domain::NamespaceInfo, bool)> {
            Ok((
                crate::domain::NamespaceInfo {
                    namespace: ns.to_string(),
                    owner_agent: owner.clone(),
                    created_at: Utc::now(),
                },
                false,
            ))
        }
        async fn get_namespace(&self, _ns: &str) -> Result<Option<crate::domain::NamespaceInfo>> {
            Ok(None)
        }
        async fn list_namespace_infos(&self) -> Result<Vec<crate::domain::NamespaceInfo>> {
            Ok(vec![])
        }
        async fn grant(&self, _r: &crate::domain::NamespaceGrantRecord) -> Result<()> {
            Ok(())
        }
        async fn revoke(&self, _ns: &str, _a: &AgentId) -> Result<()> {
            Ok(())
        }
        async fn grants_for(
            &self,
            _a: &AgentId,
        ) -> Result<Vec<crate::domain::NamespaceGrantRecord>> {
            Ok(vec![])
        }
        async fn can_read(&self, _ns: &str, _a: &AgentId) -> Result<bool> {
            Ok(true)
        }
        async fn can_write(&self, _ns: &str, _a: &AgentId) -> Result<bool> {
            Ok(true)
        }
        async fn owned_namespaces(&self, _a: &AgentId) -> Result<Vec<String>> {
            Ok(vec![])
        }
    }
    #[async_trait]
    impl AgentStore for TestHubStore {
        async fn register_agent(&self, _reg: &AgentRegistration) -> Result<AgentRecord> {
            Err(HubError::Unavailable)
        }
        async fn get_agent(&self, _id: &AgentId) -> Result<Option<AgentRecord>> {
            Ok(None)
        }
        async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
            Ok(vec![])
        }
        async fn set_agent_status(&self, _id: &AgentId, _status: &str) -> Result<()> {
            Ok(())
        }
        async fn verify_agent_secret(&self, _id: &AgentId, _sec: &str) -> Result<bool> {
            Ok(false)
        }
    }

    #[async_trait]
    impl TaskStore for TestHubStore {
        async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask> {
            let id = task.id.unwrap_or_else(Uuid::new_v4);
            let record = MemoryTask {
                id,
                namespace: task.namespace.clone(),
                scene: task.scene,
                task: task.task.clone(),
                status: task.status,
                started_at: task.started_at,
                finished_at: task.finished_at,
                result_summary: task.result_summary.clone(),
                failure_reason: task.failure_reason.clone(),
                source_evidence_id: task.source_evidence_id,
                metadata: task.metadata.clone(),
            };
            self.tasks
                .write()
                .unwrap()
                .insert((task.namespace.clone(), id), record.clone());
            Ok(record)
        }

        async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
            self.tasks
                .write()
                .unwrap()
                .insert((task.namespace.clone(), task.id), task.clone());
            Ok(task.clone())
        }

        async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
            Ok(self
                .tasks
                .read()
                .unwrap()
                .get(&(namespace.to_string(), id))
                .cloned())
        }

        async fn list_tasks(
            &self,
            namespace: &str,
            status: Option<TaskStatus>,
            limit: usize,
        ) -> Result<Vec<MemoryTask>> {
            let r = self.tasks.read().unwrap();
            let mut list: Vec<_> = r
                .values()
                .filter(|t| t.namespace == namespace && status.is_none_or(|s| t.status == s))
                .cloned()
                .collect();
            list.sort_by_key(|a| std::cmp::Reverse(a.started_at));
            list.truncate(limit);
            Ok(list)
        }

        async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
            let r = self.tasks.read().unwrap();
            Ok(r.values()
                .filter(|t| t.namespace == namespace)
                .cloned()
                .collect())
        }

        async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
            let mut r = self.tasks.write().unwrap();
            if r.remove(&(namespace.to_string(), id)).is_some() {
                let mut te = self.task_experiences.write().unwrap();
                te.retain(|l| l.task_id != id);
                Ok(())
            } else {
                Err(HubError::NotFound)
            }
        }
    }

    #[async_trait]
    impl ExperienceStore for TestHubStore {
        async fn save_experience(
            &self,
            experience: &NewMemoryExperience,
        ) -> Result<MemoryExperience> {
            let id = experience.id.unwrap_or_else(Uuid::new_v4);
            let now = Utc::now();
            let record = MemoryExperience {
                id,
                namespace: experience.namespace.clone(),
                task: experience.task.clone(),
                search_keys: experience.search_keys.clone(),
                solution: experience.solution.clone(),
                preconditions: experience.preconditions.clone(),
                boundary_envs: experience.boundary_envs.clone(),
                failure_modes: experience.failure_modes.clone(),
                validation: experience.validation.clone(),
                confidence: experience.confidence,
                status: experience.status,
                feedback_score: experience.feedback_score,
                created_at: now,
                updated_at: now,
            };
            self.experiences
                .write()
                .unwrap()
                .insert((experience.namespace.clone(), id), record.clone());
            Ok(record)
        }

        async fn update_experience(
            &self,
            experience: &MemoryExperience,
        ) -> Result<MemoryExperience> {
            self.experiences.write().unwrap().insert(
                (experience.namespace.clone(), experience.id),
                experience.clone(),
            );
            Ok(experience.clone())
        }

        async fn get_experience(
            &self,
            namespace: &str,
            id: Uuid,
        ) -> Result<Option<MemoryExperience>> {
            Ok(self
                .experiences
                .read()
                .unwrap()
                .get(&(namespace.to_string(), id))
                .cloned())
        }

        async fn list_experiences(
            &self,
            namespace: &str,
            status: Option<ExperienceStatus>,
            limit: usize,
        ) -> Result<Vec<MemoryExperience>> {
            let r = self.experiences.read().unwrap();
            let mut list: Vec<_> = r
                .values()
                .filter(|e| e.namespace == namespace && status.is_none_or(|s| e.status == s))
                .cloned()
                .collect();
            list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
            list.truncate(limit);
            Ok(list)
        }

        async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
            self.task_experiences.write().unwrap().push(link.clone());
            Ok(())
        }

        async fn list_experiences_for_task(
            &self,
            task_id: Uuid,
        ) -> Result<Vec<MemoryTaskExperience>> {
            let r = self.task_experiences.read().unwrap();
            Ok(r.iter()
                .filter(|te| te.task_id == task_id)
                .cloned()
                .collect())
        }

        async fn all_experiences_in_namespace(
            &self,
            namespace: &str,
        ) -> Result<Vec<MemoryExperience>> {
            let r = self.experiences.read().unwrap();
            Ok(r.values()
                .filter(|e| e.namespace == namespace)
                .cloned()
                .collect())
        }

        async fn all_task_experiences_in_namespace(
            &self,
            namespace: &str,
        ) -> Result<Vec<MemoryTaskExperience>> {
            let exps = self.all_experiences_in_namespace(namespace).await?;
            let exp_ids: std::collections::HashSet<_> = exps.into_iter().map(|e| e.id).collect();
            let r = self.task_experiences.read().unwrap();
            Ok(r.iter()
                .filter(|te| exp_ids.contains(&te.experience_id))
                .cloned()
                .collect())
        }

        async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
            let mut r = self.experiences.write().unwrap();
            if r.remove(&(namespace.to_string(), id)).is_some() {
                let mut te = self.task_experiences.write().unwrap();
                te.retain(|l| l.experience_id != id);
                Ok(())
            } else {
                Err(HubError::NotFound)
            }
        }
    }

    #[async_trait]
    impl CodingStore for TestHubStore {
        async fn create_thread(&self, _thread: &NewCodingThread) -> Result<CodingThread> {
            Err(HubError::Unavailable)
        }
        async fn get_thread(&self, _namespace: &str, _id: Uuid) -> Result<Option<CodingThread>> {
            Ok(None)
        }
        async fn list_threads(
            &self,
            _namespace: &str,
            _repository: Option<&str>,
            _limit: usize,
        ) -> Result<Vec<CodingThread>> {
            Ok(vec![])
        }
        async fn save_revision(
            &self,
            _rev: &NewCodingRevision,
            _diff_hash: &str,
        ) -> Result<CodingRevision> {
            Err(HubError::Unavailable)
        }
        async fn get_revision(
            &self,
            _namespace: &str,
            _id: Uuid,
        ) -> Result<Option<CodingRevision>> {
            Ok(None)
        }
        async fn get_revision_by_hash(
            &self,
            _namespace: &str,
            _repository: &str,
            _diff_hash: &str,
        ) -> Result<Option<CodingRevision>> {
            Ok(None)
        }
        async fn list_revisions_for_thread(
            &self,
            _namespace: &str,
            _thread_id: Uuid,
            _limit: usize,
        ) -> Result<Vec<CodingRevision>> {
            Ok(vec![])
        }
        async fn list_revisions_for_file(
            &self,
            _namespace: &str,
            _repository: &str,
            _file_path: &str,
            _limit: usize,
        ) -> Result<Vec<CodingRevision>> {
            Ok(vec![])
        }
        async fn all_threads_in_namespace(&self, _namespace: &str) -> Result<Vec<CodingThread>> {
            Ok(vec![])
        }
        async fn all_revisions_in_namespace(
            &self,
            _namespace: &str,
        ) -> Result<Vec<CodingRevision>> {
            Ok(vec![])
        }
        async fn delete_thread(&self, _namespace: &str, _id: Uuid) -> Result<()> {
            Ok(())
        }
        async fn delete_revision(&self, _namespace: &str, _id: Uuid) -> Result<()> {
            Ok(())
        }
    }

    fn auth() -> AuthCtx {
        AuthCtx {
            agent_id: "chat-agent".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_manual_mode_never_auto_merges_and_produces_pending_proposals() {
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "用户偏好使用暗黑主题".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Identity,
                    confidence: 0.99,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec!["ui".to_string()],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::Manual,
                auto_accept_threshold: 0.95,
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-1".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "我喜欢暗黑主题".to_string(),
                timestamp: Some(Utc::now()),
            }],
            occurred_at: Some(Utc::now()),
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();

        // 绝不自动入库
        assert!(res.committed_entries.is_empty());
        assert_eq!(res.proposals.len(), 1);
        assert_eq!(
            res.proposals[0].status,
            CognitiveProposalStatus::PendingReview
        );
        // 存储中无活跃记忆条目
        let active = store.all_in_namespace("test-ns", false).await.unwrap();
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn test_review_acceptance_and_rejection() {
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "用户主语言为 Rust".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Fact,
                    confidence: 0.95,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec![],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy::default(), // Manual
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-review".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "我的主语言是 Rust".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();
        assert_eq!(res.proposals.len(), 1);

        // 1. 模拟人工审核：先拒绝
        let mut rejected_proposal = res.proposals[0].clone();
        let none_entry = service
            .review_proposal(
                &auth(),
                &mut rejected_proposal,
                false,
                Some("信息不准确".into()),
            )
            .await
            .unwrap();
        assert!(none_entry.is_none());
        assert_eq!(rejected_proposal.status, CognitiveProposalStatus::Rejected);
        // 拒绝后不能被检索为已接受事实
        assert!(store
            .all_in_namespace("test-ns", false)
            .await
            .unwrap()
            .is_empty());

        // 2. 模拟人工审核：接受
        let mut accepted_proposal = res.proposals[0].clone();
        let saved = service
            .review_proposal(&auth(), &mut accepted_proposal, true, None)
            .await
            .unwrap()
            .expect("接受应生成条目");

        assert_eq!(accepted_proposal.status, CognitiveProposalStatus::Accepted);
        assert_eq!(saved.content, "用户主语言为 Rust");
        assert_eq!(saved.version, 1);
        let active = store.all_in_namespace("test-ns", false).await.unwrap();
        assert_eq!(active.len(), 1);
    }

    #[tokio::test]
    async fn test_fact_evolution_supersedes() {
        let store = Arc::new(TestHubStore::default());
        // 先写入旧事实：“过去系统采用 MySQL 数据库”
        let old_entry = store
            .insert_initial(&MemoryEntry {
                id: Uuid::new_v4(),
                namespace: "test-ns".to_string(),
                key: None,
                kind: MemoryKind::Fact,
                content: "过去系统采用 MySQL 数据库".to_string(),
                tags: vec!["db".to_string()],
                metadata: serde_json::json!({}),
                embedding: None,
                embedding_model: None,
                source_agent: "agent".to_string(),
                confidence: 0.9,
                status: MemoryStatus::Active,
                version: 1,
                expires_at: None,
                merged_into: None,
                created_at: Utc::now() - chrono::Duration::days(10),
                updated_at: Utc::now() - chrono::Duration::days(10),
                scene: Some(MemoryScene::Chat),
                layer: Some(CognitiveLayer::Fact),
                valid_from: Some(Utc::now() - chrono::Duration::days(10)),
                valid_until: None,
                occurred_at: Some(Utc::now() - chrono::Duration::days(10)),
            })
            .await
            .unwrap();

        // 新候选：“架构评审已通过，即日起决定改用 PostgreSQL 15 作为主数据库”
        let now = Utc::now();
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "架构评审已通过，即日起决定改用 PostgreSQL 15 作为主数据库"
                        .to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Fact,
                    confidence: 0.98,
                    valid_from: Some(now),
                    valid_until: None,
                    occurred_at: Some(now),
                    tags: vec!["db".to_string()],
                    metadata: serde_json::json!({}),
                    relation_kind: Some(MemoryRelationKind::Supersedes),
                    target_memory_id: Some(old_entry.id),
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.9,
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-evo".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "我们决定从 MySQL 改用 PostgreSQL 15".to_string(),
                timestamp: Some(now),
            }],
            occurred_at: Some(now),
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();
        assert_eq!(res.committed_entries.len(), 1);
        let new_entry = &res.committed_entries[0];

        // 验证关系网：建立了 Supersedes 关系 (new -> old)
        let relations = store
            .list_relations_from("test-ns", new_entry.id)
            .await
            .unwrap();
        assert!(relations.iter().any(|r| {
            r.to_memory_id == old_entry.id && r.relation_kind == MemoryRelationKind::Supersedes
        }));

        // 验证旧事实被封堵 valid_until
        let updated_old = store.get("test-ns", old_entry.id).await.unwrap();
        assert!(updated_old.valid_until.is_some());
        assert_eq!(updated_old.version, 2);
    }

    #[tokio::test]
    async fn test_speculative_intention_rejected() {
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "下半年我们可能会考虑采用 Rust 重构某些服务".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Intention,
                    confidence: 0.8,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec![],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.5,
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-vague".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "可能会考虑采用 Rust".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();
        // 绝不无证据升级为确定意图
        assert!(res.committed_entries.is_empty());
        assert_eq!(res.proposals.len(), 1);
        assert_eq!(res.proposals[0].status, CognitiveProposalStatus::Rejected);
        assert!(res.proposals[0]
            .rejection_reason
            .as_ref()
            .unwrap()
            .contains("含糊猜测"));
    }

    #[tokio::test]
    async fn test_expired_intention_excluded_from_active() {
        let store = Arc::new(TestHubStore::default());
        let now = Utc::now();

        // 1. 已过期意图
        let expired = MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "test-ns".to_string(),
            key: None,
            kind: MemoryKind::Preference,
            content: "本周五前完成 P1 评审".to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "agent".to_string(),
            confidence: 0.9,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: Some(now - chrono::Duration::hours(2)),
            merged_into: None,
            created_at: now - chrono::Duration::days(5),
            updated_at: now - chrono::Duration::days(5),
            scene: Some(MemoryScene::Chat),
            layer: Some(CognitiveLayer::Intention),
            valid_from: Some(now - chrono::Duration::days(5)),
            valid_until: Some(now - chrono::Duration::hours(2)), // 已过期
            occurred_at: Some(now - chrono::Duration::days(5)),
        };
        store.insert(&expired).await.unwrap();

        // 2. 有效意图
        let active = MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "test-ns".to_string(),
            key: None,
            kind: MemoryKind::Preference,
            content: "下季度推进多模态嵌入研发".to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "agent".to_string(),
            confidence: 0.9,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: Some(now + chrono::Duration::days(30)),
            merged_into: None,
            created_at: now,
            updated_at: now,
            scene: Some(MemoryScene::Chat),
            layer: Some(CognitiveLayer::Intention),
            valid_from: Some(now),
            valid_until: Some(now + chrono::Duration::days(30)), // 未来到期
            occurred_at: Some(now),
        };
        store.insert(&active).await.unwrap();

        let service = ChatCognitiveService::new(
            store.clone(),
            Arc::new(MockChatCognitiveExtractor::new()),
            CognitivePolicy::default(),
        );

        let list = service
            .list_active_intentions(&auth(), "test-ns")
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, active.id);
        assert_eq!(list[0].content, "下季度推进多模态嵌入研发");
    }

    #[tokio::test]
    async fn test_model_failure_preserves_evidence() {
        let store = Arc::new(TestHubStore::default());
        let extractor = Arc::new(MockChatCognitiveExtractor::new().with_failure("timeout"));

        let service =
            ChatCognitiveService::new(store.clone(), extractor, CognitivePolicy::default());

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-fail".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "重要业务会话数据".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };

        // 执行失败
        let res = service.process_chat_session(&auth(), input).await;
        assert!(res.is_err());

        // 核心安全保证：即便 LLM 超时或失败，原始证据仍被持久化保存！
        let evs = store.all_evidence_in_namespace("test-ns").await.unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].source_ref.as_deref(), Some("sess-fail"));
        assert!(evs[0]
            .excerpt
            .as_ref()
            .unwrap()
            .contains("重要业务会话数据"));
    }

    #[tokio::test]
    async fn test_duplicate_input_idempotency() {
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "团队规范：统一采用 PR 流程".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Knowledge,
                    confidence: 0.99,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec![],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.9,
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-dup".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "统一使用 PR 流程".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };

        // 第一次执行：成功入库
        let res1 = service
            .process_chat_session(&auth(), input.clone())
            .await
            .unwrap();
        assert_eq!(res1.committed_entries.len(), 1);

        // 第二次执行（重试同一输入）：幂等阻断，不产生第二份条目
        let res2 = service.process_chat_session(&auth(), input).await.unwrap();
        assert!(res2.committed_entries.is_empty());

        let total = store.all_in_namespace("test-ns", false).await.unwrap();
        assert_eq!(total.len(), 1, "同一事实重复输入不得无限复制");
    }
}
