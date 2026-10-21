//! 定时备份作业：命名空间快照 + 保留策略
//!
//! 作业以系统特权主体运行（不经过 AuthCtx），直接操作存储；
//! 与 MCP 暴露的 `memory_backup_create` 走同一套快照格式与校验和算法。

use crate::lease::JobLease;
use chrono::{DateTime, Utc};
use common::{AgentId, Result};
use mem_core::{BackupRecord, MemoryHubStore};
use sha2::{Digest, Sha256};
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
            let entries = self.store.all_in_namespace(&info.namespace, true).await?;
            let checksum = checksum_of(&entries);
            let record = BackupRecord {
                id: uuid::Uuid::new_v4(),
                namespace: info.namespace.clone(),
                created_by: self.worker_agent.clone(),
                note: Some("scheduled-backup".to_string()),
                entry_count: entries.len(),
                checksum,
                payload: serde_json::json!({
                    "version": 1,
                    "namespace": info.namespace,
                    "entries": entries,
                }),
                retain_until: None,
                created_at: now,
            };
            self.store.save_backup(&record).await?;
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

/// 快照校验和（与 `DefaultBackupService` 同一算法：规范化 JSON 的 SHA-256）
fn checksum_of(entries: &[mem_core::MemoryEntry]) -> String {
    let bytes = serde_json::to_vec(entries).unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::{GrantStore, MemoryStore, NamespaceGrantRecord};
    use store::InMemoryHubStore;

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

    #[tokio::test]
    async fn test_checksum_is_deterministic() {
        // 同内容快照应有同一校验和
        let store = store_with_namespace("ns", "w").await;
        let entries = store.all_in_namespace("ns", true).await.unwrap();
        assert_eq!(checksum_of(&entries), checksum_of(&entries));
        assert!(!checksum_of(&entries).is_empty());
    }
}
