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
    NewCodingThread, NewEvidence, NewExperienceVersion, NewMemoryExperience, NewMemoryRelation,
    NewMemoryTask, RestoreMode, RestoreReport, VersionChange,
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
pub const BACKUP_PAYLOAD_VERSION: u32 = 5;

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

    /// 从负载取出经验版本历史（v5+；旧备份无此键时为空）
    fn experience_versions_of(
        backup: &BackupRecord,
    ) -> Result<Vec<crate::domain::ExperienceVersion>> {
        match backup.payload.get("experience_versions") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                HubError::Internal(anyhow::anyhow!(
                    "备份负载 experience_versions 解析失败: {}",
                    e
                ))
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

    /// 计算 v5 负载的规范校验和（v4 + 经验版本历史）
    #[allow(clippy::too_many_arguments)]
    fn checksum_v5(
        entries: &[MemoryEntry],
        evidences: &[MemoryEvidence],
        relations: &[MemoryRelation],
        tasks: &[MemoryTask],
        experiences: &[MemoryExperience],
        experience_versions: &[crate::domain::ExperienceVersion],
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
        hasher.update(serde_json::to_vec(experience_versions).unwrap_or_default());
        hasher.update(serde_json::to_vec(task_experiences).unwrap_or_default());
        hasher.update(serde_json::to_vec(coding_threads).unwrap_or_default());
        hasher.update(serde_json::to_vec(coding_revisions).unwrap_or_default());
        hex::encode(hasher.finalize())
    }

    /// 构建备份负载（v5 格式）
    #[allow(clippy::too_many_arguments)]
    fn payload_of(
        namespace: &str,
        entries: &[MemoryEntry],
        evidences: &[MemoryEvidence],
        relations: &[MemoryRelation],
        tasks: &[MemoryTask],
        experiences: &[MemoryExperience],
        experience_versions: &[crate::domain::ExperienceVersion],
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
            "experience_versions": experience_versions,
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
        let experience_versions = store
            .all_experience_versions_in_namespace(namespace)
            .await?;
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
            checksum: Self::checksum_v5(
                &entries,
                &evidences,
                &relations,
                &tasks,
                &experiences,
                &experience_versions,
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
                &experience_versions,
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
                let experience_versions = scoped
                    .all_experience_versions_in_namespace(&visible_ns)
                    .await?;
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
                    checksum: Self::checksum_v5(
                        &entries,
                        &evidences,
                        &relations,
                        &tasks,
                        &experiences,
                        &experience_versions,
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
                        &experience_versions,
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
                let experience_versions = store
                    .all_experience_versions_in_namespace(namespace)
                    .await?;
                let task_experiences = store.all_task_experiences_in_namespace(namespace).await?;
                let coding_threads = store.all_threads_in_namespace(namespace).await?;
                let coding_revisions = store.all_revisions_in_namespace(namespace).await?;
                let record = BackupRecord {
                    id: Uuid::new_v4(),
                    namespace: namespace.to_string(),
                    created_by: created_by.clone(),
                    note,
                    entry_count: entries.len(),
                    checksum: Self::checksum_v5(
                        &entries,
                        &evidences,
                        &relations,
                        &tasks,
                        &experiences,
                        &experience_versions,
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
                        &experience_versions,
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

        let experience_versions: Vec<crate::domain::ExperienceVersion> = if req.namespace == "*" {
            let mut all = Vec::new();
            for ns in crate::infra::MemoryStore::list_namespaces(&store).await? {
                all.extend(store.all_experience_versions_in_namespace(&ns).await?);
            }
            all
        } else {
            store
                .all_experience_versions_in_namespace(&req.namespace)
                .await?
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
            checksum: Self::checksum_v5(
                &entries,
                &evidences,
                &relations,
                &tasks,
                &experiences,
                &experience_versions,
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
                &experience_versions,
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

        // 完整性校验：重建校验和与写入时比对（兼容 v1–v5）
        let entries = Self::entries_of(&backup)?;
        let evidences = Self::evidences_of(&backup)?;
        let relations = Self::relations_of(&backup)?;
        let tasks = Self::tasks_of(&backup)?;
        let experiences = Self::experiences_of(&backup)?;
        let experience_versions = Self::experience_versions_of(&backup)?;
        let task_experiences = Self::task_experiences_of(&backup)?;
        let coding_threads = Self::coding_threads_of(&backup)?;
        let coding_revisions = Self::coding_revisions_of(&backup)?;

        // 先试 v5：旧备份的 checksum 不匹配 v5，会自然落到 v4–v1
        let actual_v5 = Self::checksum_v5(
            &entries,
            &evidences,
            &relations,
            &tasks,
            &experiences,
            &experience_versions,
            &task_experiences,
            &coding_threads,
            &coding_revisions,
        );
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

        let valid = backup.checksum == actual_v5
            || backup.checksum == actual_v4
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
            failed_experience_versions: 0,
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
            match store
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
                Ok(_) => {
                    // save 只写内容与基础状态；版本号、验证元数据、来源计数与
                    // 被替代指向由完整记录回写，避免恢复后版本历史与实体分叉
                    if let Err(e) = store.update_experience(exp).await {
                        Self::tally_failure(&mut report.failed_experiences, &mut first_err, e);
                    }
                }
                Err(e) => Self::tally_failure(&mut report.failed_experiences, &mut first_err, e),
            }
        }
        Self::warn_restore_failures(
            "experiences",
            &backup.namespace,
            report.failed_experiences,
            &first_err,
        );

        // 恢复经验版本历史（v5+；旧备份为空数组，不报错）
        let mut first_err: Option<HubError> = None;
        for v in &experience_versions {
            if let Err(e) = store
                .append_experience_version(&NewExperienceVersion {
                    id: Some(v.id),
                    experience_id: v.experience_id,
                    namespace: v.namespace.clone(),
                    version: v.version,
                    snapshot: v.snapshot.clone(),
                    change_kind: v.change_kind,
                    changed_by: v.changed_by.clone(),
                    status: v.status,
                    note: v.note.clone(),
                    changed_at: v.changed_at,
                })
                .await
            {
                Self::tally_failure(&mut report.failed_experience_versions, &mut first_err, e);
            }
        }
        Self::warn_restore_failures(
            "experience_versions",
            &backup.namespace,
            report.failed_experience_versions,
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
mod tests;
