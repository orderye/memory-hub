//! 整合服务门面：负责权限检查、命名空间作用域包装，内部统一委托 ConsolidationEngine 执行

use crate::engine::ConsolidationEngine;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AuthCtx, HubError, Result, Scope};
use mem_core::{
    account_scope::AccountScopedStore,
    AccessChecker, Clock, Embedder, IntegrationProposal, IntegrationService, IntegrationStore,
    MemoryEntry, MemoryHubStore, ResolveRequest, ScanReport, ScanRequest,
    SystemClock,
};
use std::sync::Arc;

/// 整合服务实现：统一委托 ConsolidationEngine 的安全门面
pub struct DefaultIntegrationService {
    engine: Arc<ConsolidationEngine>,
    store: Arc<dyn MemoryHubStore>,
    access: Arc<dyn AccessChecker>,
    #[allow(dead_code)]
    clock: Arc<dyn Clock>,
    sim_threshold: f32,
    #[allow(dead_code)]
    max_proposals: usize,
    embedder: Option<Arc<dyn Embedder>>,
}

impl DefaultIntegrationService {
    pub fn new(
        store: Arc<dyn MemoryHubStore>,
        access: Arc<dyn AccessChecker>,
        sim_threshold: f32,
        max_proposals: usize,
    ) -> Self {
        let sim = sim_threshold.clamp(0.0, 1.0);
        let max_p = max_proposals.max(1);
        let engine = Arc::new(ConsolidationEngine::new(store.clone(), sim, max_p));
        Self {
            engine,
            store,
            access,
            clock: Arc::new(SystemClock),
            sim_threshold: sim,
            max_proposals: max_p,
            embedder: None,
        }
    }

    pub fn with_engine(mut self, engine: Arc<ConsolidationEngine>) -> Self {
        self.engine = engine;
        self
    }

    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder.clone());
        self.engine = Arc::new(self.engine.as_ref().clone().with_embedder(embedder));
        self
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    async fn require_read(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        if !self.access.can_read(auth, namespace).await? {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }

    async fn require_write(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
        if !self.access.can_write(auth, namespace).await? {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }
}

#[async_trait]
impl IntegrationService for DefaultIntegrationService {
    async fn scan(&self, auth: &AuthCtx, req: ScanRequest) -> Result<ScanReport> {
        self.require_write(auth, &req.namespace).await?;
        let threshold = req.sim_threshold.unwrap_or(self.sim_threshold);
        if !(0.0..=1.0).contains(&threshold) {
            return Err(HubError::InvalidArgument(
                "sim_threshold 必须落在 [0,1]".to_string(),
            ));
        }

        let scoped_store = Arc::new(AccountScopedStore::new(self.store.clone(), auth.account_id));
        let scoped_engine = self.engine.with_store(scoped_store);

        let report = scoped_engine
            .consolidate_namespace(
                &req.namespace,
                req.dry_run,
                req.sim_threshold,
                &auth.agent_id,
            )
            .await?;

        Ok(ScanReport {
            namespace: report.namespace,
            scanned: report.scanned,
            dry_run: report.dry_run,
            proposals: report.proposals,
            skipped_existing: report.skipped_existing,
        })
    }

    async fn list(&self, auth: &AuthCtx, namespace: &str) -> Result<Vec<IntegrationProposal>> {
        self.require_read(auth, namespace).await?;
        let scoped = AccountScopedStore::new(self.store.clone(), auth.account_id);
        scoped.list_proposals(namespace, None).await
    }

    async fn resolve(&self, auth: &AuthCtx, req: ResolveRequest) -> Result<MemoryEntry> {
        let scoped_store = Arc::new(AccountScopedStore::new(self.store.clone(), auth.account_id));
        let proposal = scoped_store.get_proposal(req.proposal_id).await?;
        self.require_write(auth, &proposal.namespace).await?;

        let scoped_engine = self.engine.with_store(scoped_store);
        scoped_engine.resolve_proposal(req, &auth.agent_id).await
    }

    async fn expire_stale(&self, now: DateTime<Utc>) -> Result<usize> {
        self.store.archive_expired(now).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{AgentId, Scope};
    use mem_core::{
        access::GrantChecker,
        account_scope::scope_name,
        algorithm::l2_normalize,
        domain::{
            IntegrationKind, MemoryKind, MemoryStatus, NamespaceGrantRecord, ProposalStatus,
        },
        infra::{GrantStore, MemoryStore},
        ResolveAction,
    };
    use store::InMemoryHubStore;
    use uuid::Uuid;

    fn scoped_ns(ns: &str) -> String {
        scope_name(&common::BOOTSTRAP_ACCOUNT_ID, ns)
    }

    fn make_entry(ns: &str, content: &str, key: Option<&str>) -> MemoryEntry {
        make_entry_with_embedding(ns, content, key, None)
    }

    fn make_entry_with_embedding(
        ns: &str,
        content: &str,
        key: Option<&str>,
        embedding: Option<Vec<f32>>,
    ) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            key: key.map(str::to_string),
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding,
            embedding_model: None,
            source_agent: AgentId::from("agent-a"),
            confidence: 1.0,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: None,
            merged_into: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn writer() -> AuthCtx {
        AuthCtx {
            agent_id: AgentId::from("agent-a"),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write],
            acl: Default::default(),
        }
    }

    async fn setup() -> (DefaultIntegrationService, Arc<InMemoryHubStore>) {
        let store = Arc::new(InMemoryHubStore::default());
        store
            .ensure_namespace(&scoped_ns("ns"), &AgentId::from("agent-a"))
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: AgentId::from("agent-a"),
                can_read: true,
                can_write: true,
                granted_by: AgentId::from("agent-a"),
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        let svc = DefaultIntegrationService::new(
            store.clone(),
            Arc::new(GrantChecker::new(store.clone())),
            0.99,
            100,
        );
        (svc, store)
    }

    fn near_identical_vec(seed: &str, jitter: f32) -> Vec<f32> {
        let mut v: Vec<f32> = (0..8)
            .map(|i| ((seed.len() as f32 + i as f32) * 0.37).sin())
            .collect();
        v[0] += jitter;
        l2_normalize(&mut v);
        v
    }

    #[tokio::test]
    async fn test_scan_detects_duplicates_and_dry_run_does_not_persist() {
        let (svc, store) = setup().await;
        store
            .insert(&make_entry_with_embedding(
                &scoped_ns("ns"),
                "服务器部署在东京",
                None,
                Some(near_identical_vec("x", 0.0)),
            ))
            .await
            .unwrap();
        store
            .insert(&make_entry_with_embedding(
                &scoped_ns("ns"),
                "服务器部署在东京区域",
                None,
                Some(near_identical_vec("x", 0.001)),
            ))
            .await
            .unwrap();

        let report = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: true,
                    sim_threshold: Some(0.99),
                },
            )
            .await
            .unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(report.proposals[0].kind, IntegrationKind::Duplicate);

        let persisted = svc.list(&writer(), "ns").await.unwrap();
        assert!(persisted.is_empty());
    }

    #[tokio::test]
    async fn test_scan_detects_key_conflicts() {
        let (svc, store) = setup().await;
        store
            .insert(&make_entry(
                &scoped_ns("ns"),
                "数据库用 MySQL",
                Some("db:engine"),
            ))
            .await
            .unwrap();
        store
            .insert(&make_entry(
                &scoped_ns("ns"),
                "数据库用 PostgreSQL",
                Some("db:engine"),
            ))
            .await
            .unwrap();

        let report = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(report.proposals[0].kind, IntegrationKind::Conflict);
        assert_eq!(report.proposals[0].member_ids.len(), 2);

        let report2 = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(report2.proposals.len(), 0);
        assert_eq!(report2.skipped_existing, 1);
    }

    #[tokio::test]
    async fn test_resolve_merge_archives_members_and_keeps_lineage() {
        let (svc, store) = setup().await;
        let e1 = store
            .insert(&make_entry(
                &scoped_ns("ns"),
                "数据库用 MySQL",
                Some("db:engine"),
            ))
            .await
            .unwrap();
        let e2 = store
            .insert(&make_entry(
                &scoped_ns("ns"),
                "数据库用 PostgreSQL",
                Some("db:engine"),
            ))
            .await
            .unwrap();

        let scan = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();
        let proposal_id = scan.proposals[0].id;

        let survivor = svc
            .resolve(
                &writer(),
                ResolveRequest {
                    proposal_id,
                    action: ResolveAction::Merge,
                    survivor_id: Some(e2.id),
                    note: Some("以新内容为准".to_string()),
                },
            )
            .await
            .unwrap();
        assert_eq!(survivor.id, e2.id);

        let archived = store.get(&scoped_ns("ns"), e1.id).await.unwrap();
        assert_eq!(archived.status, MemoryStatus::Merged);
        assert_eq!(archived.merged_into, Some(e2.id));

        let proposals = svc.list(&writer(), "ns").await.unwrap();
        assert_eq!(proposals[0].status, ProposalStatus::Merged);
        assert_eq!(
            proposals[0].resolution_note.as_deref(),
            Some("以新内容为准")
        );

        let err = svc
            .resolve(
                &writer(),
                ResolveRequest {
                    proposal_id,
                    action: ResolveAction::Merge,
                    survivor_id: Some(e2.id),
                    note: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::Conflict));
    }

    #[tokio::test]
    async fn test_resolve_dismiss_keeps_members() {
        let (svc, store) = setup().await;
        let e1 = store
            .insert(&make_entry(
                &scoped_ns("ns"),
                "数据库用 MySQL",
                Some("db:engine"),
            ))
            .await
            .unwrap();
        store
            .insert(&make_entry(
                &scoped_ns("ns"),
                "数据库用 PostgreSQL",
                Some("db:engine"),
            ))
            .await
            .unwrap();

        let scan = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();
        let proposal_id = scan.proposals[0].id;

        svc.resolve(
            &writer(),
            ResolveRequest {
                proposal_id,
                action: ResolveAction::Dismiss,
                survivor_id: None,
                note: Some("保留分立".to_string()),
            },
        )
        .await
        .unwrap();

        let e1_after = store.get(&scoped_ns("ns"), e1.id).await.unwrap();
        assert_eq!(e1_after.status, MemoryStatus::Active);
        let proposals = svc.list(&writer(), "ns").await.unwrap();
        assert_eq!(proposals[0].status, ProposalStatus::Dismissed);
    }

    #[tokio::test]
    async fn test_resolve_rejects_non_member_survivor() {
        let (svc, store) = setup().await;
        store
            .insert(&make_entry(&scoped_ns("ns"), "A", Some("k:1")))
            .await
            .unwrap();
        store
            .insert(&make_entry(&scoped_ns("ns"), "B", Some("k:1")))
            .await
            .unwrap();
        let outsider = store
            .insert(&make_entry(&scoped_ns("ns"), "C", Some("k:2")))
            .await
            .unwrap();

        let scan = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();

        let err = svc
            .resolve(
                &writer(),
                ResolveRequest {
                    proposal_id: scan.proposals[0].id,
                    action: ResolveAction::Merge,
                    survivor_id: Some(outsider.id),
                    note: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_merge_unions_tags() {
        let (svc, store) = setup().await;
        let mut e1 = make_entry(&scoped_ns("ns"), "旧内容", Some("k:1"));
        e1.tags = vec!["db".to_string()];
        let e1 = store.insert(&e1).await.unwrap();
        let mut e2 = make_entry(&scoped_ns("ns"), "新内容", Some("k:1"));
        e2.tags = vec!["pg".to_string()];
        let e2 = store.insert(&e2).await.unwrap();

        let scan = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();

        let survivor = svc
            .resolve(
                &writer(),
                ResolveRequest {
                    proposal_id: scan.proposals[0].id,
                    action: ResolveAction::Merge,
                    survivor_id: Some(e2.id),
                    note: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(survivor.tags, vec!["pg".to_string(), "db".to_string()]);
        let _ = e1;
    }

    #[tokio::test]
    async fn test_expire_stale_archives_expired() {
        let (svc, store) = setup().await;
        let mut e = make_entry(&scoped_ns("ns"), "临时状态", None);
        e.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
        let id = store.insert(&e).await.unwrap().id;

        let count = svc.expire_stale(Utc::now()).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            store.get(&scoped_ns("ns"), id).await.unwrap().status,
            MemoryStatus::Archived
        );
    }

    #[tokio::test]
    async fn test_scan_requires_write_scope() {
        let (svc, _store) = setup().await;
        let reader = AuthCtx {
            agent_id: AgentId::from("agent-a"),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read],
            acl: Default::default(),
        };
        let err = svc
            .scan(
                &reader,
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));
    }
}
