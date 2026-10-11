//! 混合检索引擎：FTS + 向量双路召回 → 五维加权融合 → MMR 重排
//!
//! 设计要点：
//! - **双路召回**：关键词（`text_contains` 走存储层 FTS）与语义（`similar`
//!   走 pgvector HNSW）并行执行，任一路失败不影响另一路；
//! - **软超时**：整体受 `soft_timeout_ms` 约束，超时时降级为纯列表召回，
//!   保证检索永不无限期挂起；
//! - **融合打分**：sim / kw / conf / rec / fresh 五维加权（权重见配置）；
//! - **MMR 重排**：在相关性与多样性间取平衡，避免 top-k 全是近重复内容。

use async_trait::async_trait;
use common::{AuthCtx, Result};
use mem_core::{
    algorithm::ItemScoreParams, calculate_item_score, cosine_similarity, keyword_score,
    l2_normalize, recency_score, Embedder, MemoryEntry, MemoryFilter, MemoryStatus, MemoryStore,
    Retriever, Scored, SearchQuery,
};
use std::sync::Arc;
use std::time::Duration;

/// 近期性半衰期（天）
pub const DEFAULT_RECENCY_TAU_DAYS: f32 = 30.0;

/// 混合检索器
#[derive(Clone)]
pub struct HybridRetriever {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn Embedder>,
    fts_k: usize,
    vec_k: usize,
    score_weights: [f32; 5],
    mmr_lambda: f32,
    soft_timeout: Duration,
    recency_tau_days: f32,
}

impl HybridRetriever {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        embedder: Arc<dyn Embedder>,
        fts_k: usize,
        vec_k: usize,
        score_weights: [f32; 5],
        mmr_lambda: f32,
        soft_timeout_ms: u64,
    ) -> Self {
        Self {
            store,
            embedder,
            fts_k: fts_k.max(1),
            vec_k: vec_k.max(1),
            score_weights,
            mmr_lambda: mmr_lambda.clamp(0.0, 1.0),
            soft_timeout: Duration::from_millis(soft_timeout_ms.max(1)),
            recency_tau_days: DEFAULT_RECENCY_TAU_DAYS,
        }
    }

    /// 自定义近期性半衰期（builder）
    pub fn with_recency_tau(mut self, tau_days: f32) -> Self {
        self.recency_tau_days = tau_days.max(1.0);
        self
    }

    /// 关键词路召回
    async fn fts_candidates(&self, q: &SearchQuery) -> Vec<Scored<MemoryEntry>> {
        if q.query_text.trim().is_empty() {
            return Vec::new();
        }
        let filter = MemoryFilter {
            namespace: Some(q.namespace.clone()),
            kinds: q.kinds.clone(),
            statuses: Some(if q.include_archived {
                vec![MemoryStatus::Active, MemoryStatus::Archived]
            } else {
                vec![MemoryStatus::Active]
            }),
            text_contains: Some(q.query_text.clone()),
            limit: Some(self.fts_k),
            ..Default::default()
        };
        let items = self.store.list(&filter).await.unwrap_or_default();
        items
            .into_iter()
            .map(|item| Scored { item, score: 0.0 })
            .collect()
    }

    /// 向量路召回
    async fn vec_candidates(&self, q: &SearchQuery) -> Vec<Scored<MemoryEntry>> {
        if q.query_text.trim().is_empty() {
            return Vec::new();
        }
        let vectors = match self
            .embedder
            .embed(std::slice::from_ref(&q.query_text))
            .await
        {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let Some(mut qv) = vectors.into_iter().next() else {
            return Vec::new();
        };
        l2_normalize(&mut qv);
        self.store
            .similar(&q.namespace, &qv, self.vec_k, q.kinds.as_deref())
            .await
            .unwrap_or_default()
    }

    /// 兜底：纯列表召回（超时或查询为空时）
    async fn fallback_candidates(&self, q: &SearchQuery) -> Vec<Scored<MemoryEntry>> {
        let filter = MemoryFilter {
            namespace: Some(q.namespace.clone()),
            kinds: q.kinds.clone(),
            statuses: Some(if q.include_archived {
                vec![MemoryStatus::Active, MemoryStatus::Archived]
            } else {
                vec![MemoryStatus::Active]
            }),
            limit: Some(q.top_k.max(1)),
            ..Default::default()
        };
        self.store
            .list(&filter)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|item| Scored { item, score: 0.0 })
            .collect()
    }

    /// 合并双路结果并按 ID 去重
    fn merge_candidates(
        fts: Vec<Scored<MemoryEntry>>,
        vec: Vec<Scored<MemoryEntry>>,
    ) -> Vec<(MemoryEntry, Option<f32>)> {
        let mut out: Vec<(MemoryEntry, Option<f32>)> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for s in vec {
            if seen.insert(s.item.id) {
                out.push((s.item, Some(s.score)));
            }
        }
        for s in fts {
            if seen.insert(s.item.id) {
                out.push((s.item, None));
            }
        }
        out
    }

    /// 五维加权打分
    fn score_items(
        &self,
        candidates: Vec<(MemoryEntry, Option<f32>)>,
        q: &SearchQuery,
    ) -> Vec<Scored<MemoryEntry>> {
        let now = chrono::Utc::now();
        let mut scored: Vec<Scored<MemoryEntry>> = candidates
            .into_iter()
            .map(|(item, sim)| {
                let kw = keyword_score(&item.content, &q.query_text, &item.tags);
                let rec = recency_score(now, item.updated_at, self.recency_tau_days);
                let fresh = if item.version > 1 { 1.0 } else { 0.5 };
                let score = calculate_item_score(
                    &ItemScoreParams {
                        sim: sim.unwrap_or(0.0),
                        kw,
                        conf: item.confidence,
                        rec,
                        fresh,
                    },
                    &self.score_weights,
                );
                Scored { item, score }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored
    }

    /// MMR 重排（嵌入缺失项的相互相似度按 0 处理）
    fn mmr_rerank(&self, scored: Vec<Scored<MemoryEntry>>, k: usize) -> Vec<Scored<MemoryEntry>> {
        let n = scored.len();
        if n <= 1 {
            return scored;
        }
        let sim_matrix: Vec<Vec<f32>> = scored
            .iter()
            .map(|a| {
                scored
                    .iter()
                    .map(|b| match (&a.item.embedding, &b.item.embedding) {
                        (Some(x), Some(y)) => cosine_similarity(x, y).max(0.0),
                        _ => 0.0,
                    })
                    .collect()
            })
            .collect();

        let scores: Vec<f32> = scored.iter().map(|s| s.score).collect();
        let order = mem_core::mmr_rerank(&scores, &sim_matrix, k, self.mmr_lambda);
        order.into_iter().map(|i| scored[i].clone()).collect()
    }
}

#[async_trait]
impl Retriever for HybridRetriever {
    async fn search(&self, _auth: &AuthCtx, q: &SearchQuery) -> Result<Vec<Scored<MemoryEntry>>> {
        let top_k = q.top_k.max(1);

        // 空查询直接走兜底列表
        if q.query_text.trim().is_empty() {
            let items = self.fallback_candidates(q).await;
            let candidates = items
                .into_iter()
                .map(|s| (s.item, None))
                .collect::<Vec<_>>();
            return Ok(self
                .score_items(candidates, q)
                .into_iter()
                .take(top_k)
                .collect());
        }

        // 双路并行 + 软超时
        let fts = self.fts_candidates(q);
        let vec_c = self.vec_candidates(q);
        let joined =
            tokio::time::timeout(self.soft_timeout, async { tokio::join!(fts, vec_c) }).await;

        let merged = match joined {
            Ok((fts_res, vec_res)) => Self::merge_candidates(fts_res, vec_res),
            Err(_) => {
                tracing::warn!(
                    timeout_ms = self.soft_timeout.as_millis(),
                    namespace = %q.namespace,
                    "检索软超时，降级为列表召回"
                );
                self.fallback_candidates(q)
                    .await
                    .into_iter()
                    .map(|s| (s.item, None))
                    .collect::<Vec<_>>()
            }
        };

        if merged.is_empty() {
            return Ok(Vec::new());
        }

        let this = self.clone();
        let q_clone = q.clone();
        let reranked = tokio::task::spawn_blocking(move || {
            let scored = this.score_items(merged, &q_clone);
            this.mmr_rerank(scored, top_k)
        })
        .await
        .map_err(|e| common::HubError::Internal(anyhow::anyhow!("检索重排任务调度失败: {}", e)))?;

        Ok(reranked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::HubError;
    use mem_core::testing::TestEmbedder;
    use mem_core::MemoryKind;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::RwLock;
    use uuid::Uuid;

    /// 测试用内存存储（仅实现检索所需路径）
    #[derive(Default)]
    struct TestStore {
        items: RwLock<BTreeMap<(String, Uuid), MemoryEntry>>,
    }

    impl TestStore {
        fn add(&self, mut e: MemoryEntry, embedding: Option<Vec<f32>>) {
            e.embedding = embedding;
            self.items
                .write()
                .unwrap()
                .insert((e.namespace.clone(), e.id), e);
        }
    }

    #[async_trait]
    impl MemoryStore for TestStore {
        async fn insert(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            self.add(e.clone(), None);
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
        async fn get_by_key(&self, _ns: &str, _key: &str) -> Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn save(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            Ok(e.clone())
        }
        async fn delete(&self, _ns: &str, _id: Uuid) -> Result<()> {
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
                .filter(|e| {
                    filter
                        .text_contains
                        .as_ref()
                        .is_none_or(|t| e.content.to_lowercase().contains(&t.to_lowercase()))
                })
                .cloned()
                .collect();
            out.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
            if let Some(limit) = filter.limit {
                out.truncate(limit);
            }
            Ok(out)
        }
        async fn count(&self, _filter: &MemoryFilter) -> Result<usize> {
            Ok(self.items.read().unwrap().len())
        }
        async fn similar(
            &self,
            ns: &str,
            qv: &[f32],
            k: usize,
            _kinds: Option<&[MemoryKind]>,
        ) -> Result<Vec<Scored<MemoryEntry>>> {
            let mut out: Vec<Scored<MemoryEntry>> = self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| e.namespace == ns && e.status == MemoryStatus::Active)
                .filter_map(|e| {
                    let iv = e.embedding.as_ref()?;
                    Some(Scored {
                        item: e.clone(),
                        score: cosine_similarity(qv, iv),
                    })
                })
                .collect();
            out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
            out.truncate(k);
            Ok(out)
        }
        async fn all_in_namespace(&self, ns: &str, _m: bool) -> Result<Vec<MemoryEntry>> {
            Ok(self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| e.namespace == ns)
                .cloned()
                .collect())
        }
        async fn list_namespaces(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn record_version(&self, _v: &mem_core::MemoryVersion) -> Result<()> {
            Ok(())
        }
        async fn versions(&self, _id: Uuid) -> Result<Vec<mem_core::MemoryVersion>> {
            Ok(vec![])
        }
        async fn archive_expired(&self, _now: chrono::DateTime<chrono::Utc>) -> Result<usize> {
            Ok(0)
        }
    }

    fn entry(ns: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec![],
            metadata: json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "agent-a".to_string(),
            confidence: 1.0,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: None,
            merged_into: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn retriever(store: Arc<TestStore>) -> HybridRetriever {
        HybridRetriever::new(
            store,
            Arc::new(TestEmbedder::default()),
            20,
            20,
            [0.5, 0.2, 0.1, 0.1, 0.1],
            0.7,
            500,
        )
    }

    fn query(ns: &str, text: &str) -> SearchQuery {
        SearchQuery {
            namespace: ns.to_string(),
            query_text: text.to_string(),
            kinds: None,
            tags: None,
            top_k: 5,
            include_archived: false,
        }
    }

    fn auth() -> AuthCtx {
        AuthCtx {
            agent_id: "agent-a".to_string(),
            user_id: None,
            scopes: vec![],
            acl: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_fts_only_when_no_embeddings() {
        let store = Arc::new(TestStore::default());
        store.add(entry("ns", "用户喜欢喝美式咖啡"), None);
        store.add(entry("ns", "服务器部署在东京"), None);

        let r = retriever(store);
        let res = r.search(&auth(), &query("ns", "咖啡")).await.unwrap();
        assert_eq!(res.len(), 1);
        assert!(res[0].item.content.contains("咖啡"));
    }

    #[tokio::test]
    async fn test_vector_path_boosts_semantic_match() {
        let embedder = Arc::new(TestEmbedder::default());
        let store = Arc::new(TestStore::default());

        // 两条条目：内容不同，但其中一条与查询同文本（向量 cosine = 1）
        let v_same = embedder
            .embed(&["部署架构".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut e_same = entry("ns", "部署架构");
        e_same.embedding = Some(v_same);
        store.add(e_same, None);

        store.add(entry("ns", "午餐吃了沙拉"), None);

        let r = HybridRetriever::new(
            store.clone(),
            embedder,
            20,
            20,
            [0.9, 0.1, 0.0, 0.0, 0.0],
            0.7,
            500,
        );
        let res = r.search(&auth(), &query("ns", "部署架构")).await.unwrap();
        assert_eq!(res[0].item.content, "部署架构");
    }

    #[tokio::test]
    async fn test_empty_query_falls_back_to_list() {
        let store = Arc::new(TestStore::default());
        store.add(entry("ns", "第一条"), None);
        store.add(entry("ns", "第二条"), None);

        let r = retriever(store);
        let res = r.search(&auth(), &query("ns", "  ")).await.unwrap();
        assert_eq!(res.len(), 2);
    }

    #[tokio::test]
    async fn test_top_k_limit_enforced() {
        let store = Arc::new(TestStore::default());
        for i in 0..10 {
            store.add(entry("ns", &format!("记忆 {}", i)), None);
        }
        let r = retriever(store);
        let mut q = query("ns", "记忆");
        q.top_k = 3;
        let res = r.search(&auth(), &q).await.unwrap();
        assert_eq!(res.len(), 3);
    }

    #[tokio::test]
    async fn test_namespace_isolation() {
        let store = Arc::new(TestStore::default());
        store.add(entry("ns-a", "A 的咖啡记忆"), None);
        store.add(entry("ns-b", "B 的咖啡记忆"), None);

        let r = retriever(store);
        let res = r.search(&auth(), &query("ns-a", "咖啡")).await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].item.namespace, "ns-a");
    }
}
