//! 开发与测试用的确定性嵌入器
//!
//! 与 `embed::MockEmbedder` 相同思路（SHA-256 哈希 → 归一化伪向量），
//! 定义在 core 中以避免 core → embed 的循环依赖；生产环境请使用
//! `embed` crate 的 `HttpEmbedder`。

use crate::infra::Embedder;
use async_trait::async_trait;
use common::{HubError, Result};

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
        crate::algorithm::sha256_pseudo_vector(text, self.dimension)
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

    /// 伪向量：余弦相似度是哈希噪声，不构成语义信号。
    /// 检索打分据此把向量维度标记为"不可用"，健康检查据此声明语义能力未就绪。
    fn provides_semantic_signal(&self) -> bool {
        false
    }
}

use crate::domain::{ChatCognitiveInput, CognitiveCandidate};
use crate::infra::ChatCognitiveExtractor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

/// 确定性"语义"测试嵌入器
///
/// 与 [`TestEmbedder`]（纯 SHA-256 哈希伪向量，同语义的两次文本得到互不相关的向量）
/// 不同：它按**字符桶计数**投影到固定维度，词形相近的文本会得到相近向量。
/// 这足以在测试中区分"向量通道是否真的被走到"，但它**不是真实语义模型**。
///
/// ⚠️ `provides_semantic_signal()` 返回 `true` 只是为了让**测试**能进入向量模式分支；
/// 它的相似度分布与真实嵌入模型完全不同，**不得**用于标定任何阈值，
/// 更不得用于生产（生产请用 `embed::HttpEmbedder`）。
pub struct SemanticTestEmbedder {
    dimension: usize,
}

impl Default for SemanticTestEmbedder {
    fn default() -> Self {
        Self { dimension: 64 }
    }
}

impl SemanticTestEmbedder {
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension: dimension.max(1),
        }
    }

    /// 字符桶投影：同一段文本总是得到同一个向量
    fn generate_vector(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0.0f32; self.dimension];
        for (i, ch) in text.chars().filter(|c| !c.is_whitespace()).enumerate() {
            // 字符码与位置共同决定落桶：既保留字符身份，也保留粗略的顺序信息
            let bucket = ((ch as usize).wrapping_mul(31).wrapping_add(i)) % self.dimension;
            vec[bucket] += 1.0;
        }
        let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut vec {
                *x /= norm;
            }
        }
        vec
    }
}

#[async_trait]
impl Embedder for SemanticTestEmbedder {
    fn model_id(&self) -> &str {
        "semantic-test-embedder"
    }

    fn dim(&self) -> usize {
        self.dimension
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.generate_vector(t)).collect())
    }

    /// 测试替身：让测试能进入向量模式分支。**不代表真实语义能力**。
    fn provides_semantic_signal(&self) -> bool {
        true
    }
}

/// 计数嵌入器：在真实嵌入器外面套一层，统计**调用次数**与**处理的文本条数**
///
/// 用途：证明"纯规则整合路径一次嵌入调用都不会发出"。
/// `provides_semantic_signal` 默认跟随被包装的嵌入器；
/// 用 `with_semantic_signal(true)` 可构造一个"提供真实语义信号"的计数包装器，
/// 供向量模式的调用隔离测试使用。
pub struct CountingEmbedder {
    inner: Arc<dyn Embedder>,
    calls: AtomicUsize,
    texts: AtomicUsize,
    semantic_signal: bool,
}

impl CountingEmbedder {
    /// 包装一个已有嵌入器
    pub fn new(inner: Arc<dyn Embedder>) -> Self {
        let semantic_signal = inner.provides_semantic_signal();
        Self {
            inner,
            calls: AtomicUsize::new(0),
            texts: AtomicUsize::new(0),
            semantic_signal,
        }
    }

    /// 强制声明是否提供真实语义信号（默认跟随被包装的嵌入器）
    pub fn with_semantic_signal(mut self, semantic_signal: bool) -> Self {
        self.semantic_signal = semantic_signal;
        self
    }

    /// 嵌入服务调用次数
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// 提交给嵌入服务的文本条数
    pub fn text_count(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }

    /// 重置计数
    pub fn reset(&self) {
        self.calls.store(0, Ordering::SeqCst);
        self.texts.store(0, Ordering::SeqCst);
    }
}

#[async_trait]
impl Embedder for CountingEmbedder {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        self.inner.embed(texts).await
    }

    fn provides_semantic_signal(&self) -> bool {
        self.semantic_signal
    }
}

impl std::fmt::Debug for CountingEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingEmbedder")
            .field("model_id", &self.inner.model_id())
            .field("calls", &self.call_count())
            .field("texts", &self.text_count())
            .finish()
    }
}

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
use common::{AccountId, AgentId};
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
    pub cognitive_proposals: RwLock<HashMap<Uuid, CognitiveProposalRecord>>,
    pub evidences: RwLock<Vec<MemoryEvidence>>,
    pub relations: RwLock<Vec<MemoryRelation>>,
    pub tasks: RwLock<HashMap<(String, Uuid), MemoryTask>>,
    pub experiences: RwLock<HashMap<(String, Uuid), MemoryExperience>>,
    pub experience_versions: RwLock<Vec<ExperienceVersion>>,
    pub task_experiences: RwLock<Vec<MemoryTaskExperience>>,
    pub coding_threads: RwLock<HashMap<(String, Uuid), CodingThread>>,
    pub coding_revisions: RwLock<HashMap<(String, Uuid), CodingRevision>>,
    pub collections: RwLock<Vec<MemoryCollection>>,
    pub collection_bindings: RwLock<Vec<CollectionBinding>>,
    /// 错误注入：置位后 add_relation 一律失败（测试次要写失败时的降级行为）
    pub fail_add_relation: std::sync::atomic::AtomicBool,
    /// 错误注入：置位后 revise 一律失败（测试 supersedes 封堵失败不拖垮主流程）
    pub fail_revise: std::sync::atomic::AtomicBool,
    /// 原子替换接口被调用的次数（验证 Replace 走批量事务而非逐步提交）
    pub replace_calls: std::sync::atomic::AtomicUsize,
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
        // 契约对齐（infra.rs）：save 要求条目已存在，否则 NotFound；
        // 无条件 upsert 会让依赖 TestHubStore 的测试验不出生产的丢写行为
        let mut w = self.items.write().unwrap();
        let key = (e.namespace.clone(), e.id);
        if !w.contains_key(&key) {
            return Err(HubError::NotFound);
        }
        w.insert(key, e.clone());
        Ok(e.clone())
    }

    /// 契约对齐（infra.rs）：乐观锁前置不成立（条目不存在或版本不符）
    /// 一律 Conflict，绝不静默覆盖
    async fn save_if_version(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
    ) -> Result<MemoryEntry> {
        let mut w = self.items.write().unwrap();
        match w.get_mut(&(entry.namespace.clone(), entry.id)) {
            Some(current) if current.version == expected_version => {
                *current = entry.clone();
                Ok(entry.clone())
            }
            _ => Err(HubError::Conflict),
        }
    }

    async fn revise(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
        change: VersionChange,
    ) -> Result<MemoryEntry> {
        if self.fail_revise.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(HubError::Unavailable);
        }
        // 契约对齐（infra.rs）：版本不符返回 Conflict 而非静默覆盖
        {
            let mut w = self.items.write().unwrap();
            match w.get_mut(&(entry.namespace.clone(), entry.id)) {
                Some(current) if current.version == expected_version => {
                    *current = entry.clone();
                }
                _ => return Err(HubError::Conflict),
            }
        }
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
            .filter(|e| filter.kinds.as_ref().is_none_or(|ks| ks.contains(&e.kind)))
            .filter(|e| {
                filter
                    .statuses
                    .as_ref()
                    .is_none_or(|ss| ss.contains(&e.status))
            })
            .filter(|e| {
                filter
                    .tags
                    .as_ref()
                    .is_none_or(|tags| tags.iter().all(|t| e.tags.contains(t)))
            })
            .filter(|e| {
                filter
                    .source_agent
                    .as_ref()
                    .is_none_or(|a| &e.source_agent == a)
            })
            .filter(|e| {
                filter
                    .key
                    .as_ref()
                    .is_none_or(|k| e.key.as_deref() == Some(k))
            })
            .filter(|e| {
                filter
                    .text_contains
                    .as_ref()
                    .is_none_or(|t| crate::fts_query_hits(&e.content, t) > 0)
            })
            .filter(|e| {
                filter
                    .scene
                    .as_ref()
                    .is_none_or(|s| e.scene.as_ref() == Some(s))
            })
            .filter(|e| {
                filter
                    .layer
                    .as_ref()
                    .is_none_or(|l| e.layer.as_ref() == Some(l))
            })
            .filter(|e| match &filter.valid_at {
                Some(at) => {
                    e.valid_from.is_none_or(|from| *at >= from)
                        && e.valid_until.is_none_or(|until| *at < until)
                }
                None => true,
            })
            .filter(|e| match &filter.before {
                Some((ts, id)) => (e.updated_at, e.id) < (*ts, *id),
                None => true,
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
        let no_limit = MemoryFilter {
            limit: None,
            ..filter.clone()
        };
        Ok(self.list(&no_limit).await?.len())
    }

    async fn similar(
        &self,
        _filter: &MemoryFilter,
        _qv: &[f32],
        _k: usize,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        Ok(vec![])
    }

    async fn embeddings_by_ids(
        &self,
        _namespace: &str,
        _ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, Vec<f32>)>> {
        // 与 similar 一致：测试桩不提供向量路
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

    async fn replace_namespaces_from_snapshots(
        &self,
        snapshots: &[(Namespace, crate::domain::RestoreSnapshot)],
    ) -> Result<()> {
        self.replace_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let items = self.items.read().unwrap().clone();
        let versions = self.versions.read().unwrap().clone();
        let evidences = self.evidences.read().unwrap().clone();
        let relations = self.relations.read().unwrap().clone();
        let tasks = self.tasks.read().unwrap().clone();
        let experiences = self.experiences.read().unwrap().clone();
        let experience_versions = self.experience_versions.read().unwrap().clone();
        let task_experiences = self.task_experiences.read().unwrap().clone();
        let coding_threads = self.coding_threads.read().unwrap().clone();
        let coding_revisions = self.coding_revisions.read().unwrap().clone();

        for (namespace, _) in snapshots {
            self.items
                .write()
                .unwrap()
                .retain(|(ns, _), _| ns != namespace);
            let namespace_entry_ids: std::collections::HashSet<Uuid> = items
                .keys()
                .filter(|(ns, _)| ns == namespace)
                .map(|(_, id)| *id)
                .collect();
            self.versions
                .write()
                .unwrap()
                .retain(|version| !namespace_entry_ids.contains(&version.memory_id));
            self.evidences
                .write()
                .unwrap()
                .retain(|evidence| evidence.namespace != *namespace);
            self.relations
                .write()
                .unwrap()
                .retain(|relation| relation.namespace.as_deref() != Some(namespace));
            self.tasks
                .write()
                .unwrap()
                .retain(|(ns, _), _| ns != namespace);
            self.experiences
                .write()
                .unwrap()
                .retain(|(ns, _), _| ns != namespace);
            self.experience_versions
                .write()
                .unwrap()
                .retain(|version| version.namespace != *namespace);
            self.task_experiences
                .write()
                .unwrap()
                .retain(|link| link.namespace.as_deref() != Some(namespace));
            self.coding_threads
                .write()
                .unwrap()
                .retain(|(ns, _), _| ns != namespace);
            self.coding_revisions
                .write()
                .unwrap()
                .retain(|(ns, _), _| ns != namespace);
        }

        let result = async {
            for (namespace, snapshot) in snapshots {
                for entry in &snapshot.entries {
                    if entry.namespace != *namespace {
                        return Err(HubError::InvalidArgument(
                            "snapshot namespace mismatch".into(),
                        ));
                    }
                    self.insert_initial(entry).await?;
                }
                for evidence in &snapshot.evidences {
                    if evidence.namespace != *namespace {
                        return Err(HubError::InvalidArgument(
                            "snapshot namespace mismatch".into(),
                        ));
                    }
                    self.append_evidence(&NewEvidence {
                        id: Some(evidence.id),
                        namespace: evidence.namespace.clone(),
                        source_kind: evidence.source_kind,
                        source_ref: evidence.source_ref.clone(),
                        content_hash: evidence.content_hash.clone(),
                        excerpt: evidence.excerpt.clone(),
                        occurred_at: evidence.occurred_at,
                        expires_at: evidence.expires_at,
                    })
                    .await?;
                }
                for task in &snapshot.tasks {
                    self.create_task(&NewMemoryTask {
                        id: Some(task.id),
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
                    })
                    .await?;
                }
                for experience in &snapshot.experiences {
                    self.save_experience(&NewMemoryExperience {
                        id: Some(experience.id),
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
                    })
                    .await?;
                    self.update_experience(experience).await?;
                }
                for version in &snapshot.experience_versions {
                    self.append_experience_version(&NewExperienceVersion {
                        id: Some(version.id),
                        experience_id: version.experience_id,
                        namespace: version.namespace.clone(),
                        version: version.version,
                        snapshot: version.snapshot.clone(),
                        change_kind: version.change_kind,
                        changed_by: version.changed_by.clone(),
                        status: version.status,
                        note: version.note.clone(),
                        changed_at: version.changed_at,
                    })
                    .await?;
                }
                for link in &snapshot.task_experiences {
                    self.link_task_experience(link).await?;
                }
                for relation in &snapshot.relations {
                    self.add_relation(
                        namespace,
                        &NewMemoryRelation {
                            from_memory_id: relation.from_memory_id,
                            to_memory_id: relation.to_memory_id,
                            relation_kind: relation.relation_kind,
                            confidence: relation.confidence,
                            source_evidence_id: relation.source_evidence_id,
                        },
                    )
                    .await?;
                }
                for thread in &snapshot.coding_threads {
                    self.create_thread(&NewCodingThread {
                        id: Some(thread.id),
                        namespace: thread.namespace.clone(),
                        repository: thread.repository.clone(),
                        branch: thread.branch.clone(),
                        task_id: thread.task_id,
                        title: thread.title.clone(),
                        description: thread.description.clone(),
                    })
                    .await?;
                }
                for revision in &snapshot.coding_revisions {
                    self.save_revision(
                        &NewCodingRevision {
                            id: Some(revision.id),
                            thread_id: revision.thread_id,
                            namespace: revision.namespace.clone(),
                            repository: revision.repository.clone(),
                            branch: revision.branch.clone(),
                            commit_sha: revision.commit_sha.clone(),
                            parent_sha: revision.parent_sha.clone(),
                            file_path: revision.file_path.clone(),
                            change_kind: revision.change_kind,
                            diff_content: revision.diff_content.clone(),
                            symbol_fingerprint: revision.symbol_fingerprint.clone(),
                            is_committed: revision.is_committed,
                            author: revision.author.clone(),
                            occurred_at: Some(revision.occurred_at),
                            source_evidence_id: revision.source_evidence_id,
                            source_task_id: revision.source_task_id,
                        },
                        &revision.diff_hash,
                    )
                    .await?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            *self.items.write().unwrap() = items;
            *self.versions.write().unwrap() = versions;
            *self.evidences.write().unwrap() = evidences;
            *self.relations.write().unwrap() = relations;
            *self.tasks.write().unwrap() = tasks;
            *self.experiences.write().unwrap() = experiences;
            *self.experience_versions.write().unwrap() = experience_versions;
            *self.task_experiences.write().unwrap() = task_experiences;
            *self.coding_threads.write().unwrap() = coding_threads;
            *self.coding_revisions.write().unwrap() = coding_revisions;
            return Err(error);
        }
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
impl CognitiveProposalStore for TestHubStore {
    async fn save_cognitive_proposal(&self, p: &CognitiveProposalRecord) -> Result<()> {
        let mut w = self.cognitive_proposals.write().unwrap();
        if w.contains_key(&p.id) {
            return Err(HubError::Conflict);
        }
        w.insert(p.id, p.clone());
        Ok(())
    }

    async fn get_cognitive_proposal(&self, id: Uuid) -> Result<CognitiveProposalRecord> {
        self.cognitive_proposals
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn list_cognitive_proposals(
        &self,
        ns: &str,
        status: Option<CognitiveProposalStatus>,
    ) -> Result<Vec<CognitiveProposalRecord>> {
        let mut out: Vec<CognitiveProposalRecord> = self
            .cognitive_proposals
            .read()
            .unwrap()
            .values()
            .filter(|p| p.namespace == ns && status.is_none_or(|s| p.status == s))
            .cloned()
            .collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.created_at));
        Ok(out)
    }

    async fn update_cognitive_proposal_status(
        &self,
        id: Uuid,
        expected_status: CognitiveProposalStatus,
        new_status: CognitiveProposalStatus,
        rejection_reason: Option<&str>,
        resulting_memory_id: Option<Uuid>,
        resolved_by: Option<&str>,
    ) -> Result<CognitiveProposalRecord> {
        let mut w = self.cognitive_proposals.write().unwrap();
        let p = w.get_mut(&id).ok_or(HubError::NotFound)?;
        if p.status != expected_status {
            return Err(HubError::Conflict);
        }
        p.status = new_status;
        p.rejection_reason = rejection_reason.map(str::to_string);
        p.resulting_memory_id = resulting_memory_id;
        p.resolved_by = resolved_by.map(str::to_string);
        p.updated_at = Utc::now();
        Ok(p.clone())
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
            expires_at: ev.expires_at,
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
    /// 点查覆盖：不受 `list_evidence` 的 limit 截断影响，
    /// 保证幂等查重测试能覆盖"证据数超过窗口"的场景
    async fn find_evidence_by_hash(
        &self,
        ns: &str,
        source_kind: EvidenceSourceKind,
        source_ref: &str,
        content_hash: &str,
    ) -> Result<Option<MemoryEvidence>> {
        Ok(self
            .evidences
            .read()
            .unwrap()
            .iter()
            .find(|e| {
                e.namespace == ns
                    && e.source_kind == source_kind
                    && e.source_ref.as_deref() == Some(source_ref)
                    && e.content_hash == content_hash
            })
            .cloned())
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

    async fn purge_expired_evidence(&self, ns: &str, now: chrono::DateTime<Utc>) -> Result<usize> {
        let mut list = self.evidences.write().unwrap();
        let before = list.len();
        list.retain(|e| !(e.namespace == ns && e.expires_at.is_some_and(|exp| exp <= now)));
        Ok(before - list.len())
    }
}

#[async_trait]
impl RelationStore for TestHubStore {
    async fn add_relation(&self, ns: &str, rel: &NewMemoryRelation) -> Result<MemoryRelation> {
        if self
            .fail_add_relation
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(HubError::Unavailable);
        }
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
            version: 1,
            superseded_by: None,
            verified_at: None,
            verified_by: None,
            verification_note: None,
            status_reason: None,
            source_count: 0,
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

    async fn update_experience_if_version(
        &self,
        experience: &MemoryExperience,
        expected_version: i64,
    ) -> Result<MemoryExperience> {
        let mut w = self.experiences.write().unwrap();
        let key = (experience.namespace.clone(), experience.id);
        match w.get(&key) {
            Some(current) if current.version == expected_version => {
                w.insert(key, experience.clone());
                Ok(experience.clone())
            }
            Some(_) => Err(HubError::Conflict),
            None => Err(HubError::NotFound),
        }
    }

    async fn append_experience_version(
        &self,
        version: &NewExperienceVersion,
    ) -> Result<ExperienceVersion> {
        let mut w = self.experience_versions.write().unwrap();
        if let Some(existing) = w
            .iter()
            .find(|v| v.experience_id == version.experience_id && v.version == version.version)
        {
            return Ok(existing.clone());
        }
        let record = ExperienceVersion {
            id: version.id.unwrap_or_else(Uuid::new_v4),
            experience_id: version.experience_id,
            namespace: version.namespace.clone(),
            version: version.version,
            snapshot: version.snapshot.clone(),
            change_kind: version.change_kind,
            changed_by: version.changed_by.clone(),
            status: version.status,
            note: version.note.clone(),
            changed_at: version.changed_at,
        };
        w.push(record.clone());
        Ok(record)
    }

    async fn list_experience_versions(
        &self,
        namespace: &str,
        experience_id: Uuid,
        limit: usize,
    ) -> Result<Vec<ExperienceVersion>> {
        let r = self.experience_versions.read().unwrap();
        let mut list: Vec<ExperienceVersion> = r
            .iter()
            .filter(|v| v.namespace == namespace && v.experience_id == experience_id)
            .cloned()
            .collect();
        list.sort_by_key(|v| std::cmp::Reverse(v.version));
        list.truncate(limit.max(1));
        Ok(list)
    }

    async fn all_experience_versions_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<ExperienceVersion>> {
        let r = self.experience_versions.read().unwrap();
        let mut list: Vec<ExperienceVersion> = r
            .iter()
            .filter(|v| v.namespace == namespace)
            .cloned()
            .collect();
        list.sort_by_key(|v| (v.experience_id, v.version));
        Ok(list)
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
        offset: usize,
    ) -> Result<Vec<MemoryExperience>> {
        let r = self.experiences.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|e| e.namespace == namespace && status.is_none_or(|s| e.status == s))
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(list.into_iter().skip(offset).take(limit.max(1)).collect())
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
            let mut versions = self.experience_versions.write().unwrap();
            versions.retain(|v| v.experience_id != id);
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

#[async_trait]
impl CollectionStore for TestHubStore {
    async fn upsert_collection(&self, collection: &MemoryCollection) -> Result<MemoryCollection> {
        let mut w = self.collections.write().unwrap();
        if let Some(existing) = w
            .iter_mut()
            .find(|c| c.account_id == collection.account_id && c.name == collection.name)
        {
            existing.description = collection.description.clone();
            existing.filter = collection.filter.clone();
            existing.updated_at = Utc::now();
            return Ok(existing.clone());
        }
        w.push(collection.clone());
        Ok(collection.clone())
    }

    async fn get_collection(
        &self,
        account_id: AccountId,
        id: Uuid,
    ) -> Result<Option<MemoryCollection>> {
        Ok(self
            .collections
            .read()
            .unwrap()
            .iter()
            .find(|c| c.account_id == account_id && c.id == id)
            .cloned())
    }

    async fn get_collection_by_name(
        &self,
        account_id: AccountId,
        name: &str,
    ) -> Result<Option<MemoryCollection>> {
        Ok(self
            .collections
            .read()
            .unwrap()
            .iter()
            .find(|c| c.account_id == account_id && c.name == name)
            .cloned())
    }

    async fn list_collections(&self, account_id: AccountId) -> Result<Vec<MemoryCollection>> {
        Ok(self
            .collections
            .read()
            .unwrap()
            .iter()
            .filter(|c| c.account_id == account_id)
            .cloned()
            .collect())
    }

    async fn delete_collection(&self, account_id: AccountId, id: Uuid) -> Result<()> {
        let mut w = self.collections.write().unwrap();
        let before = w.len();
        w.retain(|c| !(c.account_id == account_id && c.id == id));
        if w.len() == before {
            return Err(HubError::NotFound);
        }
        self.collection_bindings
            .write()
            .unwrap()
            .retain(|b| b.collection_id != id);
        Ok(())
    }

    async fn bind_collection(&self, binding: &CollectionBinding) -> Result<()> {
        let mut w = self.collection_bindings.write().unwrap();
        if !w
            .iter()
            .any(|b| b.collection_id == binding.collection_id && b.agent_id == binding.agent_id)
        {
            w.push(binding.clone());
        }
        Ok(())
    }

    async fn unbind_collection(&self, collection_id: Uuid, agent_id: &AgentId) -> Result<()> {
        let mut w = self.collection_bindings.write().unwrap();
        let before = w.len();
        w.retain(|b| !(b.collection_id == collection_id && b.agent_id == *agent_id));
        if w.len() == before {
            return Err(HubError::NotFound);
        }
        Ok(())
    }

    async fn list_bindings(&self, collection_id: Uuid) -> Result<Vec<CollectionBinding>> {
        Ok(self
            .collection_bindings
            .read()
            .unwrap()
            .iter()
            .filter(|b| b.collection_id == collection_id)
            .cloned()
            .collect())
    }

    async fn is_bound(&self, collection_id: Uuid, agent_id: &AgentId) -> Result<bool> {
        Ok(self
            .collection_bindings
            .read()
            .unwrap()
            .iter()
            .any(|b| b.collection_id == collection_id && b.agent_id == *agent_id))
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
