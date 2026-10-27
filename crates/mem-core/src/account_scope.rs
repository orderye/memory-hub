//! 账户沙箱作用域：命名空间在存储层按 `{account_id}/{namespace}` 隔离
//!
//! 服务层与 MCP 工具看到的一直是"用户可见"的命名空间名（如 `personal`）；
//! 所有落库/查库的键都携带账户前缀（如 `00000000-…-0001/personal`）。
//! 沙箱由两条不变量保证：
//!
//! 1. 前缀拼接是**单射**：一个账户的令牌永远拼不出另一个账户的存储键
//!    （用户命名空间不允许出现 `/`，见 `access::validate_namespace`）；
//! 2. 授权表内不存在跨账户行：命名空间 owner 与授权目标都取自同账户 Agent。
//!
//! `AccountScopedStore` 是这两条不变量在代码上的唯一落点：服务层为每次
//! 请求构造它，全部数据访问经其代理。前缀拼接对"已带前缀"的名称是幂等的，
//! 因此备份负载（内部存用户可见名）恢复往返不会二次加前缀。

use crate::domain::{
    BackupRecord, CodingRevision, CodingThread, CognitiveProposalRecord, CognitiveProposalStatus,
    CollectionBinding, EvidenceSourceKind, ExperienceStatus, ExperienceVersion,
    IntegrationProposal, MemoryCollection, MemoryEntry, MemoryEvidence, MemoryExperience,
    MemoryFilter, MemoryRelation, MemoryRelationKind, MemoryTask, MemoryTaskExperience,
    MemoryVersion, Namespace, NamespaceGrantRecord, NamespaceInfo, NewCodingRevision,
    NewCodingThread, NewEvidence, NewExperienceVersion, NewMemoryExperience, NewMemoryRelation,
    NewMemoryTask, ProposalStatus, Scored, TaskStatus,
};
use crate::infra::{
    AgentRecord, AgentRegistration, AgentStore, BackupStore, CodingStore, CognitiveProposalStore,
    CollectionStore, EvidenceStore, ExperienceStore, GrantStore, IntegrationStore, MemoryStore,
    RelationStore, TaskStore,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, HubError, Result};
use std::sync::Arc;
use uuid::Uuid;

/// 账户前缀分隔符。用户命名空间不允许 `/`（校验器拒绝），
/// 因此含 `/` 的内部键永远是引擎拼接产物，不可能与用户自造名冲突。
pub const SCOPE_SEPARATOR: char = '/';

/// 组合内部存储键：`{account}/{namespace}`
pub fn scope_name(account: &AccountId, namespace: &str) -> String {
    format!("{}{}{}", account, SCOPE_SEPARATOR, namespace)
}

/// 剥离账户前缀（不属于该账户或本就无前缀时原样返回）
pub fn unscope_name(account: &AccountId, name: &str) -> String {
    let prefix = format!("{}{}", account, SCOPE_SEPARATOR);
    name.strip_prefix(&prefix).unwrap_or(name).to_string()
}

/// 存储键是否属于该账户
pub fn in_scope(account: &AccountId, name: &str) -> bool {
    let prefix = format!("{}{}", account, SCOPE_SEPARATOR);
    name.starts_with(&prefix)
}

/// 幂等作用域化：已带本账户前缀则原样返回
fn scope_idempotent(account: &AccountId, name: &str) -> String {
    if in_scope(account, name) {
        name.to_string()
    } else {
        scope_name(account, name)
    }
}

/// 账户作用域存储包装器：输入命名空间加账户前缀，输出剥离前缀，
/// 集合查询只返回本账户数据。系统级维护操作（worker）请使用原始存储。
///
/// 泛型参数 `S` 允许包装任意粒度的存储抽象：
/// `AccountScopedStore<dyn MemoryHubStore>`（备份/整合服务）或
/// `AccountScopedStore<dyn MemoryStore>`（记忆服务）。
pub struct AccountScopedStore<S: ?Sized> {
    inner: Arc<S>,
    account: AccountId,
}

impl<S: ?Sized> AccountScopedStore<S> {
    pub fn new(inner: Arc<S>, account: AccountId) -> Self {
        Self { inner, account }
    }

    /// 输入侧：用户可见名 → 存储键（幂等）
    fn scoped(&self, ns: &str) -> String {
        scope_idempotent(&self.account, ns)
    }

    /// 输出侧：存储键 → 用户可见名
    fn unscoped(&self, ns: &str) -> String {
        unscope_name(&self.account, ns)
    }

    /// 按主键（UUID）取回的记录必须先验归属：`unscoped` 对异账户键是"原样返回"的
    /// 宽松语义，若直接透传会把他人数据连同其明文命名空间一起交给调用方，
    /// 进而被 restore 写回本账户。此处在剥前缀之前 fail-closed 地拒绝跨账户读取。
    fn assert_owned(&self, raw_namespace: &str) -> Result<()> {
        if in_scope(&self.account, raw_namespace) {
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }

    /// 账户级配置（集合/绑定）的装配校验：参数账户必须等于本包装器的账户。
    /// 跨账户操作一律拒绝，不依赖调用方自觉。
    fn ensure_account(&self, account_id: AccountId) -> Result<()> {
        if account_id == self.account {
            Ok(())
        } else {
            Err(HubError::InsufficientScope)
        }
    }

    fn entry_out(&self, mut e: MemoryEntry) -> MemoryEntry {
        e.namespace = self.unscoped(&e.namespace);
        e
    }

    fn scoped_filter(&self, filter: &MemoryFilter) -> MemoryFilter {
        let mut f = filter.clone();
        f.namespace = f.namespace.as_deref().map(|ns| self.scoped(ns));
        f
    }

    fn namespace_info_out(&self, mut info: NamespaceInfo) -> NamespaceInfo {
        info.namespace = self.unscoped(&info.namespace);
        info
    }
}

// ===== MemoryStore =====

#[async_trait]
impl<S: MemoryStore + ?Sized> MemoryStore for AccountScopedStore<S> {
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut scoped = entry.clone();
        scoped.namespace = self.scoped(&entry.namespace);
        let saved = self.inner.insert(&scoped).await?;
        Ok(self.entry_out(saved))
    }

    async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut scoped = entry.clone();
        scoped.namespace = self.scoped(&entry.namespace);
        let saved = self.inner.insert_initial(&scoped).await?;
        Ok(self.entry_out(saved))
    }

    async fn get(&self, namespace: &str, id: Uuid) -> Result<MemoryEntry> {
        let e = self.inner.get(&self.scoped(namespace), id).await?;
        Ok(self.entry_out(e))
    }

    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let e = self.inner.get_by_key(&self.scoped(namespace), key).await?;
        Ok(e.map(|e| self.entry_out(e)))
    }

    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut scoped = entry.clone();
        scoped.namespace = self.scoped(&entry.namespace);
        let saved = self.inner.save(&scoped).await?;
        Ok(self.entry_out(saved))
    }

    async fn delete(&self, namespace: &str, id: Uuid) -> Result<()> {
        self.inner.delete(&self.scoped(namespace), id).await
    }

    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
        let rows = self.inner.list(&self.scoped_filter(filter)).await?;
        Ok(rows.into_iter().map(|e| self.entry_out(e)).collect())
    }

    async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
        self.inner.count(&self.scoped_filter(filter)).await
    }

    async fn save_if_version(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
    ) -> Result<MemoryEntry> {
        let mut scoped = entry.clone();
        scoped.namespace = self.scoped(&entry.namespace);
        let saved = self
            .inner
            .save_if_version(&scoped, expected_version)
            .await?;
        Ok(self.entry_out(saved))
    }

    async fn revise(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
        change: crate::domain::VersionChange,
    ) -> Result<MemoryEntry> {
        let mut scoped = entry.clone();
        scoped.namespace = self.scoped(&entry.namespace);
        let saved = self.inner.revise(&scoped, expected_version, change).await?;
        Ok(self.entry_out(saved))
    }

    async fn similar(
        &self,
        filter: &MemoryFilter,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        let rows = self
            .inner
            .similar(&self.scoped_filter(filter), embedding, k)
            .await?;
        Ok(rows
            .into_iter()
            .map(|s| Scored {
                item: self.entry_out(s.item),
                score: s.score,
            })
            .collect())
    }

    async fn embeddings_by_ids(
        &self,
        namespace: &str,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, Vec<f32>)>> {
        self.inner
            .embeddings_by_ids(&self.scoped(namespace), ids)
            .await
    }

    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>> {
        let rows = self
            .inner
            .all_in_namespace(&self.scoped(namespace), include_merged)
            .await?;
        Ok(rows.into_iter().map(|e| self.entry_out(e)).collect())
    }

    async fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let all = self.inner.list_namespaces().await?;
        let mut out: Vec<Namespace> = all
            .into_iter()
            .filter(|ns| in_scope(&self.account, ns))
            .map(|ns| self.unscoped(&ns))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
        // 版本历史按 memory_id 关联，无命名空间字段，直接透传
        self.inner.record_version(v).await
    }

    async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
        self.inner.versions(id).await
    }

    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize> {
        // 系统级批处理（worker 用原始存储调用）；经本包装器调用会作用于全库，
        // 服务层不得依赖此方法做账户内归档。
        self.inner.archive_expired(now).await
    }

    async fn ping(&self) -> Result<()> {
        self.inner.ping().await
    }
}

// ===== BackupStore =====

#[async_trait]
impl<S: BackupStore + ?Sized> BackupStore for AccountScopedStore<S> {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()> {
        let mut scoped = backup.clone();
        scoped.namespace = self.scoped(&backup.namespace);
        self.inner.save_backup(&scoped).await
    }

    async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
        let b = self.inner.get_backup(id).await?;
        self.assert_owned(&b.namespace)?;
        Ok(BackupRecord {
            namespace: self.unscoped(&b.namespace),
            ..b
        })
    }
    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>> {
        let rows = match namespace {
            Some(ns) => self.inner.list_backups(Some(&self.scoped(ns))).await?,
            None => {
                let all = self.inner.list_backups(None).await?;
                all.into_iter()
                    .filter(|b| in_scope(&self.account, &b.namespace))
                    .collect()
            }
        };
        Ok(rows
            .into_iter()
            .map(|mut b| {
                b.namespace = self.unscoped(&b.namespace);
                b
            })
            .collect())
    }

    async fn delete_backup(&self, id: Uuid) -> Result<()> {
        // 备份按 id 删除，保留策略由 worker 用原始存储执行；服务层调用前
        // 必须先经 get_backup 校验归属，故此处透传。
        self.inner.delete_backup(id).await
    }
}

// ===== IntegrationStore =====

#[async_trait]
impl<S: IntegrationStore + ?Sized> IntegrationStore for AccountScopedStore<S> {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
        let mut scoped = p.clone();
        scoped.namespace = self.scoped(&p.namespace);
        self.inner.save_proposal(&scoped).await
    }

    async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
        let p = self.inner.get_proposal(id).await?;
        self.assert_owned(&p.namespace)?;
        Ok(IntegrationProposal {
            namespace: self.unscoped(&p.namespace),
            ..p
        })
    }

    async fn list_proposals(
        &self,
        namespace: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>> {
        let rows = self
            .inner
            .list_proposals(&self.scoped(namespace), status)
            .await?;
        Ok(rows
            .into_iter()
            .map(|mut p| {
                p.namespace = self.unscoped(&p.namespace);
                p
            })
            .collect())
    }

    async fn open_exists(
        &self,
        namespace: &str,
        kind_member: &[(String, Vec<Uuid>)],
    ) -> Result<bool> {
        self.inner
            .open_exists(&self.scoped(namespace), kind_member)
            .await
    }
}

// ===== CognitiveProposalStore =====

#[async_trait]
impl<S: CognitiveProposalStore + ?Sized> CognitiveProposalStore for AccountScopedStore<S> {
    async fn save_cognitive_proposal(&self, p: &CognitiveProposalRecord) -> Result<()> {
        let mut scoped = p.clone();
        scoped.namespace = self.scoped(&p.namespace);
        self.inner.save_cognitive_proposal(&scoped).await
    }

    async fn get_cognitive_proposal(&self, id: Uuid) -> Result<CognitiveProposalRecord> {
        let p = self.inner.get_cognitive_proposal(id).await?;
        self.assert_owned(&p.namespace)?;
        Ok(CognitiveProposalRecord {
            namespace: self.unscoped(&p.namespace),
            ..p
        })
    }

    async fn list_cognitive_proposals(
        &self,
        namespace: &str,
        status: Option<CognitiveProposalStatus>,
    ) -> Result<Vec<CognitiveProposalRecord>> {
        let rows = self
            .inner
            .list_cognitive_proposals(&self.scoped(namespace), status)
            .await?;
        Ok(rows
            .into_iter()
            .map(|mut p| {
                p.namespace = self.unscoped(&p.namespace);
                p
            })
            .collect())
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
        // 归属校验先于状态推进：跨账户提案 ID 不得触发任何写路径
        self.get_cognitive_proposal(id).await?;
        let mut updated = self
            .inner
            .update_cognitive_proposal_status(
                id,
                expected_status,
                new_status,
                rejection_reason,
                resulting_memory_id,
                resolved_by,
            )
            .await?;
        updated.namespace = self.unscoped(&updated.namespace);
        Ok(updated)
    }
}

// ===== GrantStore =====

#[async_trait]
impl<S: GrantStore + ?Sized> GrantStore for AccountScopedStore<S> {
    async fn ensure_namespace(
        &self,
        namespace: &str,
        owner: &AgentId,
    ) -> Result<(NamespaceInfo, bool)> {
        let (info, created) = self
            .inner
            .ensure_namespace(&self.scoped(namespace), owner)
            .await?;
        Ok((self.namespace_info_out(info), created))
    }

    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>> {
        let info = self.inner.get_namespace(&self.scoped(namespace)).await?;
        Ok(info.map(|i| self.namespace_info_out(i)))
    }

    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
        let all = self.inner.list_namespace_infos().await?;
        Ok(all
            .into_iter()
            .filter(|i| in_scope(&self.account, &i.namespace))
            .map(|i| self.namespace_info_out(i))
            .collect())
    }

    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()> {
        let mut scoped = record.clone();
        scoped.namespace = self.scoped(&record.namespace);
        self.inner.grant(&scoped).await
    }

    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()> {
        self.inner.revoke(&self.scoped(namespace), agent_id).await
    }

    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
        let rows = self.inner.grants_for(agent_id).await?;
        Ok(rows
            .into_iter()
            .filter(|r| in_scope(&self.account, &r.namespace))
            .map(|mut r| {
                r.namespace = self.unscoped(&r.namespace);
                r
            })
            .collect())
    }

    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        self.inner.can_read(&self.scoped(namespace), agent_id).await
    }

    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        self.inner
            .can_write(&self.scoped(namespace), agent_id)
            .await
    }

    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>> {
        let all = self.inner.owned_namespaces(agent_id).await?;
        Ok(all
            .into_iter()
            .filter(|ns| in_scope(&self.account, ns))
            .map(|ns| self.unscoped(&ns))
            .collect())
    }
}

// ===== AgentStore =====
//
// Agent 归属关系由 `owner_account_id` 显式表达（不做命名空间前缀），
// 此处全部透传；调用方需自行按 owner_account_id 过滤（如控制台）。

#[async_trait]
impl<S: AgentStore + ?Sized> AgentStore for AccountScopedStore<S> {
    async fn register_agent(&self, reg: &AgentRegistration) -> Result<AgentRecord> {
        self.inner.register_agent(reg).await
    }

    async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<AgentRecord>> {
        self.inner.get_agent(agent_id).await
    }

    async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        self.inner.list_agents().await
    }

    async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
        self.inner.set_agent_status(agent_id, status).await
    }

    async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
        self.inner.verify_agent_secret(agent_id, secret).await
    }
}

// ===== EvidenceStore =====

#[async_trait]
impl<S: EvidenceStore + ?Sized> EvidenceStore for AccountScopedStore<S> {
    async fn append_evidence(&self, evidence: &NewEvidence) -> Result<MemoryEvidence> {
        let mut ev = evidence.clone();
        ev.namespace = self.scoped(&ev.namespace);
        let mut saved = self.inner.append_evidence(&ev).await?;
        saved.namespace = self.unscoped(&saved.namespace);
        Ok(saved)
    }

    async fn get_evidence(&self, namespace: &str, id: Uuid) -> Result<MemoryEvidence> {
        let mut ev = self.inner.get_evidence(&self.scoped(namespace), id).await?;
        self.assert_owned(&ev.namespace)?;
        ev.namespace = self.unscoped(&ev.namespace);
        Ok(ev)
    }

    /// 点查必须委托到内层存储：trait 默认实现退化为窗口扫描，会丢掉
    /// 生产存储"覆盖会话全部历史证据"的幂等保证
    async fn find_evidence_by_hash(
        &self,
        namespace: &str,
        source_kind: EvidenceSourceKind,
        source_ref: &str,
        content_hash: &str,
    ) -> Result<Option<MemoryEvidence>> {
        let mut ev = self
            .inner
            .find_evidence_by_hash(
                &self.scoped(namespace),
                source_kind,
                source_ref,
                content_hash,
            )
            .await?;
        if let Some(e) = ev.as_mut() {
            self.assert_owned(&e.namespace)?;
            e.namespace = self.unscoped(&e.namespace);
        }
        Ok(ev)
    }

    async fn list_evidence(
        &self,
        namespace: &str,
        source_kind: Option<EvidenceSourceKind>,
        source_ref: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEvidence>> {
        let rows = self
            .inner
            .list_evidence(&self.scoped(namespace), source_kind, source_ref, limit)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|e| in_scope(&self.account, &e.namespace))
            .map(|mut e| {
                e.namespace = self.unscoped(&e.namespace);
                e
            })
            .collect())
    }

    async fn all_evidence_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryEvidence>> {
        let rows = self
            .inner
            .all_evidence_in_namespace(&self.scoped(namespace))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|e| in_scope(&self.account, &e.namespace))
            .map(|mut e| {
                e.namespace = self.unscoped(&e.namespace);
                e
            })
            .collect())
    }

    async fn delete_evidence(&self, namespace: &str, id: Uuid) -> Result<()> {
        self.inner
            .delete_evidence(&self.scoped(namespace), id)
            .await
    }

    /// 清理只在**本账户命名空间前缀内**生效，避免跨账户删除
    async fn purge_expired_evidence(
        &self,
        namespace: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<usize> {
        self.inner
            .purge_expired_evidence(&self.scoped(namespace), now)
            .await
    }
}

// ===== RelationStore =====

#[async_trait]
impl<S: RelationStore + ?Sized> RelationStore for AccountScopedStore<S> {
    async fn add_relation(
        &self,
        namespace: &str,
        relation: &NewMemoryRelation,
    ) -> Result<MemoryRelation> {
        self.inner
            .add_relation(&self.scoped(namespace), relation)
            .await
    }

    async fn list_relations_from(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        self.inner
            .list_relations_from(&self.scoped(namespace), from_memory_id)
            .await
    }

    async fn list_relations_to(
        &self,
        namespace: &str,
        to_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        self.inner
            .list_relations_to(&self.scoped(namespace), to_memory_id)
            .await
    }

    async fn delete_relation(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
        to_memory_id: Uuid,
        relation_kind: MemoryRelationKind,
    ) -> Result<()> {
        self.inner
            .delete_relation(
                &self.scoped(namespace),
                from_memory_id,
                to_memory_id,
                relation_kind,
            )
            .await
    }

    async fn all_relations_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryRelation>> {
        self.inner
            .all_relations_in_namespace(&self.scoped(namespace))
            .await
    }
}

// ===== TaskStore =====

#[async_trait]
impl<S: TaskStore + ?Sized> TaskStore for AccountScopedStore<S> {
    async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask> {
        let mut t = task.clone();
        t.namespace = self.scoped(&t.namespace);
        let mut saved = self.inner.create_task(&t).await?;
        saved.namespace = self.unscoped(&saved.namespace);
        Ok(saved)
    }

    async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
        let mut t = task.clone();
        t.namespace = self.scoped(&t.namespace);
        let mut updated = self.inner.update_task(&t).await?;
        updated.namespace = self.unscoped(&updated.namespace);
        Ok(updated)
    }

    async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
        let opt = self.inner.get_task(&self.scoped(namespace), id).await?;
        Ok(opt.and_then(|mut t| {
            if in_scope(&self.account, &t.namespace) {
                t.namespace = self.unscoped(&t.namespace);
                Some(t)
            } else {
                None
            }
        }))
    }

    async fn list_tasks(
        &self,
        namespace: &str,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> Result<Vec<MemoryTask>> {
        let rows = self
            .inner
            .list_tasks(&self.scoped(namespace), status, limit)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|t| in_scope(&self.account, &t.namespace))
            .map(|mut t| {
                t.namespace = self.unscoped(&t.namespace);
                t
            })
            .collect())
    }

    async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
        let rows = self
            .inner
            .all_tasks_in_namespace(&self.scoped(namespace))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|t| in_scope(&self.account, &t.namespace))
            .map(|mut t| {
                t.namespace = self.unscoped(&t.namespace);
                t
            })
            .collect())
    }

    async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
        self.inner.delete_task(&self.scoped(namespace), id).await
    }
}

// ===== ExperienceStore =====

#[async_trait]
impl<S: ExperienceStore + ?Sized> ExperienceStore for AccountScopedStore<S> {
    async fn save_experience(&self, experience: &NewMemoryExperience) -> Result<MemoryExperience> {
        let mut exp = experience.clone();
        exp.namespace = self.scoped(&exp.namespace);
        let mut saved = self.inner.save_experience(&exp).await?;
        saved.namespace = self.unscoped(&saved.namespace);
        Ok(saved)
    }

    async fn update_experience(&self, experience: &MemoryExperience) -> Result<MemoryExperience> {
        let mut exp = experience.clone();
        exp.namespace = self.scoped(&exp.namespace);
        let mut updated = self.inner.update_experience(&exp).await?;
        updated.namespace = self.unscoped(&updated.namespace);
        Ok(updated)
    }

    async fn update_experience_if_version(
        &self,
        experience: &MemoryExperience,
        expected_version: i64,
    ) -> Result<MemoryExperience> {
        let mut exp = experience.clone();
        exp.namespace = self.scoped(&exp.namespace);
        let mut updated = self
            .inner
            .update_experience_if_version(&exp, expected_version)
            .await?;
        updated.namespace = self.unscoped(&updated.namespace);
        Ok(updated)
    }

    async fn append_experience_version(
        &self,
        version: &NewExperienceVersion,
    ) -> Result<ExperienceVersion> {
        let mut scoped = version.clone();
        scoped.namespace = self.scoped(&scoped.namespace);
        let mut saved = self.inner.append_experience_version(&scoped).await?;
        saved.namespace = self.unscoped(&saved.namespace);
        Ok(saved)
    }

    async fn list_experience_versions(
        &self,
        namespace: &str,
        experience_id: Uuid,
        limit: usize,
    ) -> Result<Vec<ExperienceVersion>> {
        let rows = self
            .inner
            .list_experience_versions(&self.scoped(namespace), experience_id, limit)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|v| in_scope(&self.account, &v.namespace))
            .map(|mut v| {
                v.namespace = self.unscoped(&v.namespace);
                v
            })
            .collect())
    }

    async fn all_experience_versions_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<ExperienceVersion>> {
        let rows = self
            .inner
            .all_experience_versions_in_namespace(&self.scoped(namespace))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|v| in_scope(&self.account, &v.namespace))
            .map(|mut v| {
                v.namespace = self.unscoped(&v.namespace);
                v
            })
            .collect())
    }

    async fn get_experience(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryExperience>> {
        let opt = self
            .inner
            .get_experience(&self.scoped(namespace), id)
            .await?;
        Ok(opt.and_then(|mut e| {
            if in_scope(&self.account, &e.namespace) {
                e.namespace = self.unscoped(&e.namespace);
                Some(e)
            } else {
                None
            }
        }))
    }

    async fn list_experiences(
        &self,
        namespace: &str,
        status: Option<ExperienceStatus>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryExperience>> {
        let rows = self
            .inner
            .list_experiences(&self.scoped(namespace), status, limit, offset)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|e| in_scope(&self.account, &e.namespace))
            .map(|mut e| {
                e.namespace = self.unscoped(&e.namespace);
                e
            })
            .collect())
    }

    async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
        let mut scoped_link = link.clone();
        if let Some(ref ns) = link.namespace {
            scoped_link.namespace = Some(self.scoped(ns));
        }
        self.inner.link_task_experience(&scoped_link).await
    }

    async fn list_experiences_for_task(&self, task_id: Uuid) -> Result<Vec<MemoryTaskExperience>> {
        let rows = self.inner.list_experiences_for_task(task_id).await?;
        Ok(rows
            .into_iter()
            .filter(|l| match &l.namespace {
                Some(ns) => in_scope(&self.account, ns),
                None => true,
            })
            .map(|mut l| {
                if let Some(ref ns) = l.namespace {
                    l.namespace = Some(self.unscoped(ns));
                }
                l
            })
            .collect())
    }

    async fn all_experiences_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryExperience>> {
        let rows = self
            .inner
            .all_experiences_in_namespace(&self.scoped(namespace))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|e| in_scope(&self.account, &e.namespace))
            .map(|mut e| {
                e.namespace = self.unscoped(&e.namespace);
                e
            })
            .collect())
    }

    async fn all_task_experiences_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<MemoryTaskExperience>> {
        let rows = self
            .inner
            .all_task_experiences_in_namespace(&self.scoped(namespace))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|l| match &l.namespace {
                Some(ns) => in_scope(&self.account, ns),
                None => true,
            })
            .map(|mut l| {
                if let Some(ref ns) = l.namespace {
                    l.namespace = Some(self.unscoped(ns));
                }
                l
            })
            .collect())
    }

    async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
        self.inner
            .delete_experience(&self.scoped(namespace), id)
            .await
    }
}

#[async_trait]
impl<S: CodingStore + ?Sized> CodingStore for AccountScopedStore<S> {
    async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
        let mut scoped_thread = thread.clone();
        scoped_thread.namespace = self.scoped(&thread.namespace);
        let mut res = self.inner.create_thread(&scoped_thread).await?;
        res.namespace = self.unscoped(&res.namespace);
        Ok(res)
    }

    async fn get_thread(&self, namespace: &str, id: Uuid) -> Result<Option<CodingThread>> {
        let opt = self.inner.get_thread(&self.scoped(namespace), id).await?;
        Ok(opt
            .filter(|t| in_scope(&self.account, &t.namespace))
            .map(|mut t| {
                t.namespace = self.unscoped(&t.namespace);
                t
            }))
    }

    async fn list_threads(
        &self,
        namespace: &str,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodingThread>> {
        let rows = self
            .inner
            .list_threads(&self.scoped(namespace), repository, limit)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|t| in_scope(&self.account, &t.namespace))
            .map(|mut t| {
                t.namespace = self.unscoped(&t.namespace);
                t
            })
            .collect())
    }

    async fn save_revision(
        &self,
        revision: &NewCodingRevision,
        diff_hash: &str,
    ) -> Result<CodingRevision> {
        let mut scoped_rev = revision.clone();
        scoped_rev.namespace = self.scoped(&revision.namespace);
        let mut res = self.inner.save_revision(&scoped_rev, diff_hash).await?;
        res.namespace = self.unscoped(&res.namespace);
        Ok(res)
    }

    async fn get_revision(&self, namespace: &str, id: Uuid) -> Result<Option<CodingRevision>> {
        let opt = self.inner.get_revision(&self.scoped(namespace), id).await?;
        Ok(opt
            .filter(|r| in_scope(&self.account, &r.namespace))
            .map(|mut r| {
                r.namespace = self.unscoped(&r.namespace);
                r
            }))
    }

    async fn get_revision_by_hash(
        &self,
        namespace: &str,
        repository: &str,
        diff_hash: &str,
    ) -> Result<Option<CodingRevision>> {
        let opt = self
            .inner
            .get_revision_by_hash(&self.scoped(namespace), repository, diff_hash)
            .await?;
        Ok(opt
            .filter(|r| in_scope(&self.account, &r.namespace))
            .map(|mut r| {
                r.namespace = self.unscoped(&r.namespace);
                r
            }))
    }

    async fn list_revisions_for_thread(
        &self,
        namespace: &str,
        thread_id: Uuid,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let rows = self
            .inner
            .list_revisions_for_thread(&self.scoped(namespace), thread_id, limit)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| in_scope(&self.account, &r.namespace))
            .map(|mut r| {
                r.namespace = self.unscoped(&r.namespace);
                r
            })
            .collect())
    }

    async fn list_revisions_for_file(
        &self,
        namespace: &str,
        repository: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let rows = self
            .inner
            .list_revisions_for_file(&self.scoped(namespace), repository, file_path, limit)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| in_scope(&self.account, &r.namespace))
            .map(|mut r| {
                r.namespace = self.unscoped(&r.namespace);
                r
            })
            .collect())
    }

    async fn all_threads_in_namespace(&self, namespace: &str) -> Result<Vec<CodingThread>> {
        let rows = self
            .inner
            .all_threads_in_namespace(&self.scoped(namespace))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|t| in_scope(&self.account, &t.namespace))
            .map(|mut t| {
                t.namespace = self.unscoped(&t.namespace);
                t
            })
            .collect())
    }

    async fn all_revisions_in_namespace(&self, namespace: &str) -> Result<Vec<CodingRevision>> {
        let rows = self
            .inner
            .all_revisions_in_namespace(&self.scoped(namespace))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| in_scope(&self.account, &r.namespace))
            .map(|mut r| {
                r.namespace = self.unscoped(&r.namespace);
                r
            })
            .collect())
    }

    async fn delete_thread(&self, namespace: &str, id: Uuid) -> Result<()> {
        self.inner.delete_thread(&self.scoped(namespace), id).await
    }

    async fn delete_revision(&self, namespace: &str, id: Uuid) -> Result<()> {
        self.inner
            .delete_revision(&self.scoped(namespace), id)
            .await
    }
}

// ===== Agent 记忆集合（账户级配置，不做命名空间前缀）=====

#[async_trait]
impl<S: CollectionStore + ?Sized> CollectionStore for AccountScopedStore<S> {
    async fn upsert_collection(&self, collection: &MemoryCollection) -> Result<MemoryCollection> {
        // 集合本身以 account_id 为键；包装器额外校验调用账户，防止装配错误
        self.ensure_account(collection.account_id)?;
        self.inner.upsert_collection(collection).await
    }

    async fn get_collection(
        &self,
        account_id: AccountId,
        id: Uuid,
    ) -> Result<Option<MemoryCollection>> {
        self.ensure_account(account_id)?;
        self.inner.get_collection(account_id, id).await
    }

    async fn get_collection_by_name(
        &self,
        account_id: AccountId,
        name: &str,
    ) -> Result<Option<MemoryCollection>> {
        self.ensure_account(account_id)?;
        self.inner.get_collection_by_name(account_id, name).await
    }

    async fn list_collections(&self, account_id: AccountId) -> Result<Vec<MemoryCollection>> {
        self.ensure_account(account_id)?;
        self.inner.list_collections(account_id).await
    }

    async fn delete_collection(&self, account_id: AccountId, id: Uuid) -> Result<()> {
        self.ensure_account(account_id)?;
        self.inner.delete_collection(account_id, id).await
    }

    async fn bind_collection(&self, binding: &CollectionBinding) -> Result<()> {
        // 绑定前先确认集合属于当前账户（跨账户绑定必须失败）
        self.inner
            .get_collection(self.account, binding.collection_id)
            .await?
            .ok_or(HubError::NotFound)?;
        self.inner.bind_collection(binding).await
    }

    async fn unbind_collection(&self, collection_id: Uuid, agent_id: &AgentId) -> Result<()> {
        self.inner
            .get_collection(self.account, collection_id)
            .await?
            .ok_or(HubError::NotFound)?;
        self.inner.unbind_collection(collection_id, agent_id).await
    }

    async fn list_bindings(&self, collection_id: Uuid) -> Result<Vec<CollectionBinding>> {
        self.inner
            .get_collection(self.account, collection_id)
            .await?
            .ok_or(HubError::NotFound)?;
        self.inner.list_bindings(collection_id).await
    }

    async fn is_bound(&self, collection_id: Uuid, agent_id: &AgentId) -> Result<bool> {
        self.inner.is_bound(collection_id, agent_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(n: u128) -> AccountId {
        Uuid::from_u128(n)
    }

    #[test]
    fn test_scope_helpers_idempotent_and_injective() {
        let a = account(1);
        let b = account(2);
        assert_eq!(scope_name(&a, "personal"), format!("{}/personal", a));
        // 幂等：已带前缀不二次拼接
        assert_eq!(
            scope_idempotent(&a, &scope_name(&a, "personal")),
            scope_name(&a, "personal")
        );
        // 单射：不同账户的同名空间拼出不同键
        assert_ne!(scope_name(&a, "personal"), scope_name(&b, "personal"));
        // 剥离：属于本账户才剥
        assert_eq!(unscope_name(&a, &scope_name(&a, "personal")), "personal");
        assert_eq!(
            unscope_name(&b, &scope_name(&a, "personal")),
            scope_name(&a, "personal")
        );
        // 用户可见名不含 `/`，永远不可能"看起来已作用域化"
        assert!(!in_scope(&a, "personal"));
        assert!(in_scope(&a, &scope_name(&a, "personal")));
    }
}

// 完整虚拟化行为（双账户隔离 / 授权 / 备份 / 前缀幂等）在 store crate 的
// `account_scope_tests` 中验证——那里可以直接使用 InMemoryHubStore。
