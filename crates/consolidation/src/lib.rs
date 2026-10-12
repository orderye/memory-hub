//! 独立整合中枢（Consolidation Hub）
//!
//! 负责多智能体并发写入记忆的：
//! - 语义重复检测与聚类
//! - Key 冲突与语义冲突检测
//! - 陈旧记忆评估与健康度度量
//! - 分层内容融合（标签、元数据与补充事实去重拼接）
//! - 自动化策略引擎（Manual / SemiAuto / FullAuto）

pub mod detectors;
pub mod engine;
pub mod merger;
pub mod policy;
pub mod types;

pub use detectors::{ConflictDetector, DuplicateDetector, StalenessDetector, UnionFind};
pub use engine::ConsolidationEngine;
pub use merger::{ContentMerger, MergeStrategy, MergedOutput, TieredMerger};
pub use policy::{AutoPolicy, PolicyMode};
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
        }
    }

    #[tokio::test]
    async fn test_duplicate_detection_and_consolidation_flow() {
        let store = Arc::new(InMemoryHubStore::new());
        let e1 = make_test_entry(
            "ns",
            "PostgreSQL 是开源关系型数据库",
            None,
            Some(vec![1.0, 0.0]),
        );
        let e2 = make_test_entry(
            "ns",
            "PostgreSQL 是开源关系型数据库系统",
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
}
