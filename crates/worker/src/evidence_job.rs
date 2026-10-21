//! 证据保留期限清理作业
//!
//! 证据行是 append-only 的不可变记录：写入后不能悄悄改写。
//! 这意味着敏感内容必须在**写入前**脱敏（见 `mem_core::redaction`），
//! 但保留期限仍需要第二道防线——到期即物理删除，不让不可变副本无限期留存。
//!
//! 作业以系统特权主体运行（不经过 AuthCtx），直接操作存储；
//! 它只删除 `expires_at` 已到且非空的证据行，`expires_at` 为空表示永久保留。

use crate::lease::JobLease;
use chrono::{DateTime, Utc};
use common::Result;
use mem_core::MemoryHubStore;
use std::sync::Arc;
use tracing::{info, warn};

/// 证据清理作业
pub struct EvidenceRetentionJob {
    store: Arc<dyn MemoryHubStore>,
    /// 作业执行者标识
    worker_agent: String,
    /// 跨进程租约（None = 不做互斥）
    lease: Option<JobLease>,
}

impl EvidenceRetentionJob {
    pub fn new(store: Arc<dyn MemoryHubStore>, worker_agent: impl Into<String>) -> Self {
        Self {
            store,
            worker_agent: worker_agent.into(),
            lease: None,
        }
    }

    /// 注入跨进程租约：同一租约窗口内只有一个副本执行证据清理
    pub fn with_lease(mut self, lease: JobLease) -> Self {
        self.lease = Some(lease);
        self
    }

    /// 清理已过保留期限的证据，返回删除条数
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<usize> {
        if let Some(lease) = &self.lease {
            if !lease
                .try_acquire(&JobLease::global_key("evidence-purge"))
                .await?
            {
                warn!(worker = %self.worker_agent, "证据清理租约被其它副本占用，本轮跳过");
                return Ok(0);
            }
        }
        let namespaces = mem_core::GrantStore::list_namespace_infos(self.store.as_ref()).await?;
        let mut purged = 0usize;
        for info in namespaces {
            purged += self
                .store
                .purge_expired_evidence(&info.namespace, now)
                .await?;
        }
        if purged > 0 {
            info!(
                worker = %self.worker_agent,
                purged,
                "已清理超过保留期限的证据"
            );
        }
        Ok(purged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::testing::TestHubStore;
    use mem_core::{EvidenceSourceKind, EvidenceStore, NewEvidence};

    #[tokio::test]
    async fn test_purges_only_expired_evidence() {
        let store = Arc::new(TestHubStore::default());
        let now = Utc::now();
        // 命名空间必须先注册：清理作业按已注册命名空间逐个执行
        mem_core::GrantStore::ensure_namespace(store.as_ref(), "ns", &"test-agent".to_string())
            .await
            .unwrap();

        let expired = NewEvidence {
            id: None,
            namespace: "ns".into(),
            source_kind: EvidenceSourceKind::Session,
            source_ref: None,
            content_hash: "h1".into(),
            excerpt: Some("旧证据".into()),
            occurred_at: None,
            expires_at: Some(now - chrono::Duration::days(1)),
        };
        let keep = NewEvidence {
            id: None,
            namespace: "ns".into(),
            source_kind: EvidenceSourceKind::Session,
            source_ref: None,
            content_hash: "h2".into(),
            excerpt: Some("未到期证据".into()),
            occurred_at: None,
            expires_at: Some(now + chrono::Duration::days(1)),
        };
        let permanent = NewEvidence {
            id: None,
            namespace: "ns".into(),
            source_kind: EvidenceSourceKind::Session,
            source_ref: None,
            content_hash: "h3".into(),
            excerpt: Some("永久保留证据".into()),
            occurred_at: None,
            expires_at: None,
        };
        store.append_evidence(&expired).await.unwrap();
        store.append_evidence(&keep).await.unwrap();
        store.append_evidence(&permanent).await.unwrap();

        let job = EvidenceRetentionJob::new(store.clone() as Arc<dyn MemoryHubStore>, "test");
        let purged = job.run_once(now).await.unwrap();
        assert_eq!(purged, 1, "只应清理已到期的证据");

        let left = store.all_evidence_in_namespace("ns").await.unwrap();
        assert_eq!(left.len(), 2, "未到期与永久保留的证据不应被删除");
    }
}
