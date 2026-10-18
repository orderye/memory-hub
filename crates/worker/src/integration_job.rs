//! 周期性整合扫描作业：跨 Agent 写入的重复/冲突识别
//!
//! 只生成提案，绝不自动合并——合并必须由 Agent 显式裁决
//! （参阅 `DefaultIntegrationService::resolve`）。

use chrono::{DateTime, Utc};
use common::{AgentId, Result};
use consolidation::ConsolidationEngine;
use mem_core::MemoryHubStore;
use std::sync::Arc;
use tracing::info;

/// 整合扫描作业
pub struct IntegrationScanJob {
    engine: ConsolidationEngine,
    store: Arc<dyn MemoryHubStore>,
    sim_threshold: f32,
    #[allow(dead_code)]
    max_proposals: usize,
    /// 作业执行者标识
    worker_agent: AgentId,
}

/// 单轮扫描报告
#[derive(Debug, Clone, Default)]
pub struct IntegrationScanReport {
    /// 各命名空间扫描的条目数
    pub scanned: std::collections::HashMap<String, usize>,
    /// 各命名空间新建的提案数
    pub proposals: std::collections::HashMap<String, usize>,
}

impl IntegrationScanJob {
    pub fn new(
        store: Arc<dyn MemoryHubStore>,
        sim_threshold: f32,
        max_proposals: usize,
        worker_agent: impl Into<String>,
    ) -> Self {
        let agent = worker_agent.into();
        let engine = ConsolidationEngine::new(store.clone(), sim_threshold, max_proposals);
        Self {
            engine,
            store,
            sim_threshold: sim_threshold.clamp(0.0, 1.0),
            max_proposals: max_proposals.max(1),
            worker_agent: agent,
        }
    }

    /// 扫描全部命名空间（dry_run=false：生成落库提案）
    pub async fn run_once(&self, _now: DateTime<Utc>) -> Result<IntegrationScanReport> {
        info!(worker = %self.worker_agent, "开始执行周期性整合扫描");

        let mut report = IntegrationScanReport::default();
        let namespaces = mem_core::MemoryStore::list_namespaces(self.store.as_ref()).await?;

        for ns in namespaces {
            let entries = self.store.all_in_namespace(&ns, false).await?;
            let active = entries
                .iter()
                .filter(|e| e.status == mem_core::MemoryStatus::Active)
                .count();
            report.scanned.insert(ns.clone(), active);

            if active < 2 {
                continue;
            }

            // 复用整合服务的扫描逻辑（内部按成员集合去重，幂等）
            let created = self.scan_namespace(&ns).await?;
            report.proposals.insert(ns.clone(), created);
            if created > 0 {
                info!(namespace = %ns, created, "整合扫描发现新提案");
            }
        }

        Ok(report)
    }

    /// 单命名空间扫描（委托独立整合中枢引擎统一执行）
    async fn scan_namespace(&self, namespace: &str) -> Result<usize> {
        let report = self
            .engine
            .consolidate_namespace(
                namespace,
                false,
                Some(self.sim_threshold),
                &self.worker_agent,
            )
            .await?;
        Ok(report.proposals_created)
    }

    /// 归档过期条目（与备份作业共用的清理步骤）
    pub async fn expire_stale(&self, now: DateTime<Utc>) -> Result<usize> {
        let archived = mem_core::MemoryStore::archive_expired(self.store.as_ref(), now).await?;
        if archived > 0 {
            info!(archived, "已归档过期记忆");
        }
        Ok(archived)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::{
        GrantStore, MemoryEntry, MemoryKind, MemoryStore, MemoryVersion, NamespaceGrantRecord,
        NamespaceInfo, VersionChange,
    };
    use store::InMemoryHubStore;
    use uuid::Uuid;

    fn entry(ns: &str, key: Option<&str>, content: &str) -> MemoryEntry {
        MemoryEntry {
            id: uuid::Uuid::new_v4(),
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
            status: mem_core::MemoryStatus::Active,
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
    async fn test_scan_creates_conflict_proposals_idempotently() {
        let store = Arc::new(InMemoryHubStore::new());
        store
            .ensure_namespace("ns", &"agent-a".to_string())
            .await
            .unwrap();
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

        // 同 key 两个不同内容
        store
            .insert(&entry("ns", Some("db:engine"), "MySQL"))
            .await
            .unwrap();
        store
            .insert(&entry("ns", Some("db:engine"), "PostgreSQL"))
            .await
            .unwrap();

        let job = IntegrationScanJob::new(store.clone(), 0.99, 100, "scan-worker");
        let report = job.run_once(Utc::now()).await.unwrap();
        assert_eq!(report.scanned.get("ns"), Some(&2));
        assert_eq!(report.proposals.get("ns"), Some(&1));

        // 幂等：再扫一次不得重复建提案
        let report2 = job.run_once(Utc::now()).await.unwrap();
        assert_eq!(report2.proposals.get("ns"), Some(&0));
    }

    #[tokio::test]
    async fn test_scan_creates_duplicate_proposals_for_embeddings() {
        let store = Arc::new(InMemoryHubStore::new());
        store
            .ensure_namespace("ns2", &"agent-a".to_string())
            .await
            .unwrap();

        let mut e1 = entry("ns2", None, "服务部署在华东区域");
        e1.embedding = Some(vec![1.0, 0.0]);
        let mut e2 = entry("ns2", None, "服务部署在华东服务器群");
        e2.embedding = Some(vec![0.99, 0.01]);

        store.insert(&e1).await.unwrap();
        store.insert(&e2).await.unwrap();

        let job = IntegrationScanJob::new(store.clone(), 0.90, 100, "scan-worker");
        let report = job.run_once(Utc::now()).await.unwrap();
        assert_eq!(report.scanned.get("ns2"), Some(&2));
        assert_eq!(report.proposals.get("ns2"), Some(&1));
    }

    #[tokio::test]
    async fn test_scan_skips_single_entry_namespaces() {
        let store = Arc::new(InMemoryHubStore::new());
        store
            .insert(&entry("ns", Some("k"), "only one"))
            .await
            .unwrap();
        let job = IntegrationScanJob::new(store.clone(), 0.99, 100, "scan-worker");
        let report = job.run_once(Utc::now()).await.unwrap();
        assert!(report.proposals.is_empty());
    }

    #[tokio::test]
    async fn test_expire_stale_archives() {
        let store = Arc::new(InMemoryHubStore::new());
        let mut e = entry("ns", None, "temporary");
        e.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        let id = store.insert(&e).await.unwrap().id;

        let job = IntegrationScanJob::new(store.clone(), 0.99, 100, "scan-worker");
        let count = job.expire_stale(Utc::now()).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            store.get("ns", id).await.unwrap().status,
            mem_core::MemoryStatus::Archived
        );
    }

    // 编译期检查：合约组合
    #[test]
    fn test_contracts() {
        fn assert_impl<T: mem_core::MemoryHubStore>() {}
        assert_impl::<InMemoryHubStore>();
        let _: Option<NamespaceInfo> = None;
        let _: Option<MemoryVersion> = None;
        let _ = VersionChange::Created;
        let _: fn() -> Option<Uuid> = || None;
    }
}
