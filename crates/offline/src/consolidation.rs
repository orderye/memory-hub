//! 每日记忆巩固作业（设计规范 10 与 18.2 WP-17 节）

use chrono::{DateTime, Utc};
use common::Result;
use hub_core::{DecayReport, MemoryStore};
use std::sync::Arc;

/// 每日记忆巩固作业处理器
pub struct DailyConsolidationJob {
    store: Arc<dyn MemoryStore>,
}

impl DailyConsolidationJob {
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self { store }
    }

    /// 执行单次巩固周期
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<DecayReport> {
        tracing::info!(execution_time = %now, "Starting daily memory consolidation job...");
        let report = self.store.apply_decay(now).await?;

        tracing::info!(
            decayed = report.decayed_count,
            retired = report.retired_count,
            purged = report.purged_count,
            "Daily memory consolidation job completed successfully"
        );

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use hub_core::{Layer, NewMemory, Provenance, Sensitivity, UsePolicy};
    use hub_store::InMemoryMemoryStore;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_consolidation_job_execution() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let job = DailyConsolidationJob::new(store.clone());

        let user_id = Uuid::new_v4();

        // 插入一条 20 天前的 State 记忆（State τ = 7天，20天后必衰减至 < 0.2 并退役）
        store
            .upsert(NewMemory {
                user_id,
                layer: Layer::State,
                text: "近期感觉有些疲惫".to_string(),
                provenance: Provenance::Inferred,
                confidence: 0.8,
                sensitivity: Sensitivity::Normal,
                use_policy: UsePolicy::Free,
                source_client: "c1".to_string(),
                ttl_seconds: None,
                attrs: serde_json::json!({}),
            })
            .await
            .unwrap();

        let future = Utc::now() + Duration::days(21);
        let report = job.run_once(future).await.unwrap();

        assert_eq!(report.decayed_count, 1);
        assert_eq!(report.retired_count, 1);
    }
}
