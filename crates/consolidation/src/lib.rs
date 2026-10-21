//! 独立整合中枢（Consolidation Hub）
//!
//! 负责多智能体并发写入记忆的：
//! - 语义重复检测与聚类
//! - Key 冲突与语义冲突检测
//! - 陈旧记忆评估与健康度度量
//! - 分层内容融合（标签、元数据与补充事实去重拼接）
//! - 自动化策略引擎（Manual / SemiAuto / FullAuto）

pub mod candidates;
pub mod detectors;
pub mod engine;
pub mod merger;
pub mod policy;
pub mod service;
pub mod types;

pub use candidates::hybrid::HybridCandidateFinder;
pub use candidates::rules::RulesCandidateFinder;
pub use candidates::{
    dedup_pairs, CandidateContext, CandidateFinder, CandidateLimits, CandidateOutcome,
    CandidatePair, CandidateRelation, CandidateSource, CandidateThresholds,
};
pub use detectors::{ConflictDetector, DuplicateDetector, StalenessDetector, UnionFind};
pub use engine::ConsolidationEngine;
pub use merger::{ContentMerger, MergeStrategy, MergedOutput, TieredMerger};
pub use policy::{merge_eligibility, AutoPolicy, MergeEligibility, PolicyMode};
pub use service::DefaultIntegrationService;
pub use types::{
    ConflictGroup, ConflictKind, ConsolidationReport, ConsolidationStats, DuplicateGroup,
    StaleEntry,
};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use mem_core::{MemoryEntry, MemoryKind, MemoryStatus, MemoryStore};
    use std::sync::Arc;
    use store::InMemoryHubStore;
    use uuid::Uuid;

    fn make_test_entry(
        ns: &str,
        content: &str,
        key: Option<&str>,
        emb: Option<Vec<f32>>,
    ) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            key: key.map(str::to_string),
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec!["tag1".to_string()],
            metadata: serde_json::json!({"field": "val"}),
            embedding: emb,
            embedding_model: None,
            source_agent: "agent-test".to_string(),
            confidence: 1.0,
            status: MemoryStatus::Active,
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
    async fn test_duplicate_detection_and_consolidation_flow() {
        let store = Arc::new(InMemoryHubStore::new());
        // 默认模式为纯本地规则整合：原文完全相同即为精确重复候选
        let e1 = make_test_entry(
            "ns",
            "PostgreSQL 是开源关系型数据库",
            None,
            Some(vec![1.0, 0.0]),
        );
        let e2 = make_test_entry(
            "ns",
            "PostgreSQL 是开源关系型数据库",
            None,
            Some(vec![0.99, 0.01]),
        );

        store.insert(&e1).await.unwrap();
        store.insert(&e2).await.unwrap();

        let engine = ConsolidationEngine::new(store.clone(), 0.90, 10);
        let report = engine
            .consolidate_namespace("ns", false, None, &"admin-agent".to_string())
            .await
            .unwrap();

        assert_eq!(report.scanned, 2);
        assert_eq!(report.proposals_created, 1);
        assert_eq!(
            report.proposals[0].kind,
            mem_core::IntegrationKind::Duplicate
        );

        let stats = engine.calculate_stats("ns").await.unwrap();
        assert_eq!(stats.duplicate_groups_count, 1);
        assert!(stats.redundancy_ratio > 0.0);
    }

    #[tokio::test]
    async fn test_content_merger_combines_tags_and_unique_content() {
        let e1 = make_test_entry("ns", "第一行事实：北京是中国的首都。", None, None);
        let mut e2 = make_test_entry("ns", "第二行事实：上海是经济中心。", None, None);
        e2.tags = vec!["tag2".to_string()];

        let merger = TieredMerger::with_strategy(MergeStrategy::ConcatenateUnique);
        let res = merger.merge(&e1, &[e1.clone(), e2.clone()]).await.unwrap();

        assert!(res.tags.contains(&"tag1".to_string()));
        assert!(res.tags.contains(&"tag2".to_string()));
        assert!(res.content.contains("北京是中国的首都"));
        assert!(res.content.contains("上海是经济中心"));
        assert_eq!(res.strategy, MergeStrategy::ConcatenateUnique);
    }

    #[tokio::test]
    async fn test_semi_auto_policy_merges_high_similarity() {
        let store = Arc::new(InMemoryHubStore::new());
        let e1 = make_test_entry("ns", "同一事实完全一致", None, Some(vec![1.0, 0.0]));
        let e2 = make_test_entry("ns", "同一事实完全一致", None, Some(vec![1.0, 0.0]));

        store.insert(&e1).await.unwrap();
        store.insert(&e2).await.unwrap();

        let policy = AutoPolicy {
            mode: PolicyMode::SemiAuto,
            auto_merge_threshold: 0.95,
            auto_merge_max_members: 2,
            ..Default::default()
        };

        let engine = ConsolidationEngine::new(store.clone(), 0.90, 10).with_policy(policy);
        let report = engine
            .consolidate_namespace("ns", false, None, &"admin-agent".to_string())
            .await
            .unwrap();

        assert_eq!(report.auto_merged, 1);
        assert_eq!(report.proposals_created, 0);

        // 验证被合并条目状态
        let active = store.all_in_namespace("ns", false).await.unwrap();
        assert_eq!(active.len(), 1);
    }

    /// 默认模式是纯本地规则整合：即便注入了嵌入器，embedding 调用数也必须是 0
    #[tokio::test]
    async fn test_rules_mode_never_calls_embedding() {
        let store = Arc::new(InMemoryHubStore::new());
        let e1 = make_test_entry("ns", "完全相同的事实", None, Some(vec![1.0, 0.0]));
        let e2 = make_test_entry("ns", "完全相同的事实", None, Some(vec![1.0, 0.0]));
        store.insert(&e1).await.unwrap();
        store.insert(&e2).await.unwrap();

        let counter = Arc::new(mem_core::testing::CountingEmbedder::new(Arc::new(
            mem_core::testing::TestEmbedder::new(8),
        )));
        let engine = ConsolidationEngine::new(store, 0.92, 10)
            .with_mode(
                common::ConsolidationMode::Rules,
                common::ConsolidationFallback::Rules,
            )
            .with_embedder(counter.clone());
        let report = engine
            .consolidate_namespace("ns", false, None, &"admin-agent".to_string())
            .await
            .unwrap();

        assert_eq!(counter.call_count(), 0, "纯规则模式不得调用嵌入服务");
        assert_eq!(report.mode.effective_mode, "rules");
        assert!(!report.mode.embedding_used);
        assert_eq!(report.proposals_created, 1);
    }

    /// 语义近重复即使相似度极高也不得自动合并（关系门槛优先于分数门槛）
    #[tokio::test]
    async fn test_semantic_candidate_is_not_auto_merged() {
        let store = Arc::new(InMemoryHubStore::new());
        // 两条内容不同但向量几乎相同：若按旧逻辑（只看相似度）会被自动合并
        let e1 = make_test_entry("ns", "部署在上海可用区", None, Some(vec![1.0, 0.0]));
        let e2 = make_test_entry("ns", "部署在北京可用区", None, Some(vec![1.0, 0.0]));
        store.insert(&e1).await.unwrap();
        store.insert(&e2).await.unwrap();

        let policy = AutoPolicy {
            mode: PolicyMode::SemiAuto,
            auto_merge_threshold: 0.90,
            auto_merge_max_members: 3,
            ..Default::default()
        };
        let engine = ConsolidationEngine::new(store.clone(), 0.90, 10).with_policy(policy);
        let report = engine
            .consolidate_namespace("ns", false, None, &"admin-agent".to_string())
            .await
            .unwrap();

        assert_eq!(report.auto_merged, 0, "语义近重复不得自动合并");
        let active = store.all_in_namespace("ns", false).await.unwrap();
        assert_eq!(active.len(), 2, "两条记忆都应保持活跃");
    }
}
