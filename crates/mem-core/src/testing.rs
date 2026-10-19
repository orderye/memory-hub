//! 开发与测试用的确定性嵌入器
//!
//! 与 `embed::MockEmbedder` 相同思路（SHA-256 哈希 → 归一化伪向量），
//! 定义在 core 中以避免 core → embed 的循环依赖；生产环境请使用
//! `embed` crate 的 `HttpEmbedder`。

use crate::infra::Embedder;
use async_trait::async_trait;
use common::Result;
use sha2::{Digest, Sha256};

/// 确定性测试嵌入器
#[derive(Debug, Clone)]
pub struct TestEmbedder {
    dimension: usize,
}

impl Default for TestEmbedder {
    fn default() -> Self {
        Self { dimension: 64 }
    }
}

impl TestEmbedder {
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    fn generate_vector(&self, text: &str) -> Vec<f32> {
        let mut vec = Vec::with_capacity(self.dimension);
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash = hasher.finalize();

        for i in 0..self.dimension {
            let byte = hash[i % 32];
            let val = ((f32::from(byte)) / 255.0) * 2.0 - 1.0 + ((i as f32) * 0.001).sin();
            vec.push(val);
        }

        let norm_sq: f32 = vec.iter().map(|x| x * x).sum();
        let norm = norm_sq.sqrt();
        if norm > 0.0 {
            for x in &mut vec {
                *x /= norm;
            }
        }
        vec
    }
}

#[async_trait]
impl Embedder for TestEmbedder {
    fn model_id(&self) -> &str {
        "test-embedder"
    }

    fn dim(&self) -> usize {
        self.dimension
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.generate_vector(t)).collect())
    }
}

use crate::domain::{ChatCognitiveInput, CognitiveCandidate};
use crate::infra::ChatCognitiveExtractor;
use std::sync::Mutex;

/// 测试用可编程/确定性 Chat 认知抽取器
#[derive(Default)]
pub struct MockChatCognitiveExtractor {
    pub candidates: Mutex<Vec<CognitiveCandidate>>,
    pub failure_mode: Mutex<Option<String>>,
}

impl MockChatCognitiveExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_candidates(self, candidates: Vec<CognitiveCandidate>) -> Self {
        *self.candidates.lock().unwrap() = candidates;
        self
    }

    pub fn with_failure(self, failure_mode: &str) -> Self {
        *self.failure_mode.lock().unwrap() = Some(failure_mode.to_string());
        self
    }
}

#[async_trait]
impl ChatCognitiveExtractor for MockChatCognitiveExtractor {
    async fn extract_candidates(
        &self,
        _input: &ChatCognitiveInput,
    ) -> Result<Vec<CognitiveCandidate>> {
        if let Some(mode) = self.failure_mode.lock().unwrap().as_deref() {
            match mode {
                "timeout" => {
                    return Err(common::HubError::Internal(anyhow::anyhow!(
                        "LLM request timed out"
                    )))
                }
                "invalid_json" => {
                    return Err(common::HubError::InvalidArgument(
                        "LLM response malformed JSON".into(),
                    ))
                }
                _ => {
                    return Err(common::HubError::Internal(anyhow::anyhow!(
                        "LLM provider failure"
                    )))
                }
            }
        }
        let list = self.candidates.lock().unwrap().clone();
        Ok(list)
    }
}

use crate::domain::*;
use crate::infra::*;
use chrono::{DateTime, Utc};
use common::{AgentId, HubError};
use std::collections::HashMap;
use std::sync::RwLock;
use uuid::Uuid;

/// 测试用固定时钟：让时间相关断言不随墙上时钟漂移（消除评测时间炸弹）
#[derive(Debug, Clone)]
pub struct FixedClock {
    now: DateTime<Utc>,
}

impl FixedClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self { now }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }
}

/// 测试用全功能内存存储（实现 MemoryHubStore 全套 trait）
#[derive(Default)]
pub struct TestHubStore {
    pub items: RwLock<HashMap<(String, Uuid), MemoryEntry>>,
    pub versions: RwLock<Vec<MemoryVersion>>,
    pub backups: RwLock<HashMap<Uuid, BackupRecord>>,
    pub namespaces: RwLock<HashMap<String, NamespaceInfo>>,
    pub grants: RwLock<Vec<NamespaceGrantRecord>>,
    pub agents: RwLock<HashMap<AgentId, AgentRecord>>,
    pub proposals: RwLock<HashMap<Uuid, IntegrationProposal>>,
    pub evidences: RwLock<Vec<MemoryEvidence>>,
    pub relations: RwLock<Vec<MemoryRelation>>,
    pub tasks: RwLock<HashMap<(String, Uuid), MemoryTask>>,
    pub experiences: RwLock<HashMap<(String, Uuid), MemoryExperience>>,
    pub task_experiences: RwLock<Vec<MemoryTaskExperience>>,
    pub coding_threads: RwLock<HashMap<(String, Uuid), CodingThread>>,
    pub coding_revisions: RwLock<HashMap<(String, Uuid), CodingRevision>>,
}

impl TestHubStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed(&self, ns: &str, agent: &str, content: &str) -> MemoryEntry {
        let entry = MemoryEntry {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: agent.to_string(),
            confidence: 1.0,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: None,
            merged_into: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            scene: None,
            layer: None,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        };
        self.items
            .write()
            .unwrap()
            .insert((ns.to_string(), entry.id), entry.clone());
        entry
    }
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

    async fn get_by_key(&self, ns: &str, key: &str) -> Result<Option<MemoryEntry>> {
        Ok(self
            .items
            .read()
            .unwrap()
            .values()
            .find(|e| e.namespace == ns && e.key.as_deref() == Some(key))
            .cloned())
    }

    async fn save(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
        self.items
            .write()
            .unwrap()
            .insert((e.namespace.clone(), e.id), e.clone());
        Ok(e.clone())
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

    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
        let mut out: Vec<MemoryEntry> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| {
                filter
                    .namespace
                    .as_deref()
                    .is_none_or(|ns| e.namespace == ns)
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
        Ok(self.list(filter).await?.len())
    }

    async fn similar(
        &self,
        _ns: &str,
        _qv: &[f32],
        _k: usize,
        _kinds: Option<&[MemoryKind]>,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        Ok(vec![])
    }

    async fn all_in_namespace(&self, ns: &str, include_merged: bool) -> Result<Vec<MemoryEntry>> {
        Ok(self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| e.namespace == ns)
            .filter(|e| include_merged || e.status != MemoryStatus::Merged)
            .cloned()
            .collect())
    }

    async fn list_namespaces(&self) -> Result<Vec<String>> {
        let mut ns: Vec<String> = self
            .items
            .read()
            .unwrap()
            .values()
            .map(|e| e.namespace.clone())
            .collect();
        ns.sort();
        ns.dedup();
        Ok(ns)
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
impl BackupStore for TestHubStore {
    async fn save_backup(&self, b: &BackupRecord) -> Result<()> {
        self.backups.write().unwrap().insert(b.id, b.clone());
        Ok(())
    }
    async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
        self.backups
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(HubError::NotFound)
    }
    async fn list_backups(&self, ns: Option<&str>) -> Result<Vec<BackupRecord>> {
        Ok(self
            .backups
            .read()
            .unwrap()
            .values()
            .filter(|b| ns.is_none_or(|n| b.namespace == n))
            .cloned()
            .collect())
    }
    async fn delete_backup(&self, id: Uuid) -> Result<()> {
        self.backups.write().unwrap().remove(&id);
        Ok(())
    }
}

#[async_trait]
impl IntegrationStore for TestHubStore {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
        self.proposals.write().unwrap().insert(p.id, p.clone());
        Ok(())
    }
    async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
        self.proposals
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(HubError::NotFound)
    }
    async fn list_proposals(
        &self,
        ns: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>> {
        Ok(self
            .proposals
            .read()
            .unwrap()
            .values()
            .filter(|p| p.namespace == ns && status.is_none_or(|s| p.status == s))
            .cloned()
            .collect())
    }
    async fn open_exists(&self, _ns: &str, _members: &[(String, Vec<Uuid>)]) -> Result<bool> {
        Ok(false)
    }
}

#[async_trait]
impl GrantStore for TestHubStore {
    async fn ensure_namespace(&self, ns: &str, owner: &AgentId) -> Result<(NamespaceInfo, bool)> {
        let mut created = false;
        let info = self
            .namespaces
            .write()
            .unwrap()
            .entry(ns.to_string())
            .or_insert_with(|| {
                created = true;
                NamespaceInfo {
                    namespace: ns.to_string(),
                    owner_agent: owner.clone(),
                    created_at: Utc::now(),
                }
            })
            .clone();
        Ok((info, created))
    }
    async fn get_namespace(&self, ns: &str) -> Result<Option<NamespaceInfo>> {
        Ok(self.namespaces.read().unwrap().get(ns).cloned())
    }
    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
        Ok(self.namespaces.read().unwrap().values().cloned().collect())
    }
    async fn grant(&self, r: &NamespaceGrantRecord) -> Result<()> {
        let mut g = self.grants.write().unwrap();
        g.retain(|x| !(x.namespace == r.namespace && x.agent_id == r.agent_id));
        g.push(r.clone());
        Ok(())
    }
    async fn revoke(&self, ns: &str, agent: &AgentId) -> Result<()> {
        self.grants
            .write()
            .unwrap()
            .retain(|x| !(x.namespace == ns && *agent == x.agent_id));
        Ok(())
    }
    async fn grants_for(&self, agent: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .filter(|x| &x.agent_id == agent)
            .cloned()
            .collect())
    }
    async fn can_read(&self, ns: &str, agent: &AgentId) -> Result<bool> {
        Ok(self.grants.read().unwrap().iter().any(|x| {
            (x.namespace == ns || x.namespace == "*") && x.agent_id == *agent && x.can_read
        }))
    }
    async fn can_write(&self, ns: &str, agent: &AgentId) -> Result<bool> {
        Ok(self.grants.read().unwrap().iter().any(|x| {
            (x.namespace == ns || x.namespace == "*") && x.agent_id == *agent && x.can_write
        }))
    }
    async fn owned_namespaces(&self, agent: &AgentId) -> Result<Vec<String>> {
        Ok(self
            .namespaces
            .read()
            .unwrap()
            .values()
            .filter(|n| &n.owner_agent == agent)
            .map(|n| n.namespace.clone())
            .collect())
    }
}

#[async_trait]
impl AgentStore for TestHubStore {
    async fn register_agent(&self, _reg: &AgentRegistration) -> Result<AgentRecord> {
        Err(HubError::Unavailable)
    }
    async fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>> {
        Ok(self.agents.read().unwrap().get(id).cloned())
    }
    async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        Ok(self.agents.read().unwrap().values().cloned().collect())
    }
    async fn set_agent_status(&self, _agent_id: &AgentId, _status: &str) -> Result<()> {
        Err(HubError::Unavailable)
    }
    async fn verify_agent_secret(&self, _agent_id: &AgentId, _secret: &str) -> Result<bool> {
        Ok(false)
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
        source_kind: Option<EvidenceSourceKind>,
        source_ref: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEvidence>> {
        let list: Vec<_> = self
            .evidences
            .read()
            .unwrap()
            .iter()
            .filter(|e| e.namespace == ns)
            .filter(|e| source_kind.is_none_or(|k| e.source_kind == k))
            .filter(|e| source_ref.is_none_or(|r| e.source_ref.as_deref() == Some(r)))
            .take(limit)
            .cloned()
            .collect();
        Ok(list)
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
        let mut list = self.evidences.write().unwrap();
        let before = list.len();
        list.retain(|e| !(e.namespace == ns && e.id == id));
        if list.len() < before {
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
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
    async fn list_relations_from(
        &self,
        _ns: &str,
        from_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        Ok(self
            .relations
            .read()
            .unwrap()
            .iter()
            .filter(|r| r.from_memory_id == from_memory_id)
            .cloned()
            .collect())
    }
    async fn list_relations_to(
        &self,
        _ns: &str,
        to_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        Ok(self
            .relations
            .read()
            .unwrap()
            .iter()
            .filter(|r| r.to_memory_id == to_memory_id)
            .cloned()
            .collect())
    }
    async fn delete_relation(
        &self,
        _ns: &str,
        from_memory_id: Uuid,
        to_memory_id: Uuid,
        relation_kind: MemoryRelationKind,
    ) -> Result<()> {
        self.relations.write().unwrap().retain(|r| {
            !(r.from_memory_id == from_memory_id
                && r.to_memory_id == to_memory_id
                && r.relation_kind == relation_kind)
        });
        Ok(())
    }
    async fn all_relations_in_namespace(&self, _ns: &str) -> Result<Vec<MemoryRelation>> {
        Ok(self.relations.read().unwrap().clone())
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
    async fn save_experience(&self, experience: &NewMemoryExperience) -> Result<MemoryExperience> {
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

    async fn update_experience(&self, experience: &MemoryExperience) -> Result<MemoryExperience> {
        self.experiences.write().unwrap().insert(
            (experience.namespace.clone(), experience.id),
            experience.clone(),
        );
        Ok(experience.clone())
    }

    async fn get_experience(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryExperience>> {
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
        let mut r = self.task_experiences.write().unwrap();
        if !r
            .iter()
            .any(|l| l.task_id == link.task_id && l.experience_id == link.experience_id)
        {
            r.push(link.clone());
        }
        Ok(())
    }

    async fn list_experiences_for_task(&self, task_id: Uuid) -> Result<Vec<MemoryTaskExperience>> {
        let r = self.task_experiences.read().unwrap();
        Ok(r.iter()
            .filter(|te| te.task_id == task_id)
            .cloned()
            .collect())
    }

    async fn all_experiences_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryExperience>> {
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
    async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
        let id = thread.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let record = CodingThread {
            id,
            namespace: thread.namespace.clone(),
            repository: thread.repository.clone(),
            branch: thread.branch.clone(),
            task_id: thread.task_id,
            title: thread.title.clone(),
            description: thread.description.clone(),
            created_at: now,
            updated_at: now,
        };
        self.coding_threads
            .write()
            .unwrap()
            .insert((thread.namespace.clone(), id), record.clone());
        Ok(record)
    }

    async fn get_thread(&self, namespace: &str, id: Uuid) -> Result<Option<CodingThread>> {
        let r = self.coding_threads.read().unwrap();
        Ok(r.get(&(namespace.to_string(), id)).cloned())
    }

    async fn list_threads(
        &self,
        namespace: &str,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodingThread>> {
        let r = self.coding_threads.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|t| {
                t.namespace == namespace && repository.is_none_or(|repo| t.repository == repo)
            })
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
        list.truncate(limit);
        Ok(list)
    }

    async fn save_revision(
        &self,
        revision: &NewCodingRevision,
        diff_hash: &str,
    ) -> Result<CodingRevision> {
        let id = revision.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let record = CodingRevision {
            id,
            thread_id: revision.thread_id,
            namespace: revision.namespace.clone(),
            repository: revision.repository.clone(),
            branch: revision.branch.clone(),
            commit_sha: revision.commit_sha.clone(),
            parent_sha: revision.parent_sha.clone(),
            file_path: revision.file_path.clone(),
            change_kind: revision.change_kind,
            diff_content: revision.diff_content.clone(),
            diff_hash: diff_hash.to_string(),
            symbol_fingerprint: revision.symbol_fingerprint.clone(),
            is_committed: revision.is_committed,
            author: revision.author.clone(),
            occurred_at: revision.occurred_at.unwrap_or(now),
            created_at: now,
            source_evidence_id: revision.source_evidence_id,
            source_task_id: revision.source_task_id,
        };
        self.coding_revisions
            .write()
            .unwrap()
            .insert((revision.namespace.clone(), id), record.clone());
        Ok(record)
    }

    async fn get_revision(&self, namespace: &str, id: Uuid) -> Result<Option<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        Ok(r.get(&(namespace.to_string(), id)).cloned())
    }

    async fn get_revision_by_hash(
        &self,
        namespace: &str,
        repository: &str,
        diff_hash: &str,
    ) -> Result<Option<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        Ok(r.values()
            .find(|rev| {
                rev.namespace == namespace
                    && rev.repository == repository
                    && rev.diff_hash == diff_hash
            })
            .cloned())
    }

    async fn list_revisions_for_thread(
        &self,
        namespace: &str,
        thread_id: Uuid,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|rev| rev.namespace == namespace && rev.thread_id == Some(thread_id))
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.occurred_at));
        list.truncate(limit);
        Ok(list)
    }

    async fn list_revisions_for_file(
        &self,
        namespace: &str,
        repository: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|rev| {
                rev.namespace == namespace
                    && rev.repository == repository
                    && rev.file_path == file_path
            })
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.occurred_at));
        list.truncate(limit);
        Ok(list)
    }

    async fn all_threads_in_namespace(&self, namespace: &str) -> Result<Vec<CodingThread>> {
        let r = self.coding_threads.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|t| t.namespace == namespace)
            .cloned()
            .collect();
        list.sort_by_key(|t| (t.created_at, t.id));
        Ok(list)
    }

    async fn all_revisions_in_namespace(&self, namespace: &str) -> Result<Vec<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|rev| rev.namespace == namespace)
            .cloned()
            .collect();
        list.sort_by_key(|r| (r.created_at, r.id));
        Ok(list)
    }

    async fn delete_thread(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut r = self.coding_threads.write().unwrap();
        if r.remove(&(namespace.to_string(), id)).is_some() {
            // Also clean up revisions referencing this thread
            let mut revs = self.coding_revisions.write().unwrap();
            revs.retain(|_, v| v.thread_id != Some(id));
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }

    async fn delete_revision(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut r = self.coding_revisions.write().unwrap();
        if r.remove(&(namespace.to_string(), id)).is_some() {
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_deterministic_and_normalized() {
        let e = TestEmbedder::default();
        let v1 = e.embed(&["abc".to_string()]).await.unwrap();
        let v2 = e.embed(&["abc".to_string()]).await.unwrap();
        assert_eq!(v1, v2, "相同输入必须得到相同向量");
        assert_ne!(v1[0], e.embed(&["abd".to_string()]).await.unwrap()[0]);

        let norm: f32 = v1[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }
}
