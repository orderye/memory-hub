//! 内存级存储实现（开发、测试与无数据库环境）
//!
//! 单进程内实现全部存储契约（记忆 / 备份 / 整合提案 / 命名空间授权 / Agent / 账户），
//! 语义与 PostgreSQL 实现对齐：命名空间隔离、owner 首次写入、游标分页、
//! 软删除归档、合并溯源。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, HubError, Result};
use mem_core::{
    AccountRecord, BackupRecord, CodingRevision, CodingStore, CodingThread, EvidenceSourceKind,
    ExperienceStatus, IntegrationProposal, MemoryEntry, MemoryEvidence, MemoryExperience,
    MemoryFilter, MemoryRelation, MemoryRelationKind, MemoryStats, MemoryStatus, MemoryTask,
    MemoryTaskExperience, MemoryVersion, Namespace, NamespaceGrantRecord, NamespaceInfo,
    NewCodingRevision, NewCodingThread, NewEvidence, NewMemoryExperience, NewMemoryRelation,
    NewMemoryTask, ProposalStatus, Scored, SessionRecord, TaskStatus, VersionChange,
};
use std::collections::BTreeMap;
use std::sync::RwLock;
use uuid::Uuid;

type RelationKey = (Uuid, Uuid, MemoryRelationKind);
type RelationEntry = (Namespace, MemoryRelation);

/// 内存存储（多合一）
#[derive(Default)]
pub struct InMemoryHubStore {
    /// (namespace, id) → 条目
    items: RwLock<BTreeMap<(Namespace, Uuid), MemoryEntry>>,
    versions: RwLock<Vec<MemoryVersion>>,
    evidences: RwLock<BTreeMap<(Namespace, Uuid), MemoryEvidence>>,
    relations: RwLock<BTreeMap<RelationKey, RelationEntry>>,
    tasks: RwLock<BTreeMap<(Namespace, Uuid), MemoryTask>>,
    experiences: RwLock<BTreeMap<(Namespace, Uuid), MemoryExperience>>,
    task_experiences: RwLock<Vec<(Namespace, MemoryTaskExperience)>>,
    coding_threads: RwLock<BTreeMap<(Namespace, Uuid), CodingThread>>,
    coding_revisions: RwLock<BTreeMap<(Namespace, Uuid), CodingRevision>>,
    backups: RwLock<BTreeMap<Uuid, BackupRecord>>,
    proposals: RwLock<BTreeMap<Uuid, IntegrationProposal>>,
    namespaces: RwLock<BTreeMap<Namespace, NamespaceInfo>>,
    grants: RwLock<Vec<NamespaceGrantRecord>>,
    agents: RwLock<BTreeMap<AgentId, StoredAgent>>,
    accounts: RwLock<BTreeMap<AccountId, StoredAccount>>,
    /// sha256(会话令牌) → 会话
    sessions: RwLock<BTreeMap<Vec<u8>, SessionRecord>>,
}

/// 内部账户记录（含口令散列）
#[derive(Default, Clone)]
struct StoredAccount {
    username: String,
    display_name: String,
    status: String,
    password_hash: Option<String>,
    created_at: Option<chrono::DateTime<Utc>>,
}

/// 内部 Agent 记录（含密钥散列与账户归属）
#[derive(Default, Clone)]
struct StoredAgent {
    name: String,
    kind: String,
    status: String,
    owner_account_id: Option<AccountId>,
    secret_hash: Option<String>,
    redirect_uris: Vec<String>,
    max_scopes: Vec<common::Scope>,
    created_at: Option<chrono::DateTime<Utc>>,
}

impl InMemoryHubStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 写回条目。`expected_version` 为 `Some(v)` 时要求库中当前版本恰为 v，
    /// 否则返回 Conflict（与 PostgreSQL 实现同一语义）。`None` 为无条件写回。
    async fn apply_revision(
        &self,
        entry: &MemoryEntry,
        expected_version: Option<i64>,
    ) -> Result<MemoryEntry> {
        let mut w = self.items.write().unwrap();
        let key = (entry.namespace.clone(), entry.id);
        match w.get_mut(&key) {
            Some(current) => match expected_version {
                Some(v) if current.version != v => Err(common::HubError::Conflict),
                _ => {
                    *current = entry.clone();
                    Ok(entry.clone())
                }
            },
            None => Err(common::HubError::NotFound),
        }
    }

    /// 当前条目总数（测试辅助）
    pub fn count_all(&self) -> usize {
        self.items.read().unwrap().len()
    }

    /// 应用过滤条件（共享辅助）
    fn matches_filter(e: &MemoryEntry, f: &MemoryFilter) -> bool {
        if let Some(ns) = &f.namespace {
            if &e.namespace != ns {
                return false;
            }
        }
        if let Some(kinds) = &f.kinds {
            if !kinds.contains(&e.kind) {
                return false;
            }
        }
        if let Some(statuses) = &f.statuses {
            if !statuses.contains(&e.status) {
                return false;
            }
        }
        if let Some(tags) = &f.tags {
            if !tags.iter().all(|t| e.tags.contains(t)) {
                return false;
            }
        }
        if let Some(agent) = &f.source_agent {
            if &e.source_agent != agent {
                return false;
            }
        }
        if let Some(key) = &f.key {
            if e.key.as_deref() != Some(key.as_str()) {
                return false;
            }
        }
        if let Some(text) = &f.text_contains {
            // 与 PostgreSQL FTS 同源的分词部分匹配（OR 语义）：
            // 此前用整段查询做裸子串匹配，内存路径与生产路径词法不同
            if mem_core::fts_query_hits(&e.content, text) == 0 {
                return false;
            }
        }
        if let Some((ts, id)) = &f.before {
            if (e.updated_at, e.id) >= (*ts, *id) {
                return false;
            }
        }
        if let Some(scene) = &f.scene {
            if e.scene.as_ref() != Some(scene) {
                return false;
            }
        }
        if let Some(layer) = &f.layer {
            if e.layer.as_ref() != Some(layer) {
                return false;
            }
        }
        if let Some(valid_at) = &f.valid_at {
            if let Some(from) = e.valid_from {
                if *valid_at < from {
                    return false;
                }
            }
            if let Some(until) = e.valid_until {
                if *valid_at >= until {
                    return false;
                }
            }
        }
        true
    }
}

#[async_trait]
impl mem_core::MemoryStore for InMemoryHubStore {
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut w = self.items.write().unwrap();
        let key = (entry.namespace.clone(), entry.id);
        if w.contains_key(&key) {
            return Err(HubError::Conflict);
        }
        w.insert(key, entry.clone());
        Ok(entry.clone())
    }

    async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut w = self.items.write().unwrap();
        let key = (entry.namespace.clone(), entry.id);
        if w.contains_key(&key) {
            return Err(HubError::Conflict);
        }
        w.insert(key, entry.clone());
        let version = MemoryVersion {
            memory_id: entry.id,
            version: entry.version,
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
        };
        self.versions.write().unwrap().push(version);
        Ok(entry.clone())
    }

    async fn get(&self, namespace: &str, id: Uuid) -> Result<MemoryEntry> {
        self.items
            .read()
            .unwrap()
            .get(&(namespace.to_string(), id))
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let r = self.items.read().unwrap();
        let mut candidates: Vec<_> = r
            .values()
            .filter(|e| e.namespace == namespace && e.key.as_deref() == Some(key))
            .cloned()
            .collect();
        // 状态优先级：active (0) > archived (1) > merged (2)，同状态按 updated_at 倒序
        candidates.sort_by(|a, b| {
            let status_priority = |s: MemoryStatus| match s {
                MemoryStatus::Active => 0,
                MemoryStatus::Archived => 1,
                MemoryStatus::Merged => 2,
            };
            status_priority(a.status)
                .cmp(&status_priority(b.status))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        Ok(candidates.into_iter().next())
    }

    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        self.items
            .write()
            .unwrap()
            .insert((entry.namespace.clone(), entry.id), entry.clone());
        Ok(entry.clone())
    }

    /// 与 PostgreSQL 实现同一语义的乐观并发修订：库中当前版本不是
    /// `expected_version` 就返回 Conflict，而不是静默覆盖。
    /// 两套实现在这一点上必须一致，否则内存测试验不出生产的丢更新行为。
    async fn save_if_version(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
    ) -> Result<MemoryEntry> {
        Self::apply_revision(self, entry, Some(expected_version)).await
    }

    /// 原子修订：版本校验 + 写回条目 + 追加历史。
    /// 条目锁在追加历史之前释放（`RwLock` 守卫不是 `Send`，不得跨 `await` 持有）。
    async fn revise(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
        change: mem_core::domain::VersionChange,
    ) -> Result<MemoryEntry> {
        let saved = Self::apply_revision(self, entry, Some(expected_version)).await?;
        self.record_version(&MemoryVersion {
            memory_id: saved.id,
            version: saved.version,
            content: saved.content.clone(),
            tags: saved.tags.clone(),
            metadata: saved.metadata.clone(),
            confidence: saved.confidence,
            changed_by: saved.source_agent.clone(),
            change_type: change,
            changed_at: saved.updated_at,
            scene: saved.scene,
            layer: saved.layer,
            valid_from: saved.valid_from,
            valid_until: saved.valid_until,
            occurred_at: saved.occurred_at,
        })
        .await?;
        Ok(saved)
    }

    async fn delete(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.items.write().unwrap();
        w.remove(&(namespace.to_string(), id));
        let mut rels = self.relations.write().unwrap();
        rels.retain(|(from_id, to_id, _), _| *from_id != id && *to_id != id);
        Ok(())
    }

    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
        let mut out: Vec<MemoryEntry> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| Self::matches_filter(e, filter))
            .cloned()
            .collect();
        match &filter.text_contains {
            // 与 PostgreSQL 侧一致：带查询词时按词法相关度（词元命中数）排序，
            // 不做"按更新时间截断"——否则更相关但更旧的文档会在打分前被丢弃
            Some(text) => {
                out.sort_by(|a, b| {
                    let ha = mem_core::fts_query_hits(&a.content, text);
                    let hb = mem_core::fts_query_hits(&b.content, text);
                    hb.cmp(&ha)
                        .then(b.updated_at.cmp(&a.updated_at))
                        .then(b.id.cmp(&a.id))
                });
            }
            None => out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id))),
        }
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
        Ok(self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| Self::matches_filter(e, &no_limit))
            .count())
    }

    async fn similar(
        &self,
        filter: &MemoryFilter,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        // 状态缺省时按 active 处理（与历史行为一致，避免默认召回已归档条目）
        let mut effective = filter.clone();
        if effective.statuses.is_none() {
            effective.statuses = Some(vec![MemoryStatus::Active]);
        }
        let mut scored: Vec<Scored<MemoryEntry>> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| Self::matches_filter(e, &effective))
            .filter(|e| e.embedding.is_some())
            .filter_map(|e| {
                let iv = e.embedding.as_ref()?;
                if iv.len() != embedding.len() {
                    return None;
                }
                let dot: f32 = iv.iter().zip(embedding.iter()).map(|(x, y)| x * y).sum();
                Some(Scored {
                    item: e.clone(),
                    score: dot.clamp(0.0, 1.0),
                })
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }

    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>> {
        let mut out: Vec<MemoryEntry> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| e.namespace == namespace)
            .filter(|e| include_merged || e.status != MemoryStatus::Merged)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let mut ns: Vec<Namespace> = self
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
        let mut w = self.versions.write().unwrap();
        // 同 (memory_id, version) 只保留首条（幂等重放保护）
        if w.iter()
            .any(|x| x.memory_id == v.memory_id && x.version == v.version)
        {
            return Ok(());
        }
        w.push(v.clone());
        Ok(())
    }

    async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
        let mut out: Vec<MemoryVersion> = self
            .versions
            .read()
            .unwrap()
            .iter()
            .filter(|v| v.memory_id == id)
            .cloned()
            .collect();
        out.sort_by_key(|v| v.version);
        Ok(out)
    }

    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize> {
        // 第一遍：收集待归档条目快照（避免持锁写版本表）
        let expired: Vec<MemoryEntry> = {
            let r = self.items.read().unwrap();
            r.values()
                .filter(|e| {
                    e.status == MemoryStatus::Active && e.expires_at.is_some_and(|exp| exp <= now)
                })
                .cloned()
                .collect()
        };

        // 第二遍：逐条归档并记录版本历史
        let mut count = 0usize;
        for mut e in expired {
            e.status = MemoryStatus::Archived;
            e.version += 1;
            e.updated_at = now;
            let version = MemoryVersion {
                memory_id: e.id,
                version: e.version,
                content: e.content.clone(),
                tags: e.tags.clone(),
                metadata: e.metadata.clone(),
                confidence: e.confidence,
                changed_by: "system".to_string(),
                change_type: VersionChange::Archived,
                changed_at: now,
                scene: e.scene,
                layer: e.layer,
                valid_from: e.valid_from,
                valid_until: e.valid_until,
                occurred_at: e.occurred_at,
            };
            self.items
                .write()
                .unwrap()
                .insert((e.namespace.clone(), e.id), e);
            self.record_version(&version).await?;
            count += 1;
        }
        Ok(count)
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl mem_core::BackupStore for InMemoryHubStore {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()> {
        self.backups
            .write()
            .unwrap()
            .insert(backup.id, backup.clone());
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

    /// 列举快照（摘要视图）：与 PostgreSQL 实现保持同一语义——不返回 `payload`，
    /// 需要负载的调用方必须走 `get_backup(id)`。两套实现若在这一点上分叉，
    /// 内存测试就会验不出生产行为。
    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>> {
        let mut out: Vec<BackupRecord> = self
            .backups
            .read()
            .unwrap()
            .values()
            .filter(|b| namespace.is_none_or(|n| b.namespace == n))
            .cloned()
            .map(|mut b| {
                b.payload = serde_json::Value::Null;
                b
            })
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(out)
    }

    async fn delete_backup(&self, id: Uuid) -> Result<()> {
        self.backups.write().unwrap().remove(&id);
        Ok(())
    }
}

#[async_trait]
impl mem_core::IntegrationStore for InMemoryHubStore {
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
        namespace: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>> {
        let mut out: Vec<IntegrationProposal> = self
            .proposals
            .read()
            .unwrap()
            .values()
            .filter(|p| p.namespace == namespace)
            .filter(|p| status.is_none_or(|s| p.status == s))
            .cloned()
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(out)
    }

    async fn open_exists(&self, namespace: &str, members: &[(String, Vec<Uuid>)]) -> Result<bool> {
        let guard = self.proposals.read().unwrap();
        Ok(guard.values().any(|p| {
            p.status == ProposalStatus::Open
                && p.namespace == namespace
                && members.iter().any(|(kind, ids)| {
                    if kind != p.kind.as_str() || ids.len() != p.member_ids.len() {
                        return false;
                    }
                    let s1: std::collections::HashSet<_> = ids.iter().collect();
                    let s2: std::collections::HashSet<_> = p.member_ids.iter().collect();
                    s1 == s2
                })
        }))
    }
}

#[async_trait]
impl mem_core::GrantStore for InMemoryHubStore {
    async fn ensure_namespace(
        &self,
        namespace: &str,
        owner: &AgentId,
    ) -> Result<(NamespaceInfo, bool)> {
        let mut w = self.namespaces.write().unwrap();
        let mut created = false;
        let info = w
            .entry(namespace.to_string())
            .or_insert_with(|| {
                created = true;
                NamespaceInfo {
                    namespace: namespace.to_string(),
                    owner_agent: owner.clone(),
                    created_at: Utc::now(),
                }
            })
            .clone();
        Ok((info, created))
    }

    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>> {
        Ok(self.namespaces.read().unwrap().get(namespace).cloned())
    }

    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
        Ok(self.namespaces.read().unwrap().values().cloned().collect())
    }

    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()> {
        let mut w = self.grants.write().unwrap();
        w.retain(|x| !(x.namespace == record.namespace && x.agent_id == record.agent_id));
        w.push(record.clone());
        Ok(())
    }

    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()> {
        self.grants
            .write()
            .unwrap()
            .retain(|x| !(x.namespace == namespace && x.agent_id == *agent_id));
        Ok(())
    }

    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .filter(|x| &x.agent_id == agent_id)
            .cloned()
            .collect())
    }

    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        // 沙箱语义：授权行必须精确匹配命名空间；`*` 通配授权已废除
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .any(|x| x.namespace == namespace && x.agent_id == *agent_id && x.can_read))
    }

    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .any(|x| x.namespace == namespace && x.agent_id == *agent_id && x.can_write))
    }

    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>> {
        Ok(self
            .namespaces
            .read()
            .unwrap()
            .values()
            .filter(|n| &n.owner_agent == agent_id)
            .map(|n| n.namespace.clone())
            .collect())
    }
}

#[async_trait]
impl mem_core::AgentStore for InMemoryHubStore {
    async fn register_agent(
        &self,
        reg: &mem_core::AgentRegistration,
    ) -> Result<mem_core::AgentRecord> {
        let mut w = self.agents.write().unwrap();
        let entry = w
            .entry(reg.agent_id.clone())
            .or_insert_with(|| StoredAgent {
                name: reg.name.clone(),
                kind: reg.kind.clone(),
                status: "active".to_string(),
                owner_account_id: Some(reg.owner_account_id),
                secret_hash: None,
                redirect_uris: Vec::new(),
                max_scopes: Vec::new(),
                created_at: Some(Utc::now()),
            });
        // upsert 语义：名称/类型/账户归属/授权域以本次为准；密钥仅在提供时更新
        entry.name = reg.name.clone();
        entry.kind = reg.kind.clone();
        entry.owner_account_id = Some(reg.owner_account_id);
        entry.redirect_uris = reg.redirect_uris.clone();
        entry.max_scopes = reg.max_scopes.clone();
        if let Some(secret) = &reg.client_secret {
            entry.secret_hash = Some(crate::secret::hash_secret(&reg.agent_id, secret));
        }
        Ok(agent_record(&reg.agent_id, entry))
    }

    async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<mem_core::AgentRecord>> {
        Ok(self
            .agents
            .read()
            .unwrap()
            .get(agent_id)
            .map(|a| agent_record(agent_id, a)))
    }

    async fn list_agents(&self) -> Result<Vec<mem_core::AgentRecord>> {
        Ok(self
            .agents
            .read()
            .unwrap()
            .iter()
            .map(|(id, a)| agent_record(id, a))
            .collect())
    }

    async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
        let mut w = self.agents.write().unwrap();
        match w.get_mut(agent_id) {
            Some(a) => {
                a.status = status.to_string();
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
        let w = self.agents.read().unwrap();
        Ok(match w.get(agent_id) {
            Some(a) => match &a.secret_hash {
                Some(h) => crate::secret::verify_secret(agent_id, secret, h),
                None => false,
            },
            None => false,
        })
    }
}

/// StoredAgent → AgentRecord
fn agent_record(agent_id: &str, a: &StoredAgent) -> mem_core::AgentRecord {
    mem_core::AgentRecord {
        agent_id: agent_id.to_string(),
        name: a.name.clone(),
        kind: a.kind.clone(),
        status: a.status.clone(),
        owner_account_id: a.owner_account_id.unwrap_or(common::BOOTSTRAP_ACCOUNT_ID),
        redirect_uris: a.redirect_uris.clone(),
        max_scopes: a.max_scopes.clone(),
        confidential: a.secret_hash.is_some(),
        created_at: a.created_at.unwrap_or_else(Utc::now),
    }
}

/// StoredAccount → AccountRecord（不暴露口令散列本体）
fn account_record(id: &AccountId, a: &StoredAccount) -> AccountRecord {
    AccountRecord {
        id: *id,
        username: a.username.clone(),
        display_name: a.display_name.clone(),
        status: a.status.clone(),
        has_password: a.password_hash.is_some(),
        created_at: a.created_at.unwrap_or_else(Utc::now),
    }
}

#[async_trait]
impl mem_core::AccountStore for InMemoryHubStore {
    async fn create_account(
        &self,
        username: &str,
        password_hash: Option<&str>,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        let mut w = self.accounts.write().unwrap();
        if w.values().any(|a| a.username == normalized) {
            return Err(HubError::InvalidArgument(format!(
                "用户名 {} 已被占用",
                normalized
            )));
        }
        let id = Uuid::new_v4();
        let record = StoredAccount {
            username: normalized,
            display_name: display_name.to_string(),
            status: "active".to_string(),
            password_hash: password_hash.map(str::to_string),
            created_at: Some(Utc::now()),
        };
        w.insert(id, record.clone());
        Ok(account_record(&id, &record))
    }

    async fn ensure_account(
        &self,
        id: &AccountId,
        username: &str,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        let mut w = self.accounts.write().unwrap();
        // 用户名已被别的账户占用时，退回按 ID 查找（保持幂等）
        let existing = w.get(id).map(|a| (id.to_owned(), a.clone())).or_else(|| {
            w.iter()
                .find(|(_, a)| a.username == normalized)
                .map(|(k, a)| (*k, a.clone()))
        });
        if let Some((found_id, record)) = existing {
            return Ok(account_record(&found_id, &record));
        }
        let record = StoredAccount {
            username: normalized,
            display_name: display_name.to_string(),
            status: "active".to_string(),
            password_hash: None,
            created_at: Some(Utc::now()),
        };
        w.insert(*id, record.clone());
        Ok(account_record(id, &record))
    }

    async fn get_account(&self, id: &AccountId) -> Result<Option<AccountRecord>> {
        Ok(self
            .accounts
            .read()
            .unwrap()
            .get(id)
            .map(|a| account_record(id, a)))
    }

    async fn get_account_by_username(&self, username: &str) -> Result<Option<AccountRecord>> {
        let normalized = username.trim().to_ascii_lowercase();
        Ok(self
            .accounts
            .read()
            .unwrap()
            .iter()
            .find(|(_, a)| a.username == normalized)
            .map(|(id, a)| account_record(id, a)))
    }

    async fn password_hash_of(&self, username: &str) -> Result<Option<String>> {
        let normalized = username.trim().to_ascii_lowercase();
        Ok(self
            .accounts
            .read()
            .unwrap()
            .values()
            .find(|a| a.username == normalized)
            .and_then(|a| a.password_hash.clone()))
    }

    async fn set_account_password(&self, id: &AccountId, password_hash: &str) -> Result<()> {
        let mut w = self.accounts.write().unwrap();
        match w.get_mut(id) {
            Some(a) => {
                a.password_hash = Some(password_hash.to_string());
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn set_account_status(&self, id: &AccountId, status: &str) -> Result<()> {
        let mut w = self.accounts.write().unwrap();
        match w.get_mut(id) {
            Some(a) => {
                a.status = status.to_string();
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        let mut out: Vec<AccountRecord> = self
            .accounts
            .read()
            .unwrap()
            .iter()
            .map(|(id, a)| account_record(id, a))
            .collect();
        out.sort_by_key(|a| a.created_at);
        Ok(out)
    }

    async fn create_session(
        &self,
        token_hash: &[u8],
        account_id: &AccountId,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        self.sessions.write().unwrap().insert(
            token_hash.to_vec(),
            SessionRecord {
                account_id: *account_id,
                expires_at,
            },
        );
        Ok(())
    }

    async fn get_session(&self, token_hash: &[u8]) -> Result<Option<SessionRecord>> {
        Ok(self.sessions.read().unwrap().get(token_hash).cloned())
    }

    async fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        self.sessions.write().unwrap().remove(token_hash);
        Ok(())
    }

    async fn prune_sessions(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut w = self.sessions.write().unwrap();
        let before = w.len();
        w.retain(|_, s| s.expires_at > now);
        Ok(before - w.len())
    }
}

#[async_trait]
impl mem_core::infra::EvidenceStore for InMemoryHubStore {
    async fn append_evidence(&self, evidence: &NewEvidence) -> Result<MemoryEvidence> {
        let id = evidence.id.unwrap_or_else(Uuid::new_v4);
        let created_at = Utc::now();
        let ev = MemoryEvidence {
            id,
            namespace: evidence.namespace.clone(),
            source_kind: evidence.source_kind,
            source_ref: evidence.source_ref.clone(),
            content_hash: evidence.content_hash.clone(),
            excerpt: evidence.excerpt.clone(),
            occurred_at: evidence.occurred_at,
            created_at,
        };
        let mut w = self.evidences.write().unwrap();
        w.insert((evidence.namespace.clone(), id), ev.clone());
        Ok(ev)
    }

    async fn get_evidence(&self, namespace: &str, id: Uuid) -> Result<MemoryEvidence> {
        let r = self.evidences.read().unwrap();
        r.get(&(namespace.to_string(), id))
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn list_evidence(
        &self,
        namespace: &str,
        source_kind: Option<EvidenceSourceKind>,
        source_ref: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEvidence>> {
        let r = self.evidences.read().unwrap();
        let mut list: Vec<MemoryEvidence> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, ev)| ev.clone())
            .filter(|ev| {
                if let Some(kind) = source_kind {
                    if ev.source_kind != kind {
                        return false;
                    }
                }
                if let Some(r_ref) = source_ref {
                    if ev.source_ref.as_deref() != Some(r_ref) {
                        return false;
                    }
                }
                true
            })
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        list.truncate(limit.max(1));
        Ok(list)
    }

    async fn all_evidence_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryEvidence>> {
        let r = self.evidences.read().unwrap();
        let mut list: Vec<MemoryEvidence> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, ev)| ev.clone())
            .collect();
        list.sort_by_key(|e| (e.created_at, e.id));
        Ok(list)
    }

    async fn delete_evidence(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.evidences.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_none() {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl mem_core::infra::RelationStore for InMemoryHubStore {
    async fn add_relation(
        &self,
        namespace: &str,
        relation: &NewMemoryRelation,
    ) -> Result<MemoryRelation> {
        if relation.from_memory_id == relation.to_memory_id {
            return Err(HubError::InvalidArgument("自环关系不合法".to_string()));
        }
        let items = self.items.read().unwrap();
        let from_exists = items.contains_key(&(namespace.to_string(), relation.from_memory_id));
        let to_exists = items.contains_key(&(namespace.to_string(), relation.to_memory_id));
        if !from_exists || !to_exists {
            return Err(HubError::NotFound);
        }
        if let Some(ev_id) = relation.source_evidence_id {
            let evs = self.evidences.read().unwrap();
            if !evs.contains_key(&(namespace.to_string(), ev_id)) {
                return Err(HubError::NotFound);
            }
        }
        let created_at = Utc::now();
        let rel = MemoryRelation {
            namespace: Some(namespace.to_string()),
            from_memory_id: relation.from_memory_id,
            to_memory_id: relation.to_memory_id,
            relation_kind: relation.relation_kind,
            confidence: relation.confidence,
            source_evidence_id: relation.source_evidence_id,
            created_at,
        };
        let mut w = self.relations.write().unwrap();
        w.insert(
            (
                relation.from_memory_id,
                relation.to_memory_id,
                relation.relation_kind,
            ),
            (namespace.to_string(), rel.clone()),
        );
        Ok(rel)
    }

    async fn list_relations_from(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        let r = self.relations.read().unwrap();
        let mut list: Vec<MemoryRelation> = r
            .values()
            .filter(|(ns, rel)| ns == namespace && rel.from_memory_id == from_memory_id)
            .map(|(_, rel)| rel.clone())
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(list)
    }

    async fn list_relations_to(
        &self,
        namespace: &str,
        to_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        let r = self.relations.read().unwrap();
        let mut list: Vec<MemoryRelation> = r
            .values()
            .filter(|(ns, rel)| ns == namespace && rel.to_memory_id == to_memory_id)
            .map(|(_, rel)| rel.clone())
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(list)
    }

    async fn delete_relation(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
        to_memory_id: Uuid,
        relation_kind: MemoryRelationKind,
    ) -> Result<()> {
        let mut w = self.relations.write().unwrap();
        let key = (from_memory_id, to_memory_id, relation_kind);
        if let Some((ns, _)) = w.get(&key) {
            if ns == namespace {
                w.remove(&key);
                return Ok(());
            }
        }
        Err(HubError::NotFound)
    }

    async fn all_relations_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryRelation>> {
        let r = self.relations.read().unwrap();
        let mut list: Vec<MemoryRelation> = r
            .values()
            .filter(|(ns, _)| ns == namespace)
            .map(|(_, rel)| rel.clone())
            .collect();
        list.sort_by_key(|rel| (rel.created_at, rel.from_memory_id, rel.to_memory_id));
        Ok(list)
    }
}

#[async_trait]
impl mem_core::infra::TaskStore for InMemoryHubStore {
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
        let mut w = self.tasks.write().unwrap();
        w.insert((task.namespace.clone(), id), record.clone());
        Ok(record)
    }

    async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
        let mut w = self.tasks.write().unwrap();
        let key = (task.namespace.clone(), task.id);
        if !w.contains_key(&key) {
            return Err(HubError::NotFound);
        }
        w.insert(key, task.clone());
        Ok(task.clone())
    }

    async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
        let r = self.tasks.read().unwrap();
        Ok(r.get(&(namespace.to_string(), id)).cloned())
    }

    async fn list_tasks(
        &self,
        namespace: &str,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> Result<Vec<MemoryTask>> {
        let r = self.tasks.read().unwrap();
        let mut list: Vec<MemoryTask> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, t)| t.clone())
            .filter(|t| {
                if let Some(s) = status {
                    t.status == s
                } else {
                    true
                }
            })
            .collect();
        list.sort_by_key(|t| std::cmp::Reverse(t.started_at));
        list.truncate(limit.max(1));
        Ok(list)
    }

    async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
        let r = self.tasks.read().unwrap();
        let mut list: Vec<MemoryTask> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, t)| t.clone())
            .collect();
        list.sort_by_key(|t| (t.started_at, t.id));
        Ok(list)
    }

    async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.tasks.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_some() {
            let mut te = self.task_experiences.write().unwrap();
            te.retain(|(_, l)| l.task_id != id);
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }
}

#[async_trait]
impl mem_core::infra::ExperienceStore for InMemoryHubStore {
    async fn save_experience(&self, exp: &NewMemoryExperience) -> Result<MemoryExperience> {
        let id = exp.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let record = MemoryExperience {
            id,
            namespace: exp.namespace.clone(),
            task: exp.task.clone(),
            search_keys: exp.search_keys.clone(),
            solution: exp.solution.clone(),
            preconditions: exp.preconditions.clone(),
            boundary_envs: exp.boundary_envs.clone(),
            failure_modes: exp.failure_modes.clone(),
            validation: exp.validation.clone(),
            confidence: exp.confidence,
            status: exp.status,
            feedback_score: exp.feedback_score,
            created_at: now,
            updated_at: now,
        };
        let mut w = self.experiences.write().unwrap();
        w.insert((exp.namespace.clone(), id), record.clone());
        Ok(record)
    }

    async fn update_experience(&self, exp: &MemoryExperience) -> Result<MemoryExperience> {
        let mut w = self.experiences.write().unwrap();
        let key = (exp.namespace.clone(), exp.id);
        if !w.contains_key(&key) {
            return Err(HubError::NotFound);
        }
        let mut updated = exp.clone();
        updated.updated_at = Utc::now();
        w.insert(key, updated.clone());
        Ok(updated)
    }

    async fn get_experience(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryExperience>> {
        let r = self.experiences.read().unwrap();
        Ok(r.get(&(namespace.to_string(), id)).cloned())
    }

    async fn list_experiences(
        &self,
        namespace: &str,
        status: Option<ExperienceStatus>,
        limit: usize,
    ) -> Result<Vec<MemoryExperience>> {
        let r = self.experiences.read().unwrap();
        let mut list: Vec<MemoryExperience> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, e)| e.clone())
            .filter(|e| {
                if let Some(s) = status {
                    e.status == s
                } else {
                    true
                }
            })
            .collect();
        list.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
        list.truncate(limit.max(1));
        Ok(list)
    }

    async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
        let tasks = self.tasks.read().unwrap();
        let experiences = self.experiences.read().unwrap();

        let task_ns = tasks
            .iter()
            .find(|((_, tid), _)| *tid == link.task_id)
            .map(|((ns, _), _)| ns.clone())
            .ok_or(HubError::NotFound)?;

        let exp_ns = experiences
            .iter()
            .find(|((_, eid), _)| *eid == link.experience_id)
            .map(|((ns, _), _)| ns.clone())
            .ok_or(HubError::NotFound)?;

        if task_ns != exp_ns {
            return Err(HubError::InvalidArgument(
                "任务与经验不属于同一命名空间".into(),
            ));
        }

        if let Some(ref link_ns) = link.namespace {
            if *link_ns != task_ns {
                return Err(HubError::InvalidArgument(
                    "指定命名空间与任务所属命名空间不一致".into(),
                ));
            }
        }

        let mut w = self.task_experiences.write().unwrap();
        if !w.iter().any(|(_, l)| {
            l.task_id == link.task_id
                && l.experience_id == link.experience_id
                && l.relation_kind == link.relation_kind
        }) {
            let mut stored = link.clone();
            stored.namespace = Some(task_ns.clone());
            w.push((task_ns, stored));
        }
        Ok(())
    }

    async fn list_experiences_for_task(&self, task_id: Uuid) -> Result<Vec<MemoryTaskExperience>> {
        let r = self.task_experiences.read().unwrap();
        let list: Vec<MemoryTaskExperience> = r
            .iter()
            .filter(|(_, l)| l.task_id == task_id)
            .map(|(_, l)| l.clone())
            .collect();
        Ok(list)
    }

    async fn all_experiences_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryExperience>> {
        let r = self.experiences.read().unwrap();
        let mut list: Vec<MemoryExperience> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, e)| e.clone())
            .collect();
        list.sort_by_key(|e| (e.created_at, e.id));
        Ok(list)
    }

    async fn all_task_experiences_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<MemoryTaskExperience>> {
        let r = self.task_experiences.read().unwrap();
        let list: Vec<MemoryTaskExperience> = r
            .iter()
            .filter(|(ns, _)| ns == namespace)
            .map(|(_, l)| l.clone())
            .collect();
        Ok(list)
    }

    async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.experiences.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_some() {
            let mut te = self.task_experiences.write().unwrap();
            te.retain(|(_, l)| l.experience_id != id);
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }
}

#[async_trait]
impl CodingStore for InMemoryHubStore {
    async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
        if let Some(task_id) = thread.task_id {
            let tasks = self.tasks.read().unwrap();
            if !tasks.contains_key(&(thread.namespace.clone(), task_id)) {
                return Err(HubError::InvalidArgument(
                    "关联任务不存在于该命名空间".into(),
                ));
            }
        }

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
        if let Some(tid) = revision.thread_id {
            let threads = self.coding_threads.read().unwrap();
            if !threads.contains_key(&(revision.namespace.clone(), tid)) {
                return Err(HubError::InvalidArgument(
                    "关联线程不存在于该命名空间".into(),
                ));
            }
        }
        if let Some(task_id) = revision.source_task_id {
            let tasks = self.tasks.read().unwrap();
            if !tasks.contains_key(&(revision.namespace.clone(), task_id)) {
                return Err(HubError::InvalidArgument(
                    "关联任务不存在于该命名空间".into(),
                ));
            }
        }
        if let Some(ev_id) = revision.source_evidence_id {
            let evs = self.evidences.read().unwrap();
            if !evs.contains_key(&(revision.namespace.clone(), ev_id)) {
                return Err(HubError::InvalidArgument(
                    "关联证据不存在于该命名空间".into(),
                ));
            }
        }

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
        Ok(r.values()
            .filter(|t| t.namespace == namespace)
            .cloned()
            .collect())
    }

    async fn all_revisions_in_namespace(&self, namespace: &str) -> Result<Vec<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        Ok(r.values()
            .filter(|rev| rev.namespace == namespace)
            .cloned()
            .collect())
    }

    async fn delete_thread(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.coding_threads.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_some() {
            let mut revs = self.coding_revisions.write().unwrap();
            revs.retain(|_, r| r.thread_id != Some(id));
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }

    async fn delete_revision(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.coding_revisions.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_some() {
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }
}

/// 便捷统计（运维排障）
pub fn stats_of(store: &InMemoryHubStore) -> MemoryStats {
    let items = store.items.read().unwrap();
    let mut stats = MemoryStats {
        namespace: "*".to_string(),
        ..Default::default()
    };
    for e in items.values() {
        match e.status {
            MemoryStatus::Active => stats.active += 1,
            MemoryStatus::Archived => stats.archived += 1,
            MemoryStatus::Merged => stats.merged += 1,
        }
        *stats
            .by_kind
            .entry(e.kind.as_str().to_string())
            .or_insert(0) += 1;
    }
    stats.total = stats.active + stats.archived + stats.merged;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::{
        AccountStore, BackupStore, EvidenceStore, GrantStore, IntegrationStore, MemoryHubStore,
        MemoryKind, MemoryScene, MemoryStore, RelationStore,
    };

    fn entry(ns: &str, content: &str, key: Option<&str>) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            key: key.map(str::to_string),
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "agent-a".to_string(),
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
        }
    }

    #[tokio::test]
    async fn test_namespace_isolation() {
        let store = InMemoryHubStore::new();
        let a = entry("ns-a", "A 的记忆", None);
        store.insert(&a).await.unwrap();

        // 命名空间不对 → NotFound
        assert!(store.get("ns-b", a.id).await.is_err());
        // 命名空间对 → 命中
        assert!(store.get("ns-a", a.id).await.is_ok());
    }

    #[tokio::test]
    async fn test_key_lookup_and_conflict() {
        let store = InMemoryHubStore::new();
        let a = entry("ns", "第一条", Some("k:1"));
        store.insert(&a).await.unwrap();

        assert!(store.get_by_key("ns", "k:1").await.unwrap().is_some());

        // 同 id 重复插入 → Conflict
        let err = store.insert(&a).await.unwrap_err();
        assert!(matches!(err, HubError::Conflict));
    }

    #[tokio::test]
    async fn test_cursor_pagination_and_limit() {
        let store = InMemoryHubStore::new();
        let mut ids = Vec::new();
        for i in 0..5 {
            let mut e = entry("ns", &format!("条目 {}", i), None);
            e.updated_at = Utc::now() + chrono::Duration::seconds(i);
            ids.push(e.id);
            store.insert(&e).await.unwrap();
        }

        // 第一页：取最新 2 条
        let page1 = store
            .list(&MemoryFilter {
                namespace: Some("ns".to_string()),
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        // 最新在前
        assert!(page1[0].updated_at >= page1[1].updated_at);

        // 第二页：严格在游标之前
        let cursor_ts = page1[1].updated_at;
        let cursor_id = page1[1].id;
        let page2 = store
            .list(&MemoryFilter {
                namespace: Some("ns".to_string()),
                limit: Some(2),
                before: Some((cursor_ts, cursor_id)),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page2.len(), 2);
        assert!(page2.iter().all(|e| e.id != cursor_id));
    }

    #[tokio::test]
    async fn test_similar_filters_status_and_dim() {
        let store = InMemoryHubStore::new();
        let mut e = entry("ns", "有向量的条目", None);
        e.embedding = Some(vec![1.0, 0.0]);
        store.insert(&e).await.unwrap();

        let hit = store
            .similar(
                &MemoryFilter {
                    namespace: Some("ns".to_string()),
                    ..Default::default()
                },
                &[1.0, 0.0],
                5,
            )
            .await
            .unwrap();
        assert_eq!(hit.len(), 1);
        assert!((hit[0].score - 1.0).abs() < 1e-6);

        // 维度不匹配 → 无结果
        let miss = store
            .similar(
                &MemoryFilter {
                    namespace: Some("ns".to_string()),
                    ..Default::default()
                },
                &[1.0, 0.0, 0.0],
                5,
            )
            .await
            .unwrap();
        assert!(miss.is_empty());

        // 归档后不再召回
        e.status = MemoryStatus::Archived;
        store.save(&e).await.unwrap();
        let empty = store
            .similar(
                &MemoryFilter {
                    namespace: Some("ns".to_string()),
                    ..Default::default()
                },
                &[1.0, 0.0],
                5,
            )
            .await
            .unwrap();
        assert!(empty.is_empty());
    }

    /// 回归：向量召回曾把 scene / valid_at 过滤留在 Rust 侧，
    /// 过滤条件下推后，被过滤条目不得占据 top-k 名额
    #[tokio::test]
    async fn test_similar_pushes_down_scene_and_validity_filters() {
        use chrono::TimeZone;
        let store = InMemoryHubStore::new();

        // 同主题、不同场景的三条：只有 Coding 场景那条符合查询
        let mut chat = entry("ns", "代码重构方案 A", None);
        chat.scene = Some(MemoryScene::Chat);
        chat.embedding = Some(vec![1.0, 0.0]);
        let mut coding = entry("ns", "代码重构方案 B", None);
        coding.scene = Some(MemoryScene::Coding);
        coding.embedding = Some(vec![1.0, 0.0]);
        let mut expired = entry("ns", "代码重构方案 C", None);
        expired.scene = Some(MemoryScene::Coding);
        expired.embedding = Some(vec![1.0, 0.0]);
        expired.valid_until = Some(chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());

        for e in [&chat, &coding, &expired] {
            store.insert(e).await.unwrap();
        }

        let filter = MemoryFilter {
            namespace: Some("ns".to_string()),
            statuses: Some(vec![MemoryStatus::Active]),
            scene: Some(MemoryScene::Coding),
            valid_at: Some(chrono::Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()),
            ..Default::default()
        };
        let hits = store.similar(&filter, &[1.0, 0.0], 10).await.unwrap();
        assert_eq!(
            hits.len(),
            1,
            "下推过滤必须只返回满足 scene/valid_at 的条目"
        );
        assert_eq!(hits[0].item.content, "代码重构方案 B");
    }

    /// 回归：FTS 词法曾用整段查询做裸子串匹配，与 PostgreSQL 分词语义不同，
    /// 且 AND 语义下多词查询会整条遗漏
    #[tokio::test]
    async fn test_list_fts_partial_match_and_relevance_order() {
        let store = InMemoryHubStore::new();
        // 精确命中两个词元的旧条目，与只命中一个词元的新条目
        let mut both = entry("ns", "Redis 缓存集群主节点说明", None);
        both.updated_at = chrono::Utc::now() - chrono::Duration::days(30);
        let mut one = entry("ns", "缓存预热脚本", None);
        one.updated_at = chrono::Utc::now();
        store.insert(&both).await.unwrap();
        store.insert(&one).await.unwrap();

        let filter = MemoryFilter {
            namespace: Some("ns".to_string()),
            text_contains: Some("Redis 缓存集群".to_string()),
            limit: Some(10),
            ..Default::default()
        };
        let rows = store.list(&filter).await.unwrap();

        assert_eq!(
            rows.len(),
            2,
            "部分匹配（OR 语义）应召回只命中一个词元的文档"
        );
        assert_eq!(
            rows[0].content, "Redis 缓存集群主节点说明",
            "命中词元更多的文档必须排在前（相关度优先于更新时间）"
        );
    }

    #[tokio::test]
    async fn test_versions_idempotent_and_sorted() {
        let store = InMemoryHubStore::new();
        let e = entry("ns", "内容", None);
        store.insert(&e).await.unwrap();

        let mk = |v: i64| MemoryVersion {
            memory_id: e.id,
            version: v,
            content: format!("v{}", v),
            tags: vec![],
            metadata: serde_json::json!({}),
            confidence: 1.0,
            changed_by: "agent-a".to_string(),
            change_type: VersionChange::Updated,
            changed_at: Utc::now(),
            scene: None,
            layer: None,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        };
        store.record_version(&mk(1)).await.unwrap();
        store.record_version(&mk(2)).await.unwrap();
        // 重放 v1 不得重复入账
        store.record_version(&mk(1)).await.unwrap();

        let vs = store.versions(e.id).await.unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0].version, 1);
        assert_eq!(vs[1].version, 2);
    }

    #[tokio::test]
    async fn test_archive_expired() {
        let store = InMemoryHubStore::new();
        let mut e = entry("ns", "将过期", None);
        e.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.insert(&e).await.unwrap();

        let archived = store.archive_expired(Utc::now()).await.unwrap();
        assert_eq!(archived, 1);
        assert_eq!(
            store.get("ns", e.id).await.unwrap().status,
            MemoryStatus::Archived
        );
        // 版本历史记录了归档事件
        let vs = store.versions(e.id).await.unwrap();
        assert!(vs.iter().any(|v| v.change_type == VersionChange::Archived));
    }

    #[tokio::test]
    async fn test_namespace_grants_and_wildcard_disabled() {
        let store = InMemoryHubStore::new();
        store
            .ensure_namespace("ns", &"agent-a".to_string())
            .await
            .unwrap();

        // owner 尚未被授权：不可读写
        assert!(!store.can_read("ns", &"agent-a".to_string()).await.unwrap());

        store
            .grant(&NamespaceGrantRecord {
                namespace: "ns".to_string(),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        assert!(store.can_read("ns", &"agent-a".to_string()).await.unwrap());
        assert!(store.can_write("ns", &"agent-a".to_string()).await.unwrap());

        // 沙箱语义：`*` 通配授权行不再对任何命名空间生效
        store
            .grant(&NamespaceGrantRecord {
                namespace: "*".to_string(),
                agent_id: "agent-b".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        assert!(!store
            .can_read("any-ns", &"agent-b".to_string())
            .await
            .unwrap());
        assert!(!store.can_write("ns", &"agent-b".to_string()).await.unwrap());

        // 撤销后失效
        store.revoke("ns", &"agent-a".to_string()).await.unwrap();
        assert!(!store.can_write("ns", &"agent-a".to_string()).await.unwrap());

        let owned = store
            .owned_namespaces(&"agent-a".to_string())
            .await
            .unwrap();
        assert_eq!(owned, vec!["ns".to_string()]);
    }

    #[tokio::test]
    async fn test_accounts_and_sessions_roundtrip() {
        let store = InMemoryHubStore::new();

        let acct = store
            .create_account("Alice", Some("hash-a"), "爱丽丝")
            .await
            .unwrap();
        assert_eq!(acct.username, "alice", "用户名归一化为小写");
        assert!(acct.has_password);

        // 用户名冲突（大小写不敏感）
        assert!(store.create_account("ALICE", Some("x"), "").await.is_err());

        // 按用户名查哈希（认证路径）
        assert_eq!(
            store.password_hash_of("aLiCe").await.unwrap().as_deref(),
            Some("hash-a")
        );

        // 引导账户幂等（按固定 ID）
        let boot1 = store
            .ensure_account(&common::BOOTSTRAP_ACCOUNT_ID, "bootstrap", "引导账户")
            .await
            .unwrap();
        let boot2 = store
            .ensure_account(&common::BOOTSTRAP_ACCOUNT_ID, "bootstrap", "引导账户")
            .await
            .unwrap();
        assert_eq!(boot1.id, boot2.id);
        assert!(!boot1.has_password, "引导账户初始无口令");

        // 会话：创建 → 读取 → 过期清理 → 删除
        let hash = vec![7u8; 32];
        let future = Utc::now() + chrono::Duration::hours(1);
        store.create_session(&hash, &acct.id, future).await.unwrap();
        assert_eq!(
            store.get_session(&hash).await.unwrap().unwrap().account_id,
            acct.id
        );
        assert_eq!(store.prune_sessions(Utc::now()).await.unwrap(), 0);
        assert_eq!(
            store
                .prune_sessions(future + chrono::Duration::seconds(1))
                .await
                .unwrap(),
            1
        );
        assert!(store.get_session(&hash).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_backup_and_proposal_roundtrip() {
        let store = InMemoryHubStore::new();
        let backup = BackupRecord {
            id: Uuid::new_v4(),
            namespace: "ns".to_string(),
            created_by: "agent-a".to_string(),
            note: None,
            entry_count: 0,
            checksum: "abc".to_string(),
            payload: serde_json::json!({}),
            retain_until: None,
            created_at: Utc::now(),
        };
        store.save_backup(&backup).await.unwrap();
        assert_eq!(store.list_backups(None).await.unwrap().len(), 1);
        assert_eq!(store.list_backups(Some("ns")).await.unwrap().len(), 1);
        assert_eq!(store.list_backups(Some("other")).await.unwrap().len(), 0);
        store.delete_backup(backup.id).await.unwrap();
        assert!(store.list_backups(None).await.unwrap().is_empty());

        let proposal = IntegrationProposal {
            id: Uuid::new_v4(),
            namespace: "ns".to_string(),
            kind: mem_core::IntegrationKind::Duplicate,
            member_ids: vec![Uuid::new_v4(), Uuid::new_v4()],
            suggested_survivor: None,
            similarity: 0.99,
            status: ProposalStatus::Open,
            resolution_note: None,
            created_at: Utc::now(),
            resolved_at: None,
        };
        store.save_proposal(&proposal).await.unwrap();
        assert!(store
            .open_exists(
                "ns",
                &[("duplicate".to_string(), proposal.member_ids.clone())]
            )
            .await
            .unwrap());
        assert_eq!(
            store
                .list_proposals("ns", Some(ProposalStatus::Open))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_hub_store_aggregation() {
        // 编译期即验证：InMemoryHubStore 实现全部七个 trait
        fn assert_impl<T: MemoryHubStore>() {}
        assert_impl::<InMemoryHubStore>();
    }

    #[tokio::test]
    async fn test_evidence_store_crud() {
        let store = InMemoryHubStore::new();
        let ev1 = store
            .append_evidence(&NewEvidence {
                id: None,
                namespace: "ns".to_string(),
                source_kind: EvidenceSourceKind::Session,
                source_ref: Some("sess-1".to_string()),
                content_hash: "hash-1".to_string(),
                excerpt: Some("对话片段1".to_string()),
                occurred_at: Some(Utc::now()),
            })
            .await
            .unwrap();

        let ev2 = store
            .append_evidence(&NewEvidence {
                id: None,
                namespace: "ns".to_string(),
                source_kind: EvidenceSourceKind::GitCommit,
                source_ref: Some("sha-abc".to_string()),
                content_hash: "hash-2".to_string(),
                excerpt: Some("commit diff".to_string()),
                occurred_at: Some(Utc::now()),
            })
            .await
            .unwrap();

        // get_evidence
        let fetched = store.get_evidence("ns", ev1.id).await.unwrap();
        assert_eq!(fetched.id, ev1.id);
        assert_eq!(fetched.source_kind, EvidenceSourceKind::Session);

        // list_evidence with kind filter
        let sessions = store
            .list_evidence("ns", Some(EvidenceSourceKind::Session), None, 10)
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, ev1.id);

        let commits = store
            .list_evidence("ns", Some(EvidenceSourceKind::GitCommit), None, 10)
            .await
            .unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].id, ev2.id);

        // all_evidence_in_namespace
        let all = store.all_evidence_in_namespace("ns").await.unwrap();
        assert_eq!(all.len(), 2);

        // delete_evidence
        store.delete_evidence("ns", ev1.id).await.unwrap();
        assert!(store.get_evidence("ns", ev1.id).await.is_err());
        assert_eq!(
            store.all_evidence_in_namespace("ns").await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn test_relation_store_semantics() {
        let store = InMemoryHubStore::new();
        let m1 = entry("ns-a", "条目1", None);
        let m2 = entry("ns-a", "条目2", None);
        let m_diff_ns = entry("ns-b", "异命名空间条目", None);
        store.insert(&m1).await.unwrap();
        store.insert(&m2).await.unwrap();
        store.insert(&m_diff_ns).await.unwrap();

        // 1. 自环拒绝
        let loop_err = store
            .add_relation(
                "ns-a",
                &NewMemoryRelation {
                    from_memory_id: m1.id,
                    to_memory_id: m1.id,
                    relation_kind: MemoryRelationKind::Supports,
                    confidence: 1.0,
                    source_evidence_id: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(loop_err, HubError::InvalidArgument(_)));

        // 2. 跨命名空间关系拒绝
        let cross_err = store
            .add_relation(
                "ns-a",
                &NewMemoryRelation {
                    from_memory_id: m1.id,
                    to_memory_id: m_diff_ns.id,
                    relation_kind: MemoryRelationKind::Supports,
                    confidence: 1.0,
                    source_evidence_id: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(cross_err, HubError::NotFound));

        // 3. 正常添加并双向查询
        let rel = store
            .add_relation(
                "ns-a",
                &NewMemoryRelation {
                    from_memory_id: m1.id,
                    to_memory_id: m2.id,
                    relation_kind: MemoryRelationKind::Supports,
                    confidence: 0.95,
                    source_evidence_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(rel.confidence, 0.95);

        let from = store.list_relations_from("ns-a", m1.id).await.unwrap();
        assert_eq!(from.len(), 1);
        assert_eq!(from[0].to_memory_id, m2.id);

        let to = store.list_relations_to("ns-a", m2.id).await.unwrap();
        assert_eq!(to.len(), 1);
        assert_eq!(to[0].from_memory_id, m1.id);

        // 4. 删除条目引发关联关系级联删除
        store.delete("ns-a", m1.id).await.unwrap();
        assert!(store
            .list_relations_from("ns-a", m1.id)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .list_relations_to("ns-a", m2.id)
            .await
            .unwrap()
            .is_empty());
    }
}
