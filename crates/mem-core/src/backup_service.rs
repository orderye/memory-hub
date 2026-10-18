//! 备份服务：命名空间快照、恢复与保留策略
//!
//! 备份是记忆库的"整库事实来源之外的第二个事实来源"：
//! - **快照**：某命名空间（或全量 `*`）内全部条目的不可变副本，带 SHA-256 校验；
//! - **恢复**：支持 dry_run 预演、merge 合并与 replace 替换
//!   （replace 前自动为当前状态创建安全备份，防误操作不可逆）；
//! - **保留**：按"每命名空间保留 N 份 + TTL 天数"淘汰最旧快照。

use crate::access::GrantChecker;
use crate::account_scope::AccountScopedStore;
use crate::domain::{
    BackupRecord, BackupSummary, CodingRevision, CodingThread, MemoryEntry, MemoryEvidence,
    MemoryExperience, MemoryRelation, MemoryTask, MemoryTaskExperience, NewCodingRevision,
    NewCodingThread, NewEvidence, NewMemoryExperience, NewMemoryRelation, NewMemoryTask,
    RestoreMode, RestoreReport, VersionChange,
};
use crate::infra::{
    AccessChecker, BackupStore, Clock, CodingStore, EvidenceStore, ExperienceStore, MemoryHubStore,
    MemoryStore, RelationStore, SystemClock, TaskStore,
};
use async_trait::async_trait;
use common::{AuthCtx, HubError, Result, Scope};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

/// 备份负载格式版本（升级至 v4 支持代码演进线程与修订记录）
pub const BACKUP_PAYLOAD_VERSION: u32 = 4;

/// 全量快照的保留命名空间标记（不对应任何真实命名空间，也不是授权对象）
pub const ALL_NAMESPACES: &str = "*";

/// 创建备份请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupCreateRequest {
    /// 目标命名空间；`*` 表示全量
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 恢复请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub backup_id: Uuid,
    #[serde(default = "default_restore_mode")]
    pub mode: RestoreMode,
}

fn default_restore_mode() -> RestoreMode {
    RestoreMode::Merge
}

/// 保留策略执行报告
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PruneReport {
    pub deleted: Vec<Uuid>,
}

/// 备份服务
#[async_trait]
pub trait BackupService: Send + Sync {
    async fn create(&self, auth: &AuthCtx, req: BackupCreateRequest) -> Result<BackupRecord>;
    async fn list(&self, auth: &AuthCtx, namespace: Option<&str>) -> Result<Vec<BackupSummary>>;
    async fn restore(&self, auth: &AuthCtx, req: RestoreRequest) -> Result<RestoreReport>;
    /// 执行保留策略（worker 定期调用；返回被删除的快照 ID）
    async fn prune(&self, keep: usize, retention_days: u32) -> Result<PruneReport>;
}

/// 备份服务实现
pub struct DefaultBackupService {
    store: Arc<dyn MemoryHubStore>,
    access: Arc<dyn AccessChecker>,
    clock: Arc<dyn Clock>,
}

impl DefaultBackupService {
    pub fn new(store: Arc<dyn MemoryHubStore>, access: Arc<dyn AccessChecker>) -> Self {
        Self {
            store,
            access,
            clock: Arc::new(SystemClock),
        }
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// 本请求的账户作用域存储：`*` 全量与恢复都只在账户内展开
    fn scoped(&self, auth: &AuthCtx) -> AccountScopedStore<dyn MemoryHubStore> {
        AccountScopedStore::new(self.store.clone(), auth.account_id)
    }

    /// 读前置校验（含通配命名空间）
    async fn require_read(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        if !self.access.can_read(auth, namespace).await? {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }

    /// 写前置校验
    async fn require_write(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
        if !self.access.can_write(auth, namespace).await? {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }

    /// 管理前置校验（备份/恢复属管理操作）
    async fn require_admin(&self, auth: &AuthCtx) -> Result<()> {
        if !auth.has_scope(&Scope::Admin) {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }

    /// 从负载取出条目列表
    fn entries_of(backup: &BackupRecord) -> Result<Vec<MemoryEntry>> {
        let entries = backup
            .payload
            .get("entries")
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("备份负载缺少 entries 字段")))?;
        serde_json::from_value(entries.clone())
            .map_err(|e| HubError::Internal(anyhow::anyhow!("备份负载解析失败: {}", e)))
    }

    /// 从负载取出证据存根列表
    fn evidences_of(backup: &BackupRecord) -> Result<Vec<MemoryEvidence>> {
        match backup.payload.get("evidences") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                HubError::Internal(anyhow::anyhow!("备份负载 evidences 解析失败: {}", e))
            }),
            None => Ok(Vec::new()),
        }
    }

    /// 从负载取出关系列表
    fn relations_of(backup: &BackupRecord) -> Result<Vec<MemoryRelation>> {
        match backup.payload.get("relations") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                HubError::Internal(anyhow::anyhow!("备份负载 relations 解析失败: {}", e))
            }),
            None => Ok(Vec::new()),
        }
    }

    /// 从负载取出任务列表
    fn tasks_of(backup: &BackupRecord) -> Result<Vec<MemoryTask>> {
        match backup.payload.get("tasks") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| HubError::Internal(anyhow::anyhow!("备份负载 tasks 解析失败: {}", e))),
            None => Ok(Vec::new()),
        }
    }

    /// 从负载取出经验列表
    fn experiences_of(backup: &BackupRecord) -> Result<Vec<MemoryExperience>> {
        match backup.payload.get("experiences") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                HubError::Internal(anyhow::anyhow!("备份负载 experiences 解析失败: {}", e))
            }),
            None => Ok(Vec::new()),
        }
    }

    /// 从负载取出任务-经验映射列表
    fn task_experiences_of(backup: &BackupRecord) -> Result<Vec<MemoryTaskExperience>> {
        match backup.payload.get("task_experiences") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                HubError::Internal(anyhow::anyhow!("备份负载 task_experiences 解析失败: {}", e))
            }),
            None => Ok(Vec::new()),
        }
    }

    /// 从负载取出代码演进线程列表
    fn coding_threads_of(backup: &BackupRecord) -> Result<Vec<CodingThread>> {
        match backup.payload.get("coding_threads") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                HubError::Internal(anyhow::anyhow!("备份负载 coding_threads 解析失败: {}", e))
            }),
            None => Ok(Vec::new()),
        }
    }

    /// 从负载取出代码修订记录列表
    fn coding_revisions_of(backup: &BackupRecord) -> Result<Vec<CodingRevision>> {
        match backup.payload.get("coding_revisions") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                HubError::Internal(anyhow::anyhow!("备份负载 coding_revisions 解析失败: {}", e))
            }),
            None => Ok(Vec::new()),
        }
    }

    /// 计算 v1 负载的规范校验和（仅 entries）
    fn checksum_v1(entries: &[MemoryEntry]) -> String {
        let bytes = serde_json::to_vec(entries).unwrap_or_default();
        hex::encode(Sha256::digest(&bytes))
    }

    /// 计算 v2 负载的规范校验和（entries + evidences + relations）
    fn checksum_v2(
        entries: &[MemoryEntry],
        evidences: &[MemoryEvidence],
        relations: &[MemoryRelation],
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_vec(entries).unwrap_or_default());
        hasher.update(serde_json::to_vec(evidences).unwrap_or_default());
        hasher.update(serde_json::to_vec(relations).unwrap_or_default());
        hex::encode(hasher.finalize())
    }

    /// 计算 v3 负载的规范校验和（包含任务与经验实体）
    fn checksum_v3(
        entries: &[MemoryEntry],
        evidences: &[MemoryEvidence],
        relations: &[MemoryRelation],
        tasks: &[MemoryTask],
        experiences: &[MemoryExperience],
        task_experiences: &[MemoryTaskExperience],
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_vec(entries).unwrap_or_default());
        hasher.update(serde_json::to_vec(evidences).unwrap_or_default());
        hasher.update(serde_json::to_vec(relations).unwrap_or_default());
        hasher.update(serde_json::to_vec(tasks).unwrap_or_default());
        hasher.update(serde_json::to_vec(experiences).unwrap_or_default());
        hasher.update(serde_json::to_vec(task_experiences).unwrap_or_default());
        hex::encode(hasher.finalize())
    }

    /// 计算 v4 负载的规范校验和（包含代码演进线程与修订记录）
    #[allow(clippy::too_many_arguments)]
    fn checksum_v4(
        entries: &[MemoryEntry],
        evidences: &[MemoryEvidence],
        relations: &[MemoryRelation],
        tasks: &[MemoryTask],
        experiences: &[MemoryExperience],
        task_experiences: &[MemoryTaskExperience],
        coding_threads: &[CodingThread],
        coding_revisions: &[CodingRevision],
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_vec(entries).unwrap_or_default());
        hasher.update(serde_json::to_vec(evidences).unwrap_or_default());
        hasher.update(serde_json::to_vec(relations).unwrap_or_default());
        hasher.update(serde_json::to_vec(tasks).unwrap_or_default());
        hasher.update(serde_json::to_vec(experiences).unwrap_or_default());
        hasher.update(serde_json::to_vec(task_experiences).unwrap_or_default());
        hasher.update(serde_json::to_vec(coding_threads).unwrap_or_default());
        hasher.update(serde_json::to_vec(coding_revisions).unwrap_or_default());
        hex::encode(hasher.finalize())
    }

    /// 构建备份负载（v4 格式）
    #[allow(clippy::too_many_arguments)]
    fn payload_of(
        namespace: &str,
        entries: &[MemoryEntry],
        evidences: &[MemoryEvidence],
        relations: &[MemoryRelation],
        tasks: &[MemoryTask],
        experiences: &[MemoryExperience],
        task_experiences: &[MemoryTaskExperience],
        coding_threads: &[CodingThread],
        coding_revisions: &[CodingRevision],
    ) -> serde_json::Value {
        serde_json::json!({
            "version": BACKUP_PAYLOAD_VERSION,
            "namespace": namespace,
            "entries": entries,
            "evidences": evidences,
            "relations": relations,
            "tasks": tasks,
            "experiences": experiences,
            "task_experiences": task_experiences,
            "coding_threads": coding_threads,
            "coding_revisions": coding_revisions,
        })
    }

    /// 内部：为当前状态创建安全备份（replace 恢复前调用）
    async fn create_safety_backup(
        &self,
        auth: &AuthCtx,
        namespace: &str,
    ) -> Result<Option<BackupRecord>> {
        if namespace == "*" {
            // 全量恢复前先对每个命名空间逐一备份代价过高，改为跳过并告警
            tracing::warn!("全量恢复前不自动创建安全备份，请先手动创建全量备份");
            return Ok(None);
        }
        let store = self.scoped(auth);
        let entries = store.all_in_namespace(namespace, true).await?;
        let evidences = store.all_evidence_in_namespace(namespace).await?;
        let relations = store.all_relations_in_namespace(namespace).await?;
        let tasks = store.all_tasks_in_namespace(namespace).await?;
        let experiences = store.all_experiences_in_namespace(namespace).await?;
        let task_experiences = store.all_task_experiences_in_namespace(namespace).await?;
        let coding_threads = store.all_threads_in_namespace(namespace).await?;
        let coding_revisions = store.all_revisions_in_namespace(namespace).await?;
        if entries.is_empty()
            && evidences.is_empty()
            && relations.is_empty()
            && tasks.is_empty()
            && experiences.is_empty()
            && coding_threads.is_empty()
            && coding_revisions.is_empty()
        {
            return Ok(None);
        }
        let now = self.clock.now();
        let record = BackupRecord {
            id: Uuid::new_v4(),
            namespace: namespace.to_string(),
            created_by: auth.agent_id.clone(),
            note: Some(format!("restore-safety:{}", namespace)),
            entry_count: entries.len(),
            checksum: Self::checksum_v4(
                &entries,
                &evidences,
                &relations,
                &tasks,
                &experiences,
                &task_experiences,
                &coding_threads,
                &coding_revisions,
            ),
            payload: Self::payload_of(
                namespace,
                &entries,
                &evidences,
                &relations,
                &tasks,
                &experiences,
                &task_experiences,
                &coding_threads,
                &coding_revisions,
            ),
            retain_until: None,
            created_at: now,
        };
        store.save_backup(&record).await?;
        Ok(Some(record))
    }
}

#[async_trait]
impl BackupService for DefaultBackupService {
    async fn create(&self, auth: &AuthCtx, req: BackupCreateRequest) -> Result<BackupRecord> {
        self.require_admin(auth).await?;
        if req.namespace.trim().is_empty() {
            return Err(HubError::InvalidArgument("namespace 不能为空".to_string()));
        }
        let store = self.scoped(auth);

        let entries: Vec<MemoryEntry> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_in_namespace(&ns, true).await?);
            }
            all
        } else {
            self.require_write(auth, &req.namespace).await?;
            store.all_in_namespace(&req.namespace, true).await?
        };

        let evidences: Vec<MemoryEvidence> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_evidence_in_namespace(&ns).await?);
            }
            all
        } else {
            store.all_evidence_in_namespace(&req.namespace).await?
        };

        let relations: Vec<MemoryRelation> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_relations_in_namespace(&ns).await?);
            }
            all
        } else {
            store.all_relations_in_namespace(&req.namespace).await?
        };

        let tasks: Vec<MemoryTask> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_tasks_in_namespace(&ns).await?);
            }
            all
        } else {
            store.all_tasks_in_namespace(&req.namespace).await?
        };

        let experiences: Vec<MemoryExperience> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_experiences_in_namespace(&ns).await?);
            }
            all
        } else {
            store.all_experiences_in_namespace(&req.namespace).await?
        };

        let task_experiences: Vec<MemoryTaskExperience> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_task_experiences_in_namespace(&ns).await?);
            }
            all
        } else {
            store
                .all_task_experiences_in_namespace(&req.namespace)
                .await?
        };

        let coding_threads: Vec<CodingThread> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_threads_in_namespace(&ns).await?);
            }
            all
        } else {
            store.all_threads_in_namespace(&req.namespace).await?
        };

        let coding_revisions: Vec<CodingRevision> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_revisions_in_namespace(&ns).await?);
            }
            all
        } else {
            store.all_revisions_in_namespace(&req.namespace).await?
        };

        let now = self.clock.now();
        let record = BackupRecord {
            id: Uuid::new_v4(),
            namespace: req.namespace.clone(),
            created_by: auth.agent_id.clone(),
            note: req.note.clone(),
            entry_count: entries.len(),
            checksum: Self::checksum_v4(
                &entries,
                &evidences,
                &relations,
                &tasks,
                &experiences,
                &task_experiences,
                &coding_threads,
                &coding_revisions,
            ),
            payload: Self::payload_of(
                &req.namespace,
                &entries,
                &evidences,
                &relations,
                &tasks,
                &experiences,
                &task_experiences,
                &coding_threads,
                &coding_revisions,
            ),
            retain_until: None,
            created_at: now,
        };
        store.save_backup(&record).await?;
        Ok(record)
    }

    async fn list(&self, auth: &AuthCtx, namespace: Option<&str>) -> Result<Vec<BackupSummary>> {
        let all = self.scoped(auth).list_backups(namespace).await?;
        let mut out = Vec::with_capacity(all.len());
        for b in &all {
            // 逐份校验访问权。全量快照的命名空间标记 `*` 不是授权对象（通配授权已废除），
            // 若按命名空间判权必然落空并让整次列举失败，故改由 admin 作用域把关。
            if b.namespace == ALL_NAMESPACES {
                self.require_admin(auth).await?;
            } else {
                self.require_read(auth, &b.namespace).await?;
            }
            out.push(BackupSummary::from(b));
        }
        Ok(out)
    }

    async fn restore(&self, auth: &AuthCtx, req: RestoreRequest) -> Result<RestoreReport> {
        self.require_admin(auth).await?;
        let store = self.scoped(auth);
        let backup = store.get_backup(req.backup_id).await?;

        // 完整性校验：重建校验和与写入时比对（兼容 v1、v2、v3 与 v4）
        let entries = Self::entries_of(&backup)?;
        let evidences = Self::evidences_of(&backup)?;
        let relations = Self::relations_of(&backup)?;
        let tasks = Self::tasks_of(&backup)?;
        let experiences = Self::experiences_of(&backup)?;
        let task_experiences = Self::task_experiences_of(&backup)?;
        let coding_threads = Self::coding_threads_of(&backup)?;
        let coding_revisions = Self::coding_revisions_of(&backup)?;

        let actual_v4 = Self::checksum_v4(
            &entries,
            &evidences,
            &relations,
            &tasks,
            &experiences,
            &task_experiences,
            &coding_threads,
            &coding_revisions,
        );
        let actual_v3 = Self::checksum_v3(
            &entries,
            &evidences,
            &relations,
            &tasks,
            &experiences,
            &task_experiences,
        );
        let actual_v2 = Self::checksum_v2(&entries, &evidences, &relations);
        let actual_v1 = Self::checksum_v1(&entries);

        let valid = backup.checksum == actual_v4
            || backup.checksum == actual_v3
            || backup.checksum == actual_v2
            || backup.checksum == actual_v1;
        if !valid {
            return Err(HubError::Internal(anyhow::anyhow!(
                "备份校验和不匹配（内容可能已损坏）"
            )));
        }

        let now = self.clock.now();
        let mut report = RestoreReport {
            backup_id: backup.id,
            mode: req.mode,
            total: entries.len(),
            created: 0,
            updated: 0,
            unchanged: 0,
            safety_backup_id: None,
        };

        if req.mode == RestoreMode::DryRun {
            for e in &entries {
                match store.get(&e.namespace, e.id).await {
                    Ok(cur) if cur == *e => report.unchanged += 1,
                    Ok(_) => report.updated += 1,
                    Err(_) => report.created += 1,
                }
            }
            return Ok(report);
        }

        // 目标命名空间集合（全量备份可能含多个命名空间）
        let mut namespaces: Vec<String> = entries.iter().map(|e| e.namespace.clone()).collect();
        namespaces.sort();
        namespaces.dedup();

        if req.mode == RestoreMode::Replace {
            // 先为每个受影响的命名空间创建安全备份
            for ns in &namespaces {
                self.require_write(auth, ns).await?;
                if let Some(safety) = self.create_safety_backup(auth, ns).await? {
                    report.safety_backup_id = Some(safety.id);
                }
            }
            // 再清空当前状态（merged 一并清除，保证与快照一致）
            for ns in &namespaces {
                for cur in store.all_in_namespace(ns, true).await? {
                    store.delete(&cur.namespace, cur.id).await?;
                }
                for cur in store.all_evidence_in_namespace(ns).await? {
                    store.delete_evidence(&cur.namespace, cur.id).await?;
                }
            }
        } else {
            for ns in &namespaces {
                self.require_write(auth, ns).await?;
            }
        }

        // 恢复证据存根
        for ev in &evidences {
            let _ = store
                .append_evidence(&NewEvidence {
                    id: Some(ev.id),
                    namespace: ev.namespace.clone(),
                    source_kind: ev.source_kind,
                    source_ref: ev.source_ref.clone(),
                    content_hash: ev.content_hash.clone(),
                    excerpt: ev.excerpt.clone(),
                    occurred_at: ev.occurred_at,
                })
                .await;
        }

        for mut e in entries.clone() {
            match store.get(&e.namespace, e.id).await {
                Ok(cur) => {
                    if cur == e {
                        report.unchanged += 1;
                        continue;
                    }
                    // 合并：保留快照内容，推进版本与时间戳
                    e.version = cur.version + 1;
                    e.updated_at = now;
                    e.source_agent = auth.agent_id.clone();
                    store
                        .revise(&e, cur.version, VersionChange::Updated)
                        .await?;
                    report.updated += 1;
                }
                Err(_) => {
                    // 新建：保留快照的 created_at，updated_at 置为恢复时刻
                    e.updated_at = now;
                    e.source_agent = auth.agent_id.clone();
                    store.insert_initial(&e).await?;
                    report.created += 1;
                }
            }
        }

        // 恢复实体关系网
        for rel in &relations {
            let rel_ns = if backup.namespace != ALL_NAMESPACES {
                backup.namespace.clone()
            } else {
                entries
                    .iter()
                    .find(|e| e.id == rel.from_memory_id)
                    .map(|e| e.namespace.clone())
                    .unwrap_or_else(|| backup.namespace.clone())
            };
            let _ = store
                .add_relation(
                    &rel_ns,
                    &NewMemoryRelation {
                        from_memory_id: rel.from_memory_id,
                        to_memory_id: rel.to_memory_id,
                        relation_kind: rel.relation_kind,
                        confidence: rel.confidence,
                        source_evidence_id: rel.source_evidence_id,
                    },
                )
                .await;
        }

        // 恢复任务事实
        for t in &tasks {
            let _ = store
                .create_task(&NewMemoryTask {
                    id: Some(t.id),
                    namespace: t.namespace.clone(),
                    scene: t.scene,
                    task: t.task.clone(),
                    status: t.status,
                    started_at: t.started_at,
                    finished_at: t.finished_at,
                    result_summary: t.result_summary.clone(),
                    failure_reason: t.failure_reason.clone(),
                    source_evidence_id: t.source_evidence_id,
                    metadata: t.metadata.clone(),
                })
                .await;
        }

        // 恢复经验记忆
        for exp in &experiences {
            let _ = store
                .save_experience(&NewMemoryExperience {
                    id: Some(exp.id),
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
                })
                .await;
        }

        // 恢复任务-经验关联映射
        for te in &task_experiences {
            let _ = store.link_task_experience(te).await;
        }

        // 恢复代码演进线程
        for th in &coding_threads {
            let _ = store
                .create_thread(&NewCodingThread {
                    id: Some(th.id),
                    namespace: th.namespace.clone(),
                    repository: th.repository.clone(),
                    branch: th.branch.clone(),
                    task_id: th.task_id,
                    title: th.title.clone(),
                    description: th.description.clone(),
                })
                .await;
        }

        // 恢复代码修订记录
        for rev in &coding_revisions {
            let _ = store
                .save_revision(
                    &NewCodingRevision {
                        id: Some(rev.id),
                        thread_id: rev.thread_id,
                        namespace: rev.namespace.clone(),
                        repository: rev.repository.clone(),
                        branch: rev.branch.clone(),
                        commit_sha: rev.commit_sha.clone(),
                        parent_sha: rev.parent_sha.clone(),
                        file_path: rev.file_path.clone(),
                        change_kind: rev.change_kind,
                        diff_content: rev.diff_content.clone(),
                        symbol_fingerprint: rev.symbol_fingerprint.clone(),
                        is_committed: rev.is_committed,
                        author: rev.author.clone(),
                        occurred_at: Some(rev.occurred_at),
                        source_evidence_id: rev.source_evidence_id,
                        source_task_id: rev.source_task_id,
                    },
                    &rev.diff_hash,
                )
                .await;
        }

        Ok(report)
    }

    async fn prune(&self, keep: usize, retention_days: u32) -> Result<PruneReport> {
        let keep = keep.max(1);
        let now = self.clock.now();
        // 系统级保留策略（worker 调用，无请求账户）：按原始存储对全库统一执行
        let all = self.store.list_backups(None).await?;

        // 按命名空间分组，组内按 created_at 从新到旧排序
        let mut by_ns: std::collections::HashMap<String, Vec<BackupRecord>> =
            std::collections::HashMap::new();
        for b in all {
            by_ns.entry(b.namespace.clone()).or_default().push(b);
        }

        let mut deleted = Vec::new();
        for (_ns, mut records) in by_ns {
            records.sort_by_key(|b| std::cmp::Reverse(b.created_at));
            for (idx, record) in records.iter().enumerate() {
                // 超出保留份数即删；TTL 仅淘汰"非最新"的旧快照（至少保留 1 份）
                let beyond_count = idx >= keep;
                let beyond_ttl = retention_days > 0
                    && idx >= 1
                    && record.created_at + chrono::Duration::days(i64::from(retention_days)) <= now;
                if beyond_count || beyond_ttl {
                    self.store.delete_backup(record.id).await?;
                    deleted.push(record.id);
                }
            }
        }

        Ok(PruneReport { deleted })
    }
}

impl DefaultBackupService {}

/// 便捷构造：GrantChecker 的再导出（供二进制装配复用）
pub fn grant_checker(store: Arc<dyn MemoryHubStore>) -> Arc<GrantChecker> {
    Arc::new(GrantChecker::new(store.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 测试账户作用域名：与 `auth()`/`writer()` 辅助中的账户一致
    fn scoped_ns(ns: &str) -> String {
        crate::account_scope::scope_name(&common::BOOTSTRAP_ACCOUNT_ID, ns)
    }
    use crate::domain::{
        EvidenceSourceKind, ExperienceStatus, IntegrationProposal, MemoryEvidence, MemoryFilter,
        MemoryKind, MemoryRelation, MemoryRelationKind, MemoryStatus, MemoryVersion,
        NamespaceGrantRecord, NamespaceInfo, NewEvidence, NewMemoryRelation, ProposalStatus,
        TaskStatus,
    };
    use crate::infra::{
        BackupStore, Embedder, EvidenceStore, GrantStore, IntegrationStore, MemoryStore,
        RelationStore,
    };
    use crate::testing::TestEmbedder;
    use common::{AgentId, Scope};
    use std::collections::HashMap as StdHashMap;
    use std::sync::RwLock;

    /// 测试用全功能存储
    #[derive(Default)]
    struct TestHubStore {
        items: RwLock<StdHashMap<(String, Uuid), MemoryEntry>>,
        backups: RwLock<StdHashMap<Uuid, BackupRecord>>,
        namespaces: RwLock<StdHashMap<String, NamespaceInfo>>,
        grants: RwLock<Vec<NamespaceGrantRecord>>,
        versions: RwLock<Vec<MemoryVersion>>,
        evidences: RwLock<Vec<MemoryEvidence>>,
        relations: RwLock<Vec<MemoryRelation>>,
        tasks: RwLock<StdHashMap<(String, Uuid), MemoryTask>>,
        experiences: RwLock<StdHashMap<(String, Uuid), MemoryExperience>>,
        task_experiences: RwLock<Vec<MemoryTaskExperience>>,
        coding_threads: RwLock<StdHashMap<(String, Uuid), CodingThread>>,
        coding_revisions: RwLock<StdHashMap<(String, Uuid), CodingRevision>>,
    }

    impl TestHubStore {
        fn seed(&self, ns: &str, agent: &str, content: &str) -> MemoryEntry {
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
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
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
        ) -> Result<Vec<crate::domain::Scored<MemoryEntry>>> {
            Ok(vec![])
        }
        async fn all_in_namespace(
            &self,
            ns: &str,
            include_merged: bool,
        ) -> Result<Vec<MemoryEntry>> {
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
        async fn archive_expired(&self, _now: chrono::DateTime<chrono::Utc>) -> Result<usize> {
            Ok(0)
        }
    }

    #[async_trait]
    impl crate::infra::BackupStore for TestHubStore {
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
            let _ = p;
            Ok(())
        }
        async fn get_proposal(&self, _id: Uuid) -> Result<IntegrationProposal> {
            Err(HubError::NotFound)
        }
        async fn list_proposals(
            &self,
            _ns: &str,
            _status: Option<ProposalStatus>,
        ) -> Result<Vec<IntegrationProposal>> {
            Ok(vec![])
        }
        async fn open_exists(&self, _ns: &str, _members: &[(String, Vec<Uuid>)]) -> Result<bool> {
            Ok(false)
        }
    }

    #[async_trait]
    impl crate::infra::GrantStore for TestHubStore {
        async fn ensure_namespace(
            &self,
            ns: &str,
            owner: &AgentId,
        ) -> Result<(NamespaceInfo, bool)> {
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
                        created_at: chrono::Utc::now(),
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
    impl crate::infra::AgentStore for TestHubStore {
        async fn register_agent(
            &self,
            _reg: &crate::infra::AgentRegistration,
        ) -> Result<crate::infra::AgentRecord> {
            Err(HubError::Unavailable)
        }
        async fn get_agent(
            &self,
            _agent_id: &AgentId,
        ) -> Result<Option<crate::infra::AgentRecord>> {
            Ok(None)
        }
        async fn list_agents(&self) -> Result<Vec<crate::infra::AgentRecord>> {
            Ok(vec![])
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
                created_at: chrono::Utc::now(),
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
        async fn add_relation(&self, _ns: &str, rel: &NewMemoryRelation) -> Result<MemoryRelation> {
            let record = MemoryRelation {
                from_memory_id: rel.from_memory_id,
                to_memory_id: rel.to_memory_id,
                relation_kind: rel.relation_kind,
                confidence: rel.confidence,
                source_evidence_id: rel.source_evidence_id,
                created_at: chrono::Utc::now(),
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
    }

    #[async_trait]
    impl ExperienceStore for TestHubStore {
        async fn save_experience(
            &self,
            experience: &NewMemoryExperience,
        ) -> Result<MemoryExperience> {
            let id = experience.id.unwrap_or_else(Uuid::new_v4);
            let now = chrono::Utc::now();
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
            let mut r = self.task_experiences.write().unwrap();
            if !r
                .iter()
                .any(|l| l.task_id == link.task_id && l.experience_id == link.experience_id)
            {
                r.push(link.clone());
            }
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
    }

    #[async_trait]
    impl CodingStore for TestHubStore {
        async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
            let id = thread.id.unwrap_or_else(Uuid::new_v4);
            let now = chrono::Utc::now();
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
            let now = chrono::Utc::now();
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
    }

    fn admin(agent: &str) -> AuthCtx {
        AuthCtx {
            agent_id: agent.to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Default::default(),
        }
    }

    fn setup() -> (DefaultBackupService, Arc<TestHubStore>) {
        let store = Arc::new(TestHubStore::default());
        let svc =
            DefaultBackupService::new(store.clone(), Arc::new(GrantChecker::new(store.clone())));
        (svc, store)
    }

    #[tokio::test]
    async fn test_backup_create_and_restore_dry_run() {
        let (svc, store) = setup();
        let a = admin("agent-a");

        // 建立命名空间归属
        store
            .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        let e1 = store.seed(&scoped_ns("ns"), "agent-a", "记忆一");
        store.seed(&scoped_ns("ns"), "agent-a", "记忆二");

        let backup = svc
            .create(
                &a,
                BackupCreateRequest {
                    namespace: "ns".to_string(),
                    note: Some("测试备份".to_string()),
                },
            )
            .await
            .unwrap();
        assert_eq!(backup.entry_count, 2);
        assert!(!backup.checksum.is_empty());

        // dry_run：应识别 2 条 unchanged
        let report = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: backup.id,
                    mode: RestoreMode::DryRun,
                },
            )
            .await
            .unwrap();
        assert_eq!(report.total, 2);
        assert_eq!(report.unchanged, 2);

        // 修改一条后 merge：应识别 1 updated 1 unchanged
        let mut changed = store.get(&scoped_ns("ns"), e1.id).await.unwrap();
        changed.content = "被外部改动了".to_string();
        store.save(&changed).await.unwrap();

        let report2 = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: backup.id,
                    mode: RestoreMode::Merge,
                },
            )
            .await
            .unwrap();
        assert_eq!(report2.updated, 1);
        assert_eq!(report2.unchanged, 1);
        assert_eq!(
            store.get(&scoped_ns("ns"), e1.id).await.unwrap().content,
            "记忆一",
            "merge 后内容应恢复为快照值"
        );
    }

    #[tokio::test]
    async fn test_replace_creates_safety_backup_and_rebuilds() {
        let (svc, store) = setup();
        let a = admin("agent-a");
        store
            .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        store.seed(&scoped_ns("ns"), "agent-a", "旧内容一");
        store.seed(&scoped_ns("ns"), "agent-a", "旧内容二");

        let backup = svc
            .create(
                &a,
                BackupCreateRequest {
                    namespace: "ns".to_string(),
                    note: None,
                },
            )
            .await
            .unwrap();

        // 删掉一条、改写一条，再 replace 恢复
        let items = store
            .all_in_namespace(&scoped_ns("ns"), true)
            .await
            .unwrap();
        store.delete(&scoped_ns("ns"), items[0].id).await.unwrap();
        let mut kept = store.get(&scoped_ns("ns"), items[1].id).await.unwrap();
        kept.content = "被改坏了".to_string();
        store.save(&kept).await.unwrap();

        let report = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: backup.id,
                    mode: RestoreMode::Replace,
                },
            )
            .await
            .unwrap();
        assert!(
            report.safety_backup_id.is_some(),
            "replace 前必须自动创建安全备份"
        );
        // replace 先清空当前状态再按快照重建，两条都应计为 created
        assert_eq!(report.created, 2, "replace 后两条都应按快照重建");

        let after = store
            .all_in_namespace(&scoped_ns("ns"), true)
            .await
            .unwrap();
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|e| e.content == "旧内容一"));
        assert!(after.iter().any(|e| e.content == "旧内容二"));
    }

    #[tokio::test]
    async fn test_safety_backup_can_recover_pre_replace_state() {
        let (svc, store) = setup();
        let a = admin("agent-a");
        store
            .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        // 初始状态：版本 A
        store.seed(&scoped_ns("ns"), "agent-a", "原始数据 A");
        let backup_v1 = svc
            .create(
                &a,
                BackupCreateRequest {
                    namespace: "ns".to_string(),
                    note: Some("v1".to_string()),
                },
            )
            .await
            .unwrap();

        // 后续改动为版本 B
        let all = store
            .all_in_namespace(&scoped_ns("ns"), true)
            .await
            .unwrap();
        let mut cur = all[0].clone();
        cur.content = "被修改后的数据 B".to_string();
        store.save(&cur).await.unwrap();

        // 执行 replace 恢复至 v1，系统必须自动创建安全备份 (保存了数据 B)
        let replace_report = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: backup_v1.id,
                    mode: RestoreMode::Replace,
                },
            )
            .await
            .unwrap();
        let safety_id = replace_report.safety_backup_id.expect("必须生成安全备份");

        // 验证当前已被 v1 替换
        let items_v1 = store
            .all_in_namespace(&scoped_ns("ns"), true)
            .await
            .unwrap();
        assert_eq!(items_v1[0].content, "原始数据 A");

        // 验证：使用 safety_backup_id 恢复可成功找回数据 B
        let rollback_report = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: safety_id,
                    mode: RestoreMode::Replace,
                },
            )
            .await
            .unwrap();
        assert_eq!(rollback_report.total, 1);
        let items_rollback = store
            .all_in_namespace(&scoped_ns("ns"), true)
            .await
            .unwrap();
        assert_eq!(items_rollback[0].content, "被修改后的数据 B");
    }

    #[tokio::test]
    async fn test_restore_detects_corrupted_checksum() {
        let (svc, store) = setup();
        let a = admin("agent-a");
        store
            .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        store.seed(&scoped_ns("ns"), "agent-a", "记忆");

        let backup = svc
            .create(
                &a,
                BackupCreateRequest {
                    namespace: "ns".to_string(),
                    note: None,
                },
            )
            .await
            .unwrap();

        // 篡改负载内容（校验和不变 → 恢复必须报错）
        let mut tampered = backup.clone();
        // 存储层保存的是带账户前缀的内部键（服务层写入必经 scoped），
        // 直接操作裸存储时需自行还原该形态。
        tampered.namespace = scoped_ns("ns");
        tampered.payload = serde_json::json!({
            "version": BACKUP_PAYLOAD_VERSION,
            "namespace": "ns",
            "entries": [],
            "evidences": [],
            "relations": [],
        });
        store.save_backup(&tampered).await.unwrap();

        let err = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: backup.id,
                    mode: RestoreMode::DryRun,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::Internal(_)));
    }

    #[tokio::test]
    async fn test_prune_keeps_latest_n() {
        let (svc, store) = setup();
        let a = admin("agent-a");
        store
            .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        store.seed(&scoped_ns("ns"), "agent-a", "记忆");

        let mut ids = Vec::new();
        for i in 0..3 {
            let b = svc
                .create(
                    &a,
                    BackupCreateRequest {
                        namespace: "ns".to_string(),
                        note: Some(format!("第 {} 份", i)),
                    },
                )
                .await
                .unwrap();
            ids.push(b.id);
            // 让创建时间拉开差距
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let report = svc.prune(1, 0).await.unwrap();
        assert_eq!(report.deleted.len(), 2, "保留 1 份应删除 2 份");
        // 最新的一份必须还在
        let remaining = store.list_backups(None).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn test_list_backup_requires_read_access() {
        let (svc, store) = setup();
        let a = admin("agent-a");
        let stranger = AuthCtx {
            agent_id: "agent-b".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Default::default(),
        };

        store
            .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        store.seed(&scoped_ns("ns"), "agent-a", "记忆");
        svc.create(
            &a,
            BackupCreateRequest {
                namespace: "ns".to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

        // 陌生人即使有全部 scope 也看不到别人的命名空间备份
        let err = svc.list(&stranger, Some("ns")).await.unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));
    }

    #[tokio::test]
    async fn test_wildcard_backup_visible_to_admin_not_to_reader() {
        let (svc, store) = setup();
        let a = admin("agent-a");
        store
            .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        store.seed(&scoped_ns("ns"), "agent-a", "记忆");

        svc.create(
            &a,
            BackupCreateRequest {
                namespace: "*".to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

        // 全量快照的 `*` 不是授权对象：过去按命名空间判权会让整次列举失败
        let listed = svc
            .list(&a, None)
            .await
            .expect("属主应能列举含全量快照的备份");
        assert!(listed.iter().any(|b| b.namespace == "*"));

        // 无 admin 作用域者不得看到全量快照
        let reader = AuthCtx {
            agent_id: "agent-a".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read],
            acl: Default::default(),
        };
        let err = svc.list(&reader, None).await.unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));
    }

    #[tokio::test]
    async fn test_backup_requires_admin_scope() {
        let (svc, _store) = setup();
        let no_admin = AuthCtx {
            agent_id: "agent-a".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write],
            acl: Default::default(),
        };
        let err = svc
            .create(
                &no_admin,
                BackupCreateRequest {
                    namespace: "ns".to_string(),
                    note: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));
    }

    // 编译期检查：Embedder trait 对象可用
    #[allow(dead_code)]
    fn assert_embedder_object() {
        let _: Arc<dyn Embedder> = Arc::new(TestEmbedder::default());
    }
}
