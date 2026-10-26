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

    /// 系统级 v4 快照构建：定时备份作业（worker）不经 AuthCtx、无请求账户，
    /// 按存储层内部命名空间键（`{account}/{namespace}`）解析出归属账户，
    /// 经 `AccountScopedStore` 收集全部实体。由此产出的载荷与 `create` 手工备份
    /// 同构（实体为用户可见命名空间名），可原样走服务层 restore；校验和复用
    /// v4 算法。无账户前缀的裸命名空间（生产不应出现；测试/遗留数据）退化为
    /// 原始存储直读，保持与旧 v1 定时快照一致的可见性边界。
    pub async fn system_snapshot(
        store: &Arc<dyn MemoryHubStore>,
        namespace: &str,
        created_by: &common::AgentId,
        note: Option<String>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<BackupRecord> {
        // 内部键形态：`{account}/{ns}`；用户可见名不含 `/`，split_once 不会切错
        let scoped_account = namespace
            .split_once(crate::account_scope::SCOPE_SEPARATOR)
            .and_then(|(acct, _)| Uuid::parse_str(acct).ok());

        match scoped_account {
            Some(account) => {
                let visible_ns = crate::account_scope::unscope_name(&account, namespace);
                let scoped = crate::account_scope::AccountScopedStore::new(store.clone(), account);
                let entries = scoped.all_in_namespace(&visible_ns, true).await?;
                let evidences = scoped.all_evidence_in_namespace(&visible_ns).await?;
                let relations = scoped.all_relations_in_namespace(&visible_ns).await?;
                let tasks = scoped.all_tasks_in_namespace(&visible_ns).await?;
                let experiences = scoped.all_experiences_in_namespace(&visible_ns).await?;
                let task_experiences = scoped
                    .all_task_experiences_in_namespace(&visible_ns)
                    .await?;
                let coding_threads = scoped.all_threads_in_namespace(&visible_ns).await?;
                let coding_revisions = scoped.all_revisions_in_namespace(&visible_ns).await?;
                let record = BackupRecord {
                    id: Uuid::new_v4(),
                    namespace: visible_ns.clone(),
                    created_by: created_by.clone(),
                    note,
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
                        &visible_ns,
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
                // 经作用域包装器落库：存储键回到内部形态，与手工备份同一存放规则
                scoped.save_backup(&record).await?;
                Ok(record)
            }
            None => {
                let entries = store.all_in_namespace(namespace, true).await?;
                let evidences = store.all_evidence_in_namespace(namespace).await?;
                let relations = store.all_relations_in_namespace(namespace).await?;
                let tasks = store.all_tasks_in_namespace(namespace).await?;
                let experiences = store.all_experiences_in_namespace(namespace).await?;
                let task_experiences = store.all_task_experiences_in_namespace(namespace).await?;
                let coding_threads = store.all_threads_in_namespace(namespace).await?;
                let coding_revisions = store.all_revisions_in_namespace(namespace).await?;
                let record = BackupRecord {
                    id: Uuid::new_v4(),
                    namespace: namespace.to_string(),
                    created_by: created_by.clone(),
                    note,
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
                Ok(record)
            }
        }
    }

    /// 记一次恢复失败：计数并保留首条错误（作汇总日志摘要）
    fn tally_failure(count: &mut usize, first_err: &mut Option<HubError>, e: HubError) {
        if first_err.is_none() {
            *first_err = Some(e);
        }
        *count += 1;
    }

    /// 每类实体恢复完成后输出一条汇总告警（不逐条打日志刷屏）
    fn warn_restore_failures(
        kind: &str,
        namespace: &str,
        failed: usize,
        first_err: &Option<HubError>,
    ) {
        if failed == 0 {
            return;
        }
        tracing::warn!(
            entity = kind,
            namespace = %namespace,
            failed,
            first_error = ?first_err.as_ref().map(|e| e.to_string()),
            "恢复部分实体失败（已跳过并继续，详见 RestoreReport 失败计数）"
        );
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
            safety_backup_ids: Vec::new(),
            failed_evidences: 0,
            failed_relations: 0,
            failed_tasks: 0,
            failed_experiences: 0,
            failed_task_links: 0,
            failed_threads: 0,
            failed_revisions: 0,
        };

        if req.mode == RestoreMode::DryRun {
            for e in &entries {
                match store.get(&e.namespace, e.id).await {
                    Ok(cur) if cur == *e => report.unchanged += 1,
                    Ok(_) => report.updated += 1,
                    // 只有 NotFound 计为"将新建"；其他存储错误是真实故障，不应
                    // 被预演报告伪装成 created，上抛让调用方感知
                    Err(HubError::NotFound) => report.created += 1,
                    Err(e) => return Err(e),
                }
            }
            return Ok(report);
        }

        // 目标命名空间集合（全量备份可能含多个命名空间，且涵盖认知各实体）
        let mut namespaces: Vec<String> = entries
            .iter()
            .map(|e| e.namespace.clone())
            .chain(evidences.iter().map(|e| e.namespace.clone()))
            .chain(tasks.iter().map(|t| t.namespace.clone()))
            .chain(experiences.iter().map(|e| e.namespace.clone()))
            .chain(coding_threads.iter().map(|t| t.namespace.clone()))
            .chain(coding_revisions.iter().map(|r| r.namespace.clone()))
            .collect();
        if backup.namespace != ALL_NAMESPACES && !backup.namespace.trim().is_empty() {
            namespaces.push(backup.namespace.clone());
        }
        namespaces.sort();
        namespaces.dedup();

        if req.mode == RestoreMode::Replace {
            // 先为每个受影响的命名空间创建安全备份（逐个记录，多命名空间时不互相覆盖）
            for ns in &namespaces {
                self.require_write(auth, ns).await?;
                if let Some(safety) = self.create_safety_backup(auth, ns).await? {
                    if report.safety_backup_id.is_none() {
                        // 兼容旧消费方：单数形式保留第一个
                        report.safety_backup_id = Some(safety.id);
                    }
                    report.safety_backup_ids.push(safety.id);
                }
            }
            // 清空当前状态：按外键依赖逆序由子表到父表深度清空，保证无脏数据且无外键阻塞
            for ns in &namespaces {
                // 1. 删除代码修订
                for cur in store
                    .all_revisions_in_namespace(ns)
                    .await
                    .unwrap_or_default()
                {
                    let _ = store.delete_revision(ns, cur.id).await;
                }
                // 2. 删除代码线程
                for cur in store.all_threads_in_namespace(ns).await.unwrap_or_default() {
                    let _ = store.delete_thread(ns, cur.id).await;
                }
                // 3. 删除经验（附带任务-经验关联清理）
                for cur in store
                    .all_experiences_in_namespace(ns)
                    .await
                    .unwrap_or_default()
                {
                    let _ = store.delete_experience(ns, cur.id).await;
                }
                // 4. 删除任务事实
                for cur in store.all_tasks_in_namespace(ns).await.unwrap_or_default() {
                    let _ = store.delete_task(ns, cur.id).await;
                }
                // 5. 删除实体关联
                for cur in store
                    .all_relations_in_namespace(ns)
                    .await
                    .unwrap_or_default()
                {
                    let _ = store
                        .delete_relation(
                            ns,
                            cur.from_memory_id,
                            cur.to_memory_id,
                            cur.relation_kind,
                        )
                        .await;
                }
                // 6. 删除证据
                for cur in store
                    .all_evidence_in_namespace(ns)
                    .await
                    .unwrap_or_default()
                {
                    let _ = store.delete_evidence(&cur.namespace, cur.id).await;
                }
                // 7. 删除核心记忆条目（包括历史版本）
                for cur in store.all_in_namespace(ns, true).await.unwrap_or_default() {
                    let _ = store.delete(&cur.namespace, cur.id).await;
                }
            }
        } else {
            for ns in &namespaces {
                self.require_write(auth, ns).await?;
            }
        }

        // 以下各实体的恢复遵循同一取舍：**单条失败不中止恢复**。
        // 部分恢复优于全不恢复——replace 模式在清空前已创建安全备份，
        // 即使恢复结果不完整，也能按安全备份重放找回现场；失败只计数、
        // 每类实体汇总一条告警（namespace + 失败数 + 首条错误摘要），避免逐条刷屏。

        // 恢复证据存根
        let mut first_err: Option<HubError> = None;
        for ev in &evidences {
            if let Err(e) = store
                .append_evidence(&NewEvidence {
                    id: Some(ev.id),
                    namespace: ev.namespace.clone(),
                    source_kind: ev.source_kind,
                    source_ref: ev.source_ref.clone(),
                    content_hash: ev.content_hash.clone(),
                    excerpt: ev.excerpt.clone(),
                    occurred_at: ev.occurred_at,
                    expires_at: ev.expires_at,
                })
                .await
            {
                Self::tally_failure(&mut report.failed_evidences, &mut first_err, e);
            }
        }
        Self::warn_restore_failures(
            "evidences",
            &backup.namespace,
            report.failed_evidences,
            &first_err,
        );

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
        let mut first_err: Option<HubError> = None;
        for rel in &relations {
            let rel_ns = if let Some(ref ns) = rel.namespace {
                ns.clone()
            } else if backup.namespace != ALL_NAMESPACES {
                backup.namespace.clone()
            } else {
                entries
                    .iter()
                    .find(|e| e.id == rel.from_memory_id)
                    .map(|e| e.namespace.clone())
                    .unwrap_or_else(|| backup.namespace.clone())
            };
            if let Err(e) = store
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
                .await
            {
                Self::tally_failure(&mut report.failed_relations, &mut first_err, e);
            }
        }
        Self::warn_restore_failures(
            "relations",
            &backup.namespace,
            report.failed_relations,
            &first_err,
        );

        // 恢复任务事实
        let mut first_err: Option<HubError> = None;
        for t in &tasks {
            if let Err(e) = store
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
                .await
            {
                Self::tally_failure(&mut report.failed_tasks, &mut first_err, e);
            }
        }
        Self::warn_restore_failures("tasks", &backup.namespace, report.failed_tasks, &first_err);

        // 恢复经验记忆
        let mut first_err: Option<HubError> = None;
        for exp in &experiences {
            if let Err(e) = store
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
                .await
            {
                Self::tally_failure(&mut report.failed_experiences, &mut first_err, e);
            }
        }
        Self::warn_restore_failures(
            "experiences",
            &backup.namespace,
            report.failed_experiences,
            &first_err,
        );

        // 恢复任务-经验关联映射
        let mut first_err: Option<HubError> = None;
        for mut te in task_experiences {
            if te.namespace.is_none() {
                let inferred = tasks
                    .iter()
                    .find(|t| t.id == te.task_id)
                    .map(|t| t.namespace.clone())
                    .or_else(|| {
                        (backup.namespace != ALL_NAMESPACES).then(|| backup.namespace.clone())
                    });
                te.namespace = inferred;
            }
            if let Err(e) = store.link_task_experience(&te).await {
                Self::tally_failure(&mut report.failed_task_links, &mut first_err, e);
            }
        }
        Self::warn_restore_failures(
            "task_experiences",
            &backup.namespace,
            report.failed_task_links,
            &first_err,
        );

        // 恢复代码演进线程
        let mut first_err: Option<HubError> = None;
        for th in &coding_threads {
            if let Err(e) = store
                .create_thread(&NewCodingThread {
                    id: Some(th.id),
                    namespace: th.namespace.clone(),
                    repository: th.repository.clone(),
                    branch: th.branch.clone(),
                    task_id: th.task_id,
                    title: th.title.clone(),
                    description: th.description.clone(),
                })
                .await
            {
                Self::tally_failure(&mut report.failed_threads, &mut first_err, e);
            }
        }
        Self::warn_restore_failures(
            "coding_threads",
            &backup.namespace,
            report.failed_threads,
            &first_err,
        );

        // 恢复代码修订记录
        let mut first_err: Option<HubError> = None;
        for rev in &coding_revisions {
            if let Err(e) = store
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
                .await
            {
                Self::tally_failure(&mut report.failed_revisions, &mut first_err, e);
            }
        }
        Self::warn_restore_failures(
            "coding_revisions",
            &backup.namespace,
            report.failed_revisions,
            &first_err,
        );

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
        BackupStore, Embedder, EvidenceStore, GrantStore, MemoryStore, RelationStore,
    };
    use crate::testing::{TestEmbedder, TestHubStore};
    use common::{AgentId, Scope};

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

    #[tokio::test]
    async fn test_v1_to_v4_historical_sample_restore_and_checksum_verification() {
        let (svc, store) = setup();
        let a = admin("admin-agent");
        let ns = "hist-ns";

        store
            .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns(ns),
                agent_id: "admin-agent".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "admin-agent".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        // 1. 构造历史 v1 备份样本 (只有 entries)
        let mut entry_v1 = store.seed(&scoped_ns(ns), "admin-agent", "历史 v1 记忆事实");
        entry_v1.namespace = ns.to_string();
        let v1_checksum = DefaultBackupService::checksum_v1(&[entry_v1.clone()]);
        let v1_payload = serde_json::json!({
            "version": 1,
            "entries": [entry_v1.clone()]
        });
        let backup_v1 = BackupRecord {
            id: Uuid::new_v4(),
            namespace: scoped_ns(ns),
            created_by: "admin-agent".to_string(),
            note: Some("v1-legacy".to_string()),
            entry_count: 1,
            checksum: v1_checksum,
            payload: v1_payload,
            retain_until: None,
            created_at: chrono::Utc::now(),
        };
        store.save_backup(&backup_v1).await.unwrap();

        // DryRun 验证
        let rep_v1_dry = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: backup_v1.id,
                    mode: RestoreMode::DryRun,
                },
            )
            .await
            .unwrap();
        assert_eq!(rep_v1_dry.mode, RestoreMode::DryRun);
        assert_eq!(rep_v1_dry.total, 1);

        // 2. 构造历史 v2 备份样本 (entries + evidences + relations)
        let ev_v2 = MemoryEvidence {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            source_kind: EvidenceSourceKind::Session,
            source_ref: Some("msg-001".to_string()),
            content_hash: "hash001".to_string(),
            excerpt: Some("用户询问架构配置".to_string()),
            occurred_at: Some(chrono::Utc::now()),
            created_at: chrono::Utc::now(),
            expires_at: None,
        };
        let rel_v2 = MemoryRelation {
            namespace: Some(ns.to_string()),
            from_memory_id: entry_v1.id,
            to_memory_id: entry_v1.id,
            relation_kind: MemoryRelationKind::DerivedFrom,
            confidence: 0.95,
            source_evidence_id: Some(ev_v2.id),
            created_at: chrono::Utc::now(),
        };
        let v2_checksum = DefaultBackupService::checksum_v2(
            std::slice::from_ref(&entry_v1),
            std::slice::from_ref(&ev_v2),
            std::slice::from_ref(&rel_v2),
        );
        let v2_payload = serde_json::json!({
            "version": 2,
            "entries": [entry_v1],
            "evidences": [ev_v2],
            "relations": [rel_v2],
        });
        let backup_v2 = BackupRecord {
            id: Uuid::new_v4(),
            namespace: scoped_ns(ns),
            created_by: "admin-agent".to_string(),
            note: Some("v2-legacy".to_string()),
            entry_count: 1,
            checksum: v2_checksum,
            payload: v2_payload,
            retain_until: None,
            created_at: chrono::Utc::now(),
        };
        store.save_backup(&backup_v2).await.unwrap();

        // 3. 构造历史 v3 备份样本 (v2 + tasks + experiences + task_experiences)
        let task_v3 = MemoryTask {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            scene: crate::domain::MemoryScene::Working,
            task: "审计历史备份恢复".to_string(),
            status: TaskStatus::Completed,
            started_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            result_summary: Some("审计通过".to_string()),
            failure_reason: None,
            source_evidence_id: None,
            metadata: serde_json::json!({}),
        };
        let exp_v3 = MemoryExperience {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            task: "备份校验".to_string(),
            search_keys: vec!["backup".to_string()],
            solution: "按版本计算校验和".to_string(),
            preconditions: serde_json::json!([]),
            boundary_envs: vec![],
            failure_modes: vec![],
            validation: serde_json::json!("成功"),
            confidence: 1.0,
            status: ExperienceStatus::Active,
            feedback_score: 1.0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let te_v3 = MemoryTaskExperience {
            task_id: task_v3.id,
            experience_id: exp_v3.id,
            namespace: Some(ns.to_string()),
            relation_kind: "derived_from".to_string(),
            created_at: chrono::Utc::now(),
        };
        let v3_checksum = DefaultBackupService::checksum_v3(
            &[],
            &[],
            &[],
            std::slice::from_ref(&task_v3),
            std::slice::from_ref(&exp_v3),
            std::slice::from_ref(&te_v3),
        );
        let v3_payload = serde_json::json!({
            "version": 3,
            "entries": [],
            "tasks": [task_v3],
            "experiences": [exp_v3],
            "task_experiences": [te_v3],
        });
        let backup_v3 = BackupRecord {
            id: Uuid::new_v4(),
            namespace: scoped_ns(ns),
            created_by: "admin-agent".to_string(),
            note: Some("v3-legacy".to_string()),
            entry_count: 0,
            checksum: v3_checksum,
            payload: v3_payload,
            retain_until: None,
            created_at: chrono::Utc::now(),
        };
        store.save_backup(&backup_v3).await.unwrap();

        // 4. 恢复 v3 备份（Merge 模式）
        let rep_v3 = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: backup_v3.id,
                    mode: RestoreMode::Merge,
                },
            )
            .await
            .unwrap();
        assert_eq!(rep_v3.mode, RestoreMode::Merge);

        // 验证 task 和 experience 成功恢复
        let recovered_task = store.get_task(&scoped_ns(ns), task_v3.id).await.unwrap();
        assert!(recovered_task.is_some());
        assert_eq!(recovered_task.unwrap().task, "审计历史备份恢复");

        let recovered_exp = store
            .get_experience(&scoped_ns(ns), exp_v3.id)
            .await
            .unwrap();
        assert!(recovered_exp.is_some());
        assert_eq!(recovered_exp.unwrap().solution, "按版本计算校验和");
    }

    #[tokio::test]
    async fn test_backup_replace_purge_safety_backup_and_rollback() {
        let (svc, store) = setup();
        let a = admin("admin-agent");
        let ns = "replace-ns";

        store
            .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns(ns),
                agent_id: "admin-agent".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "admin-agent".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        // 阶段 1：初始状态，写入一条记忆与任务
        let initial_entry = store.seed(&scoped_ns(ns), "admin-agent", "初始重要数据");
        let initial_task = store
            .create_task(&NewMemoryTask {
                id: None,
                namespace: scoped_ns(ns),
                scene: crate::domain::MemoryScene::Working,
                task: "初始任务".to_string(),
                status: TaskStatus::Running,
                started_at: chrono::Utc::now(),
                finished_at: None,
                result_summary: None,
                failure_reason: None,
                source_evidence_id: None,
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();

        // 阶段 2：准备一份快照（仅包含新条目）
        let new_entry_id = Uuid::new_v4();
        let new_entry = MemoryEntry {
            id: new_entry_id,
            namespace: ns.to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: "全新的快照数据".to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "admin-agent".to_string(),
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
        let checksum = DefaultBackupService::checksum_v1(std::slice::from_ref(&new_entry));
        let snapshot_backup = BackupRecord {
            id: Uuid::new_v4(),
            namespace: scoped_ns(ns),
            created_by: "admin-agent".to_string(),
            note: Some("snapshot-to-replace".to_string()),
            entry_count: 1,
            checksum,
            payload: serde_json::json!({
                "version": 1,
                "entries": [new_entry]
            }),
            retain_until: None,
            created_at: chrono::Utc::now(),
        };
        store.save_backup(&snapshot_backup).await.unwrap();

        // 阶段 3：执行 Replace 恢复
        let rep = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: snapshot_backup.id,
                    mode: RestoreMode::Replace,
                },
            )
            .await
            .unwrap();

        // 验证生成了 safety backup
        assert!(rep.safety_backup_id.is_some());
        let safety_id = rep.safety_backup_id.unwrap();

        // 验证旧数据已被彻底清除
        assert!(store.get(&scoped_ns(ns), initial_entry.id).await.is_err());
        assert!(store
            .get_task(&scoped_ns(ns), initial_task.id)
            .await
            .unwrap()
            .is_none());

        // 验证新数据已生效
        let cur = store.get(&scoped_ns(ns), new_entry_id).await.unwrap();
        assert_eq!(cur.content, "全新的快照数据");

        // 阶段 4：模拟故障回滚，通过 safety backup 再次执行 Replace 恢复！
        let rollback_rep = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: safety_id,
                    mode: RestoreMode::Replace,
                },
            )
            .await
            .unwrap();
        assert_eq!(rollback_rep.mode, RestoreMode::Replace);

        // 验证回滚后，初始数据完全复原！
        let restored_initial = store.get(&scoped_ns(ns), initial_entry.id).await.unwrap();
        assert_eq!(restored_initial.content, "初始重要数据");
        let restored_task = store
            .get_task(&scoped_ns(ns), initial_task.id)
            .await
            .unwrap();
        assert!(restored_task.is_some());
        assert_eq!(restored_task.unwrap().task, "初始任务");

        // 验证被替换的新数据已不存在
        assert!(store.get(&scoped_ns(ns), new_entry_id).await.is_err());
    }

    #[tokio::test]
    async fn test_corrupted_checksum_fails_restore() {
        let (svc, store) = setup();
        let a = admin("admin-agent");
        let ns = "corrupt-ns";

        store
            .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns(ns),
                agent_id: "admin-agent".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "admin-agent".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        let corrupted_backup = BackupRecord {
            id: Uuid::new_v4(),
            namespace: scoped_ns(ns),
            created_by: "admin-agent".to_string(),
            note: Some("corrupted".to_string()),
            entry_count: 1,
            checksum: "invalid_tampered_checksum".to_string(),
            payload: serde_json::json!({
                "version": 1,
                "entries": []
            }),
            retain_until: None,
            created_at: chrono::Utc::now(),
        };
        store.save_backup(&corrupted_backup).await.unwrap();

        let err = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: corrupted_backup.id,
                    mode: RestoreMode::Merge,
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, HubError::Internal(_)),
            "校验和篡改必须直接拒绝"
        );
    }

    #[tokio::test]
    async fn test_restore_counts_partial_failures_and_continues() {
        // 替身 store：第一条 relation 的 add_relation 返回 Err，
        // 其余操作全部透传内层存储——验证恢复不中止、失败计数准确。
        #[derive(Default)]
        struct FailingRelationStore {
            inner: crate::testing::TestHubStore,
            fail_relation_payloads: std::sync::Mutex<std::collections::HashSet<String>>,
        }

        #[async_trait]
        impl MemoryStore for FailingRelationStore {
            async fn insert(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
                self.inner.insert(e).await
            }
            async fn insert_initial(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
                self.inner.insert_initial(e).await
            }
            async fn get(&self, ns: &str, id: Uuid) -> Result<MemoryEntry> {
                self.inner.get(ns, id).await
            }
            async fn get_by_key(&self, ns: &str, key: &str) -> Result<Option<MemoryEntry>> {
                self.inner.get_by_key(ns, key).await
            }
            async fn save(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
                self.inner.save(e).await
            }
            async fn delete(&self, ns: &str, id: Uuid) -> Result<()> {
                self.inner.delete(ns, id).await
            }
            async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
                self.inner.list(filter).await
            }
            async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
                self.inner.count(filter).await
            }
            async fn similar(
                &self,
                filter: &MemoryFilter,
                qv: &[f32],
                k: usize,
            ) -> Result<Vec<crate::domain::Scored<MemoryEntry>>> {
                self.inner.similar(filter, qv, k).await
            }
            async fn all_in_namespace(
                &self,
                ns: &str,
                include_merged: bool,
            ) -> Result<Vec<MemoryEntry>> {
                self.inner.all_in_namespace(ns, include_merged).await
            }
            async fn list_namespaces(&self) -> Result<Vec<String>> {
                self.inner.list_namespaces().await
            }
            async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
                self.inner.record_version(v).await
            }
            async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
                self.inner.versions(id).await
            }
            async fn archive_expired(&self, _now: chrono::DateTime<chrono::Utc>) -> Result<usize> {
                Ok(0)
            }
            async fn ping(&self) -> Result<()> {
                self.inner.ping().await
            }
            async fn save_if_version(
                &self,
                entry: &MemoryEntry,
                expected_version: i64,
            ) -> Result<MemoryEntry> {
                self.inner.save_if_version(entry, expected_version).await
            }
            async fn revise(
                &self,
                entry: &MemoryEntry,
                expected_version: i64,
                change: VersionChange,
            ) -> Result<MemoryEntry> {
                self.inner.revise(entry, expected_version, change).await
            }
        }

        #[async_trait]
        impl crate::infra::BackupStore for FailingRelationStore {
            async fn save_backup(&self, b: &BackupRecord) -> Result<()> {
                self.inner.save_backup(b).await
            }
            async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
                self.inner.get_backup(id).await
            }
            async fn list_backups(&self, ns: Option<&str>) -> Result<Vec<BackupRecord>> {
                self.inner.list_backups(ns).await
            }
            async fn delete_backup(&self, id: Uuid) -> Result<()> {
                self.inner.delete_backup(id).await
            }
        }

        #[async_trait]
        impl crate::infra::IntegrationStore for FailingRelationStore {
            async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
                self.inner.save_proposal(p).await
            }
            async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
                self.inner.get_proposal(id).await
            }
            async fn list_proposals(
                &self,
                ns: &str,
                status: Option<ProposalStatus>,
            ) -> Result<Vec<IntegrationProposal>> {
                self.inner.list_proposals(ns, status).await
            }
            async fn open_exists(&self, ns: &str, members: &[(String, Vec<Uuid>)]) -> Result<bool> {
                self.inner.open_exists(ns, members).await
            }
        }

        #[async_trait]
        impl crate::infra::CognitiveProposalStore for FailingRelationStore {
            async fn save_cognitive_proposal(
                &self,
                p: &crate::domain::CognitiveProposalRecord,
            ) -> Result<()> {
                self.inner.save_cognitive_proposal(p).await
            }
            async fn get_cognitive_proposal(
                &self,
                id: Uuid,
            ) -> Result<crate::domain::CognitiveProposalRecord> {
                self.inner.get_cognitive_proposal(id).await
            }
            async fn list_cognitive_proposals(
                &self,
                ns: &str,
                status: Option<crate::domain::CognitiveProposalStatus>,
            ) -> Result<Vec<crate::domain::CognitiveProposalRecord>> {
                self.inner.list_cognitive_proposals(ns, status).await
            }
            async fn update_cognitive_proposal_status(
                &self,
                id: Uuid,
                expected_status: crate::domain::CognitiveProposalStatus,
                new_status: crate::domain::CognitiveProposalStatus,
                rejection_reason: Option<&str>,
                resulting_memory_id: Option<Uuid>,
                resolved_by: Option<&str>,
            ) -> Result<crate::domain::CognitiveProposalRecord> {
                self.inner
                    .update_cognitive_proposal_status(
                        id,
                        expected_status,
                        new_status,
                        rejection_reason,
                        resulting_memory_id,
                        resolved_by,
                    )
                    .await
            }
        }

        #[async_trait]
        impl crate::infra::GrantStore for FailingRelationStore {
            async fn ensure_namespace(
                &self,
                ns: &str,
                owner: &AgentId,
            ) -> Result<(NamespaceInfo, bool)> {
                self.inner.ensure_namespace(ns, owner).await
            }
            async fn get_namespace(&self, ns: &str) -> Result<Option<NamespaceInfo>> {
                self.inner.get_namespace(ns).await
            }
            async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
                self.inner.list_namespace_infos().await
            }
            async fn grant(&self, r: &NamespaceGrantRecord) -> Result<()> {
                self.inner.grant(r).await
            }
            async fn revoke(&self, ns: &str, agent: &AgentId) -> Result<()> {
                self.inner.revoke(ns, agent).await
            }
            async fn grants_for(&self, agent: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
                self.inner.grants_for(agent).await
            }
            async fn can_read(&self, ns: &str, agent: &AgentId) -> Result<bool> {
                self.inner.can_read(ns, agent).await
            }
            async fn can_write(&self, ns: &str, agent: &AgentId) -> Result<bool> {
                self.inner.can_write(ns, agent).await
            }
            async fn owned_namespaces(&self, agent: &AgentId) -> Result<Vec<String>> {
                self.inner.owned_namespaces(agent).await
            }
        }

        #[async_trait]
        impl crate::infra::AgentStore for FailingRelationStore {
            async fn register_agent(
                &self,
                reg: &crate::infra::AgentRegistration,
            ) -> Result<crate::infra::AgentRecord> {
                self.inner.register_agent(reg).await
            }
            async fn get_agent(
                &self,
                agent_id: &AgentId,
            ) -> Result<Option<crate::infra::AgentRecord>> {
                self.inner.get_agent(agent_id).await
            }
            async fn list_agents(&self) -> Result<Vec<crate::infra::AgentRecord>> {
                self.inner.list_agents().await
            }
            async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
                self.inner.set_agent_status(agent_id, status).await
            }
            async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
                self.inner.verify_agent_secret(agent_id, secret).await
            }
        }

        #[async_trait]
        impl EvidenceStore for FailingRelationStore {
            async fn append_evidence(&self, ev: &NewEvidence) -> Result<MemoryEvidence> {
                self.inner.append_evidence(ev).await
            }
            async fn get_evidence(&self, ns: &str, id: Uuid) -> Result<MemoryEvidence> {
                self.inner.get_evidence(ns, id).await
            }
            async fn list_evidence(
                &self,
                ns: &str,
                source_kind: Option<EvidenceSourceKind>,
                source_ref: Option<&str>,
                limit: usize,
            ) -> Result<Vec<MemoryEvidence>> {
                self.inner
                    .list_evidence(ns, source_kind, source_ref, limit)
                    .await
            }
            async fn all_evidence_in_namespace(&self, ns: &str) -> Result<Vec<MemoryEvidence>> {
                self.inner.all_evidence_in_namespace(ns).await
            }
            async fn delete_evidence(&self, ns: &str, id: Uuid) -> Result<()> {
                self.inner.delete_evidence(ns, id).await
            }
            async fn purge_expired_evidence(
                &self,
                ns: &str,
                now: chrono::DateTime<chrono::Utc>,
            ) -> Result<usize> {
                self.inner.purge_expired_evidence(ns, now).await
            }
        }

        #[async_trait]
        impl RelationStore for FailingRelationStore {
            async fn add_relation(
                &self,
                ns: &str,
                rel: &NewMemoryRelation,
            ) -> Result<MemoryRelation> {
                let key = format!(
                    "{}|{}|{}|{:?}",
                    ns, rel.from_memory_id, rel.to_memory_id, rel.relation_kind
                );
                if self.fail_relation_payloads.lock().unwrap().contains(&key) {
                    return Err(HubError::Unavailable);
                }
                self.inner.add_relation(ns, rel).await
            }
            async fn list_relations_from(
                &self,
                ns: &str,
                from_memory_id: Uuid,
            ) -> Result<Vec<MemoryRelation>> {
                self.inner.list_relations_from(ns, from_memory_id).await
            }
            async fn list_relations_to(
                &self,
                ns: &str,
                to_memory_id: Uuid,
            ) -> Result<Vec<MemoryRelation>> {
                self.inner.list_relations_to(ns, to_memory_id).await
            }
            async fn delete_relation(
                &self,
                ns: &str,
                from_memory_id: Uuid,
                to_memory_id: Uuid,
                relation_kind: MemoryRelationKind,
            ) -> Result<()> {
                self.inner
                    .delete_relation(ns, from_memory_id, to_memory_id, relation_kind)
                    .await
            }
            async fn all_relations_in_namespace(&self, ns: &str) -> Result<Vec<MemoryRelation>> {
                self.inner.all_relations_in_namespace(ns).await
            }
        }

        #[async_trait]
        impl TaskStore for FailingRelationStore {
            async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask> {
                self.inner.create_task(task).await
            }
            async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
                self.inner.update_task(task).await
            }
            async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
                self.inner.get_task(namespace, id).await
            }
            async fn list_tasks(
                &self,
                namespace: &str,
                status: Option<TaskStatus>,
                limit: usize,
            ) -> Result<Vec<MemoryTask>> {
                self.inner.list_tasks(namespace, status, limit).await
            }
            async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
                self.inner.all_tasks_in_namespace(namespace).await
            }
            async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
                self.inner.delete_task(namespace, id).await
            }
        }

        #[async_trait]
        impl ExperienceStore for FailingRelationStore {
            async fn save_experience(
                &self,
                experience: &NewMemoryExperience,
            ) -> Result<MemoryExperience> {
                self.inner.save_experience(experience).await
            }
            async fn update_experience(
                &self,
                experience: &MemoryExperience,
            ) -> Result<MemoryExperience> {
                self.inner.update_experience(experience).await
            }
            async fn get_experience(
                &self,
                namespace: &str,
                id: Uuid,
            ) -> Result<Option<MemoryExperience>> {
                self.inner.get_experience(namespace, id).await
            }
            async fn list_experiences(
                &self,
                namespace: &str,
                status: Option<ExperienceStatus>,
                limit: usize,
                offset: usize,
            ) -> Result<Vec<MemoryExperience>> {
                self.inner
                    .list_experiences(namespace, status, limit, offset)
                    .await
            }
            async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
                self.inner.link_task_experience(link).await
            }
            async fn list_experiences_for_task(
                &self,
                task_id: Uuid,
            ) -> Result<Vec<MemoryTaskExperience>> {
                self.inner.list_experiences_for_task(task_id).await
            }
            async fn all_experiences_in_namespace(
                &self,
                namespace: &str,
            ) -> Result<Vec<MemoryExperience>> {
                self.inner.all_experiences_in_namespace(namespace).await
            }
            async fn all_task_experiences_in_namespace(
                &self,
                namespace: &str,
            ) -> Result<Vec<MemoryTaskExperience>> {
                self.inner
                    .all_task_experiences_in_namespace(namespace)
                    .await
            }
            async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
                self.inner.delete_experience(namespace, id).await
            }
        }

        #[async_trait]
        impl CodingStore for FailingRelationStore {
            async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
                self.inner.create_thread(thread).await
            }
            async fn get_thread(&self, namespace: &str, id: Uuid) -> Result<Option<CodingThread>> {
                self.inner.get_thread(namespace, id).await
            }
            async fn list_threads(
                &self,
                namespace: &str,
                repository: Option<&str>,
                limit: usize,
            ) -> Result<Vec<CodingThread>> {
                self.inner.list_threads(namespace, repository, limit).await
            }
            async fn save_revision(
                &self,
                revision: &NewCodingRevision,
                diff_hash: &str,
            ) -> Result<CodingRevision> {
                self.inner.save_revision(revision, diff_hash).await
            }
            async fn get_revision(
                &self,
                namespace: &str,
                id: Uuid,
            ) -> Result<Option<CodingRevision>> {
                self.inner.get_revision(namespace, id).await
            }
            async fn get_revision_by_hash(
                &self,
                namespace: &str,
                repository: &str,
                diff_hash: &str,
            ) -> Result<Option<CodingRevision>> {
                self.inner
                    .get_revision_by_hash(namespace, repository, diff_hash)
                    .await
            }
            async fn list_revisions_for_thread(
                &self,
                namespace: &str,
                thread_id: Uuid,
                limit: usize,
            ) -> Result<Vec<CodingRevision>> {
                self.inner
                    .list_revisions_for_thread(namespace, thread_id, limit)
                    .await
            }
            async fn list_revisions_for_file(
                &self,
                namespace: &str,
                repository: &str,
                file_path: &str,
                limit: usize,
            ) -> Result<Vec<CodingRevision>> {
                self.inner
                    .list_revisions_for_file(namespace, repository, file_path, limit)
                    .await
            }
            async fn all_threads_in_namespace(&self, namespace: &str) -> Result<Vec<CodingThread>> {
                self.inner.all_threads_in_namespace(namespace).await
            }
            async fn all_revisions_in_namespace(
                &self,
                namespace: &str,
            ) -> Result<Vec<CodingRevision>> {
                self.inner.all_revisions_in_namespace(namespace).await
            }
            async fn delete_thread(&self, namespace: &str, id: Uuid) -> Result<()> {
                self.inner.delete_thread(namespace, id).await
            }
            async fn delete_revision(&self, namespace: &str, id: Uuid) -> Result<()> {
                self.inner.delete_revision(namespace, id).await
            }
        }

        let inner_store = Arc::new(FailingRelationStore {
            inner: crate::testing::TestHubStore::default(),
            fail_relation_payloads: std::sync::Mutex::new(std::collections::HashSet::new()),
        });
        let svc = DefaultBackupService::new(
            inner_store.clone(),
            Arc::new(GrantChecker::new(inner_store.clone())),
        );
        let a = admin("admin-agent");
        let ns = "partial-fail-ns";

        inner_store
            .inner
            .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
            .await
            .unwrap();
        inner_store
            .inner
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns(ns),
                agent_id: "admin-agent".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "admin-agent".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        // 快照：1 条记忆 + 1 个任务 + 2 条关系（一条将被 store 拒绝）
        // 实体命名空间用用户可见名，与服务层 create 产出的 v4 载荷同构
        let mut entry = inner_store
            .inner
            .seed(&scoped_ns(ns), "admin-agent", "记忆");
        entry.namespace = ns.to_string();
        let task = MemoryTask {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            scene: crate::domain::MemoryScene::Working,
            task: "关系失败后仍要恢复的任务".to_string(),
            status: TaskStatus::Completed,
            started_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            result_summary: None,
            failure_reason: None,
            source_evidence_id: None,
            metadata: serde_json::json!({}),
        };
        let rel_ok = MemoryRelation {
            namespace: Some(ns.to_string()),
            from_memory_id: entry.id,
            to_memory_id: entry.id,
            relation_kind: MemoryRelationKind::DerivedFrom,
            confidence: 0.9,
            source_evidence_id: None,
            created_at: chrono::Utc::now(),
        };
        let rel_fail = MemoryRelation {
            namespace: Some(ns.to_string()),
            from_memory_id: entry.id,
            to_memory_id: entry.id,
            relation_kind: MemoryRelationKind::Supersedes,
            confidence: 0.8,
            source_evidence_id: None,
            created_at: chrono::Utc::now(),
        };
        let checksum = DefaultBackupService::checksum_v4(
            &[entry.clone()],
            &[],
            &[rel_ok.clone(), rel_fail.clone()],
            std::slice::from_ref(&task),
            &[],
            &[],
            &[],
            &[],
        );
        let backup = BackupRecord {
            payload: serde_json::json!({
                "version": BACKUP_PAYLOAD_VERSION,
                "namespace": ns,
                "entries": [entry],
                "evidences": [],
                "relations": [rel_ok, rel_fail],
                "tasks": [task],
                "experiences": [],
                "task_experiences": [],
                "coding_threads": [],
                "coding_revisions": [],
            }),
            checksum,
            ..make_record(scoped_ns(ns))
        };
        inner_store.save_backup(&backup).await.unwrap();

        // 删掉当前记忆，让恢复必须真正写回（而不是 unchanged）
        inner_store
            .inner
            .delete(&scoped_ns(ns), entry.id)
            .await
            .unwrap();

        // 预置失败：指定那条 Supersedes 关系在恢复时被 store 拒绝
        {
            let failing_key = format!(
                "{}|{}|{}|{:?}",
                scoped_ns(ns),
                entry.id,
                entry.id,
                MemoryRelationKind::Supersedes
            );
            inner_store
                .fail_relation_payloads
                .lock()
                .unwrap()
                .insert(failing_key);
        }

        let report = svc
            .restore(
                &a,
                RestoreRequest {
                    backup_id: backup.id,
                    mode: RestoreMode::Merge,
                },
            )
            .await
            .expect("单条关系失败不得中止恢复");
        assert_eq!(report.failed_relations, 1, "失败的关系应被准确计数");
        assert_eq!(report.failed_evidences, 0);
        assert_eq!(report.created, 1, "记忆条目仍应恢复成功");

        // 成功的那条关系确实恢复了
        let relations = inner_store
            .inner
            .all_relations_in_namespace(&scoped_ns(ns))
            .await
            .unwrap();
        assert_eq!(relations.len(), 1, "另一条关系恢复成功，仅失败那条被跳过");
        assert_eq!(relations[0].relation_kind, MemoryRelationKind::DerivedFrom);

        // 关系失败之后恢复的任务也落库了：证明失败不中止后续实体恢复
        let restored_task = inner_store
            .inner
            .get_task(&scoped_ns(ns), task.id)
            .await
            .unwrap();
        assert!(
            restored_task.is_some(),
            "关系失败后，排在后面的任务实体仍应恢复"
        );
    }

    fn make_record(ns: String) -> BackupRecord {
        BackupRecord {
            id: Uuid::new_v4(),
            namespace: ns,
            created_by: "admin-agent".to_string(),
            note: None,
            entry_count: 0,
            checksum: String::new(),
            payload: serde_json::json!({}),
            retain_until: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_replace_multi_namespace_records_all_safety_backup_ids() {
        let (svc, store) = setup();
        let a = admin("admin-agent");

        // 两个命名空间，各有数据；全量快照 namespace = "*"
        for ns in ["multi-a", "multi-b"] {
            store
                .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
                .await
                .unwrap();
            store
                .grant(&NamespaceGrantRecord {
                    namespace: scoped_ns(ns),
                    agent_id: "admin-agent".to_string(),
                    can_read: true,
                    can_write: true,
                    granted_by: "admin-agent".to_string(),
                    created_at: chrono::Utc::now(),
                })
                .await
                .unwrap();
            store.seed(&scoped_ns(ns), "admin-agent", &format!("{ns} 数据"));
        }

        let backup = svc
            .create(
                &a,
                BackupCreateRequest {
                    namespace: ALL_NAMESPACES.to_string(),
                    note: None,
                },
            )
            .await
            .unwrap();

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

        assert_eq!(
            report.safety_backup_ids.len(),
            2,
            "两个受影响命名空间都应各有一条安全备份 ID"
        );
        assert_eq!(
            report.safety_backup_id,
            Some(report.safety_backup_ids[0]),
            "旧单数字段取第一个，避免下游消费方断裂"
        );
        // 两条安全备份都真实存在且可读取
        for id in &report.safety_backup_ids {
            assert!(store.get_backup(*id).await.is_ok());
        }
    }

    #[test]
    fn test_restore_report_deserializes_legacy_json_without_new_fields() {
        // 旧版本报告没有失败计数与 safety_backup_ids 字段，反序列化必须兼容
        let legacy = serde_json::json!({
            "backup_id": Uuid::new_v4().to_string(),
            "mode": "merge",
            "total": 3,
            "created": 1,
            "updated": 1,
            "unchanged": 1,
            "safety_backup_id": Uuid::new_v4().to_string(),
        });
        let report: RestoreReport = serde_json::from_value(legacy).unwrap();
        assert_eq!(report.failed_relations, 0);
        assert!(report.safety_backup_ids.is_empty());
    }

    // 编译期检查：Embedder trait 对象可用
    #[allow(dead_code)]
    fn assert_embedder_object() {
        let _: Arc<dyn Embedder> = Arc::new(TestEmbedder::default());
    }
}
