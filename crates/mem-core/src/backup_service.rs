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
    BackupRecord, BackupSummary, MemoryEntry, RestoreMode, RestoreReport, VersionChange,
};
use crate::infra::{AccessChecker, BackupStore, Clock, MemoryHubStore, MemoryStore, SystemClock};
use async_trait::async_trait;
use common::{AuthCtx, HubError, Result, Scope};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

/// 备份负载格式版本
pub const BACKUP_PAYLOAD_VERSION: u32 = 1;

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

    /// 计算条目列表的规范校验和
    fn checksum_of(entries: &[MemoryEntry]) -> String {
        let bytes = serde_json::to_vec(entries).unwrap_or_default();
        hex::encode(Sha256::digest(&bytes))
    }

    /// 构建备份负载
    fn payload_of(namespace: &str, entries: &[MemoryEntry]) -> serde_json::Value {
        serde_json::json!({
            "version": BACKUP_PAYLOAD_VERSION,
            "namespace": namespace,
            "entries": entries,
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
        if entries.is_empty() {
            return Ok(None);
        }
        let now = self.clock.now();
        let record = BackupRecord {
            id: Uuid::new_v4(),
            namespace: namespace.to_string(),
            created_by: auth.agent_id.clone(),
            note: Some(format!("restore-safety:{}", namespace)),
            entry_count: entries.len(),
            checksum: Self::checksum_of(&entries),
            payload: Self::payload_of(namespace, &entries),
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

        let now = self.clock.now();
        let record = BackupRecord {
            id: Uuid::new_v4(),
            namespace: req.namespace.clone(),
            created_by: auth.agent_id.clone(),
            note: req.note.clone(),
            entry_count: entries.len(),
            checksum: Self::checksum_of(&entries),
            payload: Self::payload_of(&req.namespace, &entries),
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

        // 完整性校验：重建校验和与写入时比对
        let entries = Self::entries_of(&backup)?;
        let actual = Self::checksum_of(&entries);
        if actual != backup.checksum {
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
            }
        } else {
            for ns in &namespaces {
                self.require_write(auth, ns).await?;
            }
        }

        for mut e in entries {
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

impl DefaultBackupService {
}

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
        IntegrationProposal, MemoryFilter, MemoryKind, MemoryStatus, MemoryVersion,
        NamespaceGrantRecord, NamespaceInfo, ProposalStatus,
    };
    use crate::infra::{BackupStore, Embedder, GrantStore, IntegrationStore, MemoryStore};
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
