//! 定时备份作业：命名空间快照 + 保留策略
//!
//! 作业以系统特权主体运行（不经过 AuthCtx），直接操作存储；
//! 与 MCP 暴露的 `memory_backup_create` 走同一套快照格式与校验和算法
//! （v4 载荷：条目、证据、关系、任务、经验、代码线程与修订记录），
//! 定时快照可作为完整备份参与恢复与保留策略。

use crate::lease::JobLease;
use chrono::{DateTime, Utc};
use common::{AgentId, Result};
use mem_core::{BackupRecord, DefaultBackupService, MemoryHubStore};
use std::sync::Arc;
use tracing::{info, warn};

/// 备份作业
pub struct BackupJob {
    store: Arc<dyn MemoryHubStore>,
    keep: usize,
    retention_days: u32,
    /// 作业执行者标识（记入快照 created_by）
    worker_agent: AgentId,
    /// 跨进程租约（None = 不做互斥）
    lease: Option<JobLease>,
}

/// 单轮备份报告
#[derive(Debug, Clone, Default)]
pub struct BackupJobReport {
    /// 本轮创建的快照（(backup_id, namespace)）
    pub created: Vec<(uuid::Uuid, String)>,
    /// 保留策略删除的快照数与 ID
    pub pruned: Vec<uuid::Uuid>,
    /// 本轮是否因租约被其它副本占用而整体跳过
    pub skipped_leased: bool,
}

impl BackupJob {
    pub fn new(
        store: Arc<dyn MemoryHubStore>,
        keep: usize,
        retention_days: u32,
        worker_agent: impl Into<String>,
    ) -> Self {
        Self {
            store,
            keep: keep.max(1),
            retention_days,
            worker_agent: worker_agent.into(),
            lease: None,
        }
    }

    /// 注入跨进程租约：同一租约窗口内只有一个副本执行定时备份
    pub fn with_lease(mut self, lease: JobLease) -> Self {
        self.lease = Some(lease);
        self
    }

    /// 为每个已注册命名空间创建快照，并执行保留策略
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<BackupJobReport> {
        // 跨进程互斥：多副本部署时只让一个副本真正执行备份
        if let Some(lease) = &self.lease {
            if !lease.try_acquire(&JobLease::global_key("backup")).await? {
                warn!("备份租约被其它副本占用，本轮跳过");
                return Ok(BackupJobReport {
                    skipped_leased: true,
                    ..Default::default()
                });
            }
        }

        info!(worker = %self.worker_agent, "开始执行定时备份作业");

        let mut report = BackupJobReport::default();
        let namespaces = mem_core::GrantStore::list_namespace_infos(self.store.as_ref()).await?;

        for info in namespaces {
            // v4 全实体快照：与 DefaultBackupService::create 同一载荷格式与校验和算法
            // （system_snapshot 内部完成收集与落库）
            let record = DefaultBackupService::system_snapshot(
                &self.store,
                &info.namespace,
                &self.worker_agent,
                Some("scheduled-backup".to_string()),
                now,
            )
            .await?;
            info!(
                backup_id = %record.id,
                namespace = %record.namespace,
                entries = record.entry_count,
                "已创建定时备份"
            );
            report.created.push((record.id, record.namespace));
        }

        report.pruned = self.prune(now).await?;
        info!(pruned = report.pruned.len(), "备份保留策略执行完成");

        Ok(report)
    }

    /// 保留策略：每命名空间保留最新 `keep` 份；超 TTL 的非最新快照淘汰
    pub async fn prune(&self, now: DateTime<Utc>) -> Result<Vec<uuid::Uuid>> {
        let all = mem_core::BackupStore::list_backups(self.store.as_ref(), None).await?;
        let mut by_ns: std::collections::HashMap<String, Vec<BackupRecord>> =
            std::collections::HashMap::new();
        for b in all {
            by_ns.entry(b.namespace.clone()).or_default().push(b);
        }

        let mut deleted = Vec::new();
        for records in by_ns.values_mut() {
            records.sort_by_key(|b| std::cmp::Reverse(b.created_at));
            for (idx, record) in records.iter().enumerate() {
                let beyond_count = idx >= self.keep;
                let beyond_ttl = self.retention_days > 0
                    && idx >= 1
                    && record.created_at + chrono::Duration::days(i64::from(self.retention_days))
                        <= now;
                if beyond_count || beyond_ttl {
                    self.store.delete_backup(record.id).await?;
                    deleted.push(record.id);
                }
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{AuthCtx, Scope, BOOTSTRAP_ACCOUNT_ID};
    use mem_core::account_scope::scope_name;
    use mem_core::domain::RestoreMode;
    use mem_core::domain::{
        EvidenceSourceKind, MemoryRelationKind, NewEvidence, NewMemoryRelation,
    };
    use mem_core::infra::{EvidenceStore, RelationStore};
    use mem_core::{
        BackupService, GrantChecker, GrantStore, MemoryStore, NamespaceGrantRecord, RestoreReport,
        RestoreRequest,
    };
    use store::InMemoryHubStore;
    use uuid::Uuid;

    async fn store_with_namespace(ns: &str, owner: &str) -> Arc<InMemoryHubStore> {
        let store = Arc::new(InMemoryHubStore::new());
        store
            .ensure_namespace(ns, &owner.to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: ns.to_string(),
                agent_id: owner.to_string(),
                can_read: true,
                can_write: true,
                granted_by: owner.to_string(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn test_backup_job_creates_and_prunes() {
        let store = store_with_namespace("ns", "backup-worker").await;
        let job = BackupJob::new(store.clone(), 1, 0, "backup-worker");

        // 第一轮：应创建 1 份备份
        let r1 = job.run_once(Utc::now()).await.unwrap();
        assert_eq!(r1.created.len(), 1);
        assert_eq!(r1.created[0].1, "ns");
        assert!(r1.pruned.is_empty());

        // 第二轮：再建 1 份，保留策略（keep=1）应删掉旧的
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let r2 = job.run_once(Utc::now()).await.unwrap();
        assert_eq!(r2.created.len(), 1);
        assert_eq!(r2.pruned.len(), 1, "超出保留份数的旧备份应被删除");

        let remaining = mem_core::BackupStore::list_backups(store.as_ref(), None)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn test_backup_job_without_namespaces() {
        let store = Arc::new(InMemoryHubStore::new());
        let job = BackupJob::new(store.clone(), 3, 0, "backup-worker");
        let report = job.run_once(Utc::now()).await.unwrap();
        assert!(report.created.is_empty());
        assert!(report.pruned.is_empty());
    }

    /// 定时备份必须是 v4 全实体载荷：含证据/关系等实体，
    /// 校验和能通过 restore 的完整性验证，且账户侧可按 v4 恢复。
    #[tokio::test]
    async fn test_scheduled_backup_is_v4_payload_and_restorable() {
        let account = BOOTSTRAP_ACCOUNT_ID;
        // 服务层视角的可见名；worker 落库用内部键
        let visible_ns = "ns";
        let internal_ns = scope_name(&account, visible_ns);

        let store = Arc::new(InMemoryHubStore::new());
        // 模拟账户侧已有数据：经内部键直接落库（等价于账户服务路径写入）
        store
            .ensure_namespace(&internal_ns, &"owner-agent".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: internal_ns.clone(),
                agent_id: "owner-agent".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "owner-agent".to_string(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        // 种子：1 条记忆 + 1 条证据 + 1 条关系（v1 载荷只含 entries，会丢后两者）
        let entry = mem_core::MemoryEntry {
            id: Uuid::new_v4(),
            namespace: internal_ns.clone(),
            key: None,
            kind: mem_core::domain::MemoryKind::Fact,
            content: "定时备份必须覆盖认知实体".to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "owner-agent".to_string(),
            confidence: 1.0,
            status: mem_core::domain::MemoryStatus::Active,
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
        store.insert(&entry).await.unwrap();

        // 第二条记忆：关系的合法目标（存储层拒绝自环关系）
        let entry_b = mem_core::MemoryEntry {
            id: Uuid::new_v4(),
            namespace: internal_ns.clone(),
            key: None,
            kind: mem_core::domain::MemoryKind::Fact,
            content: "关系目标条目".to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "owner-agent".to_string(),
            confidence: 1.0,
            status: mem_core::domain::MemoryStatus::Active,
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
        store.insert(&entry_b).await.unwrap();

        let evidence = store
            .append_evidence(&NewEvidence {
                id: None,
                namespace: internal_ns.clone(),
                source_kind: EvidenceSourceKind::Session,
                source_ref: Some("msg-001".to_string()),
                content_hash: "hash-001".to_string(),
                excerpt: Some("用户询问备份策略".to_string()),
                occurred_at: Some(Utc::now()),
                expires_at: None,
            })
            .await
            .unwrap();
        let relation = store
            .add_relation(
                &internal_ns,
                &NewMemoryRelation {
                    from_memory_id: entry.id,
                    to_memory_id: entry_b.id,
                    relation_kind: MemoryRelationKind::DerivedFrom,
                    confidence: 0.9,
                    source_evidence_id: Some(evidence.id),
                },
            )
            .await
            .unwrap();

        // 定时备份作业跑一轮
        let job = BackupJob::new(store.clone(), 5, 0, "backup-worker");
        let report = job.run_once(Utc::now()).await.unwrap();
        assert_eq!(report.created.len(), 1);
        let (backup_id, _) = report.created[0];

        // 1) 载荷版本必须为 v4
        let record = mem_core::BackupStore::get_backup(store.as_ref(), backup_id)
            .await
            .unwrap();
        assert_eq!(
            record.payload["version"],
            serde_json::json!(4),
            "定时快照必须是 v4 载荷"
        );

        // 2) 全实体在载荷中
        let payload_entries: Vec<mem_core::MemoryEntry> =
            serde_json::from_value(record.payload["entries"].clone()).unwrap();
        let payload_evidences: Vec<mem_core::domain::MemoryEvidence> =
            serde_json::from_value(record.payload["evidences"].clone()).unwrap();
        let payload_relations: Vec<mem_core::domain::MemoryRelation> =
            serde_json::from_value(record.payload["relations"].clone()).unwrap();
        assert_eq!(payload_entries.len(), 2);
        assert_eq!(payload_evidences.len(), 1);
        assert_eq!(payload_relations.len(), 1);

        // 3) 清空后经服务层 restore 恢复：校验和验证 + 全部实体回来
        // （先删关系再删条目：条目删除会级联清理关联关系）
        store
            .delete_relation(
                &internal_ns,
                relation.from_memory_id,
                relation.to_memory_id,
                relation.relation_kind,
            )
            .await
            .unwrap();
        store.delete(&internal_ns, entry.id).await.unwrap();
        store.delete(&internal_ns, entry_b.id).await.unwrap();
        store
            .delete_evidence(&internal_ns, evidence.id)
            .await
            .unwrap();

        let svc =
            DefaultBackupService::new(store.clone(), Arc::new(GrantChecker::new(store.clone())));
        let auth = AuthCtx {
            agent_id: "owner-agent".to_string(),
            account_id: account,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Default::default(),
        };
        let restore_report: RestoreReport = svc
            .restore(
                &auth,
                RestoreRequest {
                    backup_id,
                    mode: RestoreMode::Merge,
                },
            )
            .await
            .unwrap();
        assert_eq!(restore_report.created, 2);
        assert_eq!(restore_report.failed_evidences, 0);
        assert_eq!(restore_report.failed_relations, 0);
        assert_eq!(restore_report.failed_tasks, 0);

        let restored_entry = store.get(&internal_ns, entry.id).await.unwrap();
        assert_eq!(restored_entry.content, "定时备份必须覆盖认知实体");
        let restored_evidences = store.all_evidence_in_namespace(&internal_ns).await.unwrap();
        assert_eq!(restored_evidences.len(), 1);
        let restored_relations = store
            .all_relations_in_namespace(&internal_ns)
            .await
            .unwrap();
        assert_eq!(restored_relations.len(), 1);
    }
}
