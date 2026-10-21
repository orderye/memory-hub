//! 周期性整合扫描作业：跨 Agent 写入的重复/冲突识别
//!
//! 只生成提案，绝不自动合并——合并必须由 Agent 显式裁决
//! （参阅 `DefaultIntegrationService::resolve`）。

use crate::lease::JobLease;
use chrono::{DateTime, Utc};
use common::{AgentId, Result};
use consolidation::ConsolidationEngine;
use mem_core::MemoryHubStore;
use std::sync::Arc;
use tracing::{info, warn};

/// 整合扫描作业
pub struct IntegrationScanJob {
    engine: ConsolidationEngine,
    store: Arc<dyn MemoryHubStore>,
    sim_threshold: f32,
    #[allow(dead_code)]
    max_proposals: usize,
    /// 作业执行者标识
    worker_agent: AgentId,
    /// 跨进程租约（None = 单机/手动执行，不做互斥）
    lease: Option<JobLease>,
}

/// 单轮扫描报告
#[derive(Debug, Clone, Default)]
pub struct IntegrationScanReport {
    /// 各命名空间实际扫描的条目数
    pub scanned: std::collections::HashMap<String, usize>,
    /// 各命名空间本轮新建的提案数（0 也记录：区分"没扫到"与"扫了没新提案"）
    pub proposals: std::collections::HashMap<String, usize>,
    /// 因租约被其它副本占用而跳过的命名空间
    pub skipped_leased: Vec<String>,
    /// 因单次扫描条目上限而被截断的命名空间（扫描结果只覆盖最近的一部分条目）
    pub truncated_namespaces: Vec<String>,
    /// 扫描失败的命名空间（单个命名空间出错不得中断其余命名空间）
    pub failed_namespaces: Vec<String>,
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
            lease: None,
        }
    }

    /// 注入跨进程租约：同一命名空间在一个租约窗口内只会被一个副本扫描
    pub fn with_lease(mut self, lease: JobLease) -> Self {
        self.lease = Some(lease);
        self
    }

    /// 设置单次扫描的条目上限（0 = 不限制）
    pub fn with_scan_limit(mut self, max_entries: usize) -> Self {
        self.engine = self.engine.with_scan_limit(max_entries);
        self
    }

    /// 扫描全部命名空间（dry_run=false：生成落库提案）
    pub async fn run_once(&self, _now: DateTime<Utc>) -> Result<IntegrationScanReport> {
        info!(worker = %self.worker_agent, "开始执行周期性整合扫描");

        let mut report = IntegrationScanReport::default();
        let namespaces = mem_core::MemoryStore::list_namespaces(self.store.as_ref()).await?;

        for ns in namespaces {
            // 跨进程互斥：同一命名空间在租约窗口内只由一个副本扫描
            if let Some(lease) = &self.lease {
                if !lease
                    .try_acquire(&JobLease::namespace_key("integration-scan", &ns))
                    .await?
                {
                    report.skipped_leased.push(ns.clone());
                    info!(namespace = %ns, "整合扫描租约被其它副本占用，本轮跳过");
                    continue;
                }
            }

            // 候选空间只由引擎装载一次（此前这里先 all_in_namespace 数一遍活跃条目，
            // 再让引擎重新装载一次，等于每个命名空间读两遍全量数据）。
            //
            // 单个命名空间失败**不得中断整轮扫描**：否则一个坏命名空间会让其余
            // 命名空间整个窗口都得不到整合（而且它的租约已经被消耗掉了）。
            let scan = match self.scan_namespace(&ns).await {
                Ok(scan) => scan,
                Err(e) => {
                    warn!(namespace = %ns, error = %e, "整合扫描失败，跳过该命名空间（不影响其余）");
                    report.failed_namespaces.push(ns.clone());
                    continue;
                }
            };
            report.scanned.insert(ns.clone(), scan.scanned);
            if scan.truncated {
                report.truncated_namespaces.push(ns.clone());
                warn!(
                    namespace = %ns,
                    scanned = scan.scanned,
                    "整合扫描达到条目上限，本轮只覆盖最近更新的条目"
                );
            }
            // 始终记录（含 0）：报告语义是"各命名空间本轮新建的提案数"，
            // 缺省与 0 不是一回事——调用方要能区分"没扫到"和"扫了但没有新提案"。
            report.proposals.insert(ns.clone(), scan.proposals_created);
            if scan.proposals_created > 0 {
                info!(namespace = %ns, created = scan.proposals_created, "整合扫描发现新提案");
            }
        }

        Ok(report)
    }

    /// 设置候选发现模式、降级策略与规模限制
    pub fn with_mode(
        mut self,
        mode: common::ConsolidationMode,
        fallback: common::ConsolidationFallback,
        limits: consolidation::CandidateLimits,
        lexical_min_similarity: f32,
    ) -> Self {
        self.engine = self
            .engine
            .with_mode(mode, fallback)
            .with_candidate_limits(limits)
            .with_lexical_min_similarity(lexical_min_similarity);
        self
    }

    /// 注入嵌入器：向量模式（`hybrid`）召回语义候选时使用
    ///
    /// 纯规则模式不会注入嵌入器——整合链路因此一次嵌入调用都不会发出。
    pub fn with_embedder(mut self, embedder: Arc<dyn mem_core::Embedder>) -> Self {
        self.engine = self.engine.with_embedder(embedder);
        self
    }

    /// 单命名空间扫描（委托独立整合中枢引擎统一执行）
    async fn scan_namespace(&self, namespace: &str) -> Result<consolidation::ConsolidationReport> {
        self.engine
            .consolidate_namespace(
                namespace,
                false,
                Some(self.sim_threshold),
                &self.worker_agent,
            )
            .await
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

        // 默认是纯本地规则整合：这两条没有结构化佐证（无标签、无业务键），
        // 词法相近但不足以断言同一条事实，因此**不产生**重复提案。
        let rules_job = IntegrationScanJob::new(store.clone(), 0.90, 100, "scan-worker");
        let rules_report = rules_job.run_once(Utc::now()).await.unwrap();
        assert_eq!(rules_report.scanned.get("ns2"), Some(&2));
        assert_eq!(
            rules_report.proposals.get("ns2"),
            Some(&0),
            "纯规则模式不应凭词法相近就判定为重复"
        );

        // 向量模式：已存向量可用 → 召回语义近重复候选 → 生成提案
        let hybrid_job = IntegrationScanJob::new(store.clone(), 0.90, 100, "scan-worker")
            .with_mode(
                common::ConsolidationMode::Hybrid,
                common::ConsolidationFallback::Rules,
                consolidation::CandidateLimits::default(),
                0.80,
            )
            .with_embedder(Arc::new(mem_core::testing::SemanticTestEmbedder::new(64)));
        let report = hybrid_job.run_once(Utc::now()).await.unwrap();
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
        // 报告现在始终记录"本轮新建提案数"（含 0）：缺省与 0 不是一回事，
        // 调用方要能区分"没扫到"与"扫了但没有新提案"。
        assert_eq!(report.proposals.get("ns"), Some(&0));
    }

    /// 有界扫描：超过条目上限时只扫最近更新的 N 条，并如实标 truncated
    #[tokio::test]
    async fn test_scan_is_bounded_and_reports_truncation() {
        let store = Arc::new(InMemoryHubStore::new());
        store
            .ensure_namespace("big", &"agent-a".to_string())
            .await
            .unwrap();
        for i in 0..5 {
            store
                .insert(&entry("big", Some(&format!("k{i}")), &format!("内容 {i}")))
                .await
                .unwrap();
        }

        let job =
            IntegrationScanJob::new(store.clone(), 0.99, 100, "scan-worker").with_scan_limit(2);
        let report = job.run_once(Utc::now()).await.unwrap();
        assert_eq!(
            report.scanned.get("big"),
            Some(&2),
            "扫描条目数必须被上限截断到 2"
        );
        assert!(
            report.truncated_namespaces.contains(&"big".to_string()),
            "截断必须显式上报，不得静默"
        );
    }

    /// 跨进程租约：同一窗口内第二个副本必须跳过该命名空间
    #[tokio::test]
    async fn test_scan_skips_namespace_when_lease_held_by_peer() {
        let store = Arc::new(InMemoryHubStore::new());
        store
            .ensure_namespace("ns", &"agent-a".to_string())
            .await
            .unwrap();
        store
            .insert(&entry("ns", Some("db:engine"), "MySQL"))
            .await
            .unwrap();
        store
            .insert(&entry("ns", Some("db:engine"), "PostgreSQL"))
            .await
            .unwrap();

        let rate: std::sync::Arc<dyn common::RateLimitStore> =
            std::sync::Arc::new(common::InMemoryRateLimitStore::new());
        let lease = JobLease::from_hours(rate, 24).unwrap();

        let peer_a =
            IntegrationScanJob::new(store.clone(), 0.99, 100, "worker-a").with_lease(lease.clone());
        let peer_b =
            IntegrationScanJob::new(store.clone(), 0.99, 100, "worker-b").with_lease(lease);

        let first = peer_a.run_once(Utc::now()).await.unwrap();
        assert!(first.scanned.contains_key("ns"));
        assert!(first.skipped_leased.is_empty());

        let second = peer_b.run_once(Utc::now()).await.unwrap();
        assert!(
            second.skipped_leased.contains(&"ns".to_string()),
            "租约被占用时必须跳过该命名空间"
        );
        assert!(second.scanned.is_empty());
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
