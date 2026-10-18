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
        let mut statuses = vec![MemoryStatus::Active];
        if q.include_archived || q.include_superseded {
            statuses.push(MemoryStatus::Archived);
        }
        let filter = MemoryFilter {
            namespace: Some(q.namespace.clone()),
            kinds: q.kinds.clone(),
            statuses: Some(statuses),
            text_contains: Some(q.query_text.clone()),
            scene: q.scene,
            layer: q.layer,
            valid_at: q.valid_at,
            // 候选池必须容得下 top_k：此前固定取 fts_k(默认 20)，
            // 而工具契约允许 top_k 到 100，超出的请求会被静默截断。
            limit: Some(self.fts_k.max(q.top_k)),
            ..Default::default()
        };
        let items = match self.store.list(&filter).await {
            Ok(v) => v,
            Err(e) => {
                // 降级必须可观测：静默返回空会让"检索没结果"看起来像"库里没有"
                tracing::warn!(error = %e, namespace = %q.namespace, "关键词召回失败，该路结果为空");
                Vec::new()
            }
        };
        items
            .into_iter()
            .map(|item| Scored { item, score: 0.0 })
            .collect()
    }

    /// 向量路召回（查询向量由 `search` 统一算好后传入，避免双路各嵌一次）
    async fn vec_candidates(
        &self,
        q: &SearchQuery,
        qv: Option<&[f32]>,
    ) -> Vec<Scored<MemoryEntry>> {
        if q.query_text.trim().is_empty() {
            return Vec::new();
        }
        let Some(qv) = qv else {
            return Vec::new();
        };
        let items = match self
            .store
            .similar(
                &q.namespace,
                qv,
                self.vec_k.max(q.top_k * 2),
                q.kinds.as_deref(),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, namespace = %q.namespace, "向量召回失败，该路结果为空");
                Vec::new()
            }
        };

        // 认知过滤：scene、layer、valid_at 与状态约束
        let now = chrono::Utc::now();
        items
            .into_iter()
            .filter(|scored| {
                if let Some(scene) = &q.scene {
                    if scored.item.scene.as_ref() != Some(scene) {
                        return false;
                    }
                }
                if let Some(layer) = &q.layer {
                    if scored.item.layer.as_ref() != Some(layer) {
                        return false;
                    }
                }
                if !q.include_superseded {
                    if let Some(valid_at) = q.valid_at {
                        if !scored.item.is_valid_at(valid_at) {
                            return false;
                        }
                    } else if scored.item.is_superseded_or_expired(now) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    /// 兜底：纯列表召回（超时或查询为空时）
    async fn fallback_candidates(&self, q: &SearchQuery) -> Vec<Scored<MemoryEntry>> {
        let mut statuses = vec![MemoryStatus::Active];
        if q.include_archived || q.include_superseded {
            statuses.push(MemoryStatus::Archived);
        }
        let filter = MemoryFilter {
            namespace: Some(q.namespace.clone()),
            kinds: q.kinds.clone(),
            statuses: Some(statuses),
            scene: q.scene,
            layer: q.layer,
            valid_at: q.valid_at,
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
        query_vec: Option<&[f32]>,
    ) -> Vec<Scored<MemoryEntry>> {
        let now = chrono::Utc::now();
        let mut scored: Vec<Scored<MemoryEntry>> = candidates
            .into_iter()
            .filter(|(item, _)| {
                if !q.include_superseded {
                    if let Some(valid_at) = q.valid_at {
                        if !item.is_valid_at(valid_at) {
                            return false;
                        }
                    } else if item.is_superseded_or_expired(now) {
                        return false;
                    }
                }
                if let Some(scene) = &q.scene {
                    if item.scene.as_ref() != Some(scene) {
                        return false;
                    }
                }
                if let Some(layer) = &q.layer {
                    if item.layer.as_ref() != Some(layer) {
                        return false;
                    }
                }
                true
            })
            .map(|(item, sim)| {
                // 关键词路命中此前一律记 sim=0，而 sim 权重高达 0.50 —— 于是
                // "全文精确命中"最高只能得 0.5 分，被任何中等向量命中压过，
                // 双路融合名不副实。这里用条目自带向量与查询向量现算相似度。
                let sim = sim.or_else(|| {
                    let qv = query_vec?;
                    item.embedding
                        .as_deref()
                        .map(|e| cosine_similarity(qv, e).clamp(0.0, 1.0))
                });
                let kw = keyword_score(&item.content, &q.query_text, &item.tags);
                let rec = recency_score(now, item.updated_at, self.recency_tau_days);
                let fresh = if item.version > 1 { 1.0 } else { 0.5 };
                let mut score = calculate_item_score(
                    &ItemScoreParams {
                        sim: sim.unwrap_or(0.0),
                        kw,
                        conf: item.confidence,
                        rec,
                        fresh,
                    },
                    &self.score_weights,
                );
                // 场景自适应加成：显式命中指定场景提升 +0.05
                if let Some(scene) = &q.scene {
                    if item.scene.as_ref() == Some(scene) {
                        score = (score + 0.05).min(1.0);
                    }
                }
                // 演化回溯时，已失效的事实打折（* 0.5），使现行有效事实自然胜出
                if item.is_superseded_or_expired(now) {
                    score *= 0.5;
                }
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
            let mut scored = self.score_items(candidates, q, None);
            if let Some(min_s) = q.min_score {
                scored.retain(|s| s.score >= min_s);
            }
            return Ok(scored.into_iter().take(top_k).collect());
        }

        // 查询向量只算一次，双路召回与打分共用
        let query_vec: Option<Vec<f32>> = match self
            .embedder
            .embed(std::slice::from_ref(&q.query_text))
            .await
        {
            Ok(mut vectors) => vectors
                .pop()
                .map(|mut v| {
                    l2_normalize(&mut v);
                    v
                })
                .or_else(|| {
                    tracing::warn!(namespace = %q.namespace, "嵌入服务返回空向量列表");
                    None
                }),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    namespace = %q.namespace,
                    "查询嵌入失败：向量路不可用，仅关键词召回"
                );
                None
            }
        };

        // 双路并行 + 软超时
        let fts = self.fts_candidates(q);
        let vec_c = self.vec_candidates(q, query_vec.as_deref());
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
        let qv_clone = query_vec;
        let reranked = tokio::task::spawn_blocking(move || {
            let scored = this.score_items(merged, &q_clone, qv_clone.as_deref());
            let reranked = this.mmr_rerank(scored, top_k);
            if let Some(min_s) = q_clone.min_score {
                reranked.into_iter().filter(|s| s.score >= min_s).collect()
            } else {
                reranked
            }
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
                .filter(|e| {
                    filter
                        .statuses
                        .as_ref()
                        .is_none_or(|statuses| statuses.contains(&e.status))
                })
                .filter(|e| {
                    filter
                        .scene
                        .as_ref()
                        .is_none_or(|scene| e.scene.as_ref() == Some(scene))
                })
                .filter(|e| {
                    filter
                        .layer
                        .as_ref()
                        .is_none_or(|layer| e.layer.as_ref() == Some(layer))
                })
                .filter(|e| {
                    filter
                        .valid_at
                        .as_ref()
                        .is_none_or(|valid_at| e.is_valid_at(*valid_at))
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
            scene: None,
            layer: None,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
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
            ..Default::default()
        }
    }

    fn auth() -> AuthCtx {
        AuthCtx {
            agent_id: "agent-a".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![],
            acl: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_fts_hit_gets_similarity_from_query_vector() {
        let r = retriever(Arc::new(TestStore::default()));
        let embedder = TestEmbedder::default();
        let text = "服务部署在东京区域";
        let mut qv = embedder.embed(&[text.to_string()]).await.unwrap().remove(0);
        l2_normalize(&mut qv);

        let mut item = entry("ns", text);
        item.embedding = Some(qv.clone());

        // 没有查询向量时（如嵌入服务故障），关键词命中只能拿 sim=0 的那部分分
        let without = r.score_items(vec![(item.clone(), None)], &query("ns", "东京"), None);
        // 带查询向量时同一命中应现算出 sim≈1.0，而不是被当作零相似度判死
        let with = r.score_items(vec![(item, None)], &query("ns", "东京"), Some(&qv));

        assert!(
            with[0].score > without[0].score,
            "关键词命中的相似度不应恒为 0：{:#?} vs {:#?}",
            with[0].score,
            without[0].score
        );
        // sim 维度权重为 0.5，补回后总分应抬升接近 0.5
        assert!(
            with[0].score - without[0].score > 0.45,
            "应补回 sim 权重的几乎全部：{:#?}",
            with[0].score - without[0].score
        );
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

    #[test]
    fn test_cjk_bigram_fts_tokenization_matching() {
        let doc = "服务部署在东京区域，架构采用微服务";
        let doc_tokens = mem_core::tokenize_for_fts(doc);

        // 验证针对"东京"查询，词元中包含"东京"
        let q1 = mem_core::tokenize_query_for_fts("东京");
        assert_eq!(q1, "东京");
        assert!(doc_tokens.contains(&q1));

        // 验证针对"微服务"查询，查询拆成"微服 服务"，doc_tokens 均包含
        let q2 = mem_core::tokenize_query_for_fts("微服务");
        for term in q2.split_whitespace() {
            assert!(
                doc_tokens.contains(term),
                "文档词元应包含查询 2-gram: {}",
                term
            );
        }
    }

    #[tokio::test]
    async fn test_scene_aware_filtering_and_boost() {
        let store = Arc::new(TestStore::default());
        let mut chat_item = entry("ns", "对话中记录的偏好");
        chat_item.scene = Some(mem_core::MemoryScene::Chat);
        let mut coding_item = entry("ns", "对话中讨论的代码重构方案");
        coding_item.scene = Some(mem_core::MemoryScene::Coding);

        store.add(chat_item, None);
        store.add(coding_item, None);

        let r = retriever(store);

        // 1. 指定 scene 为 Chat 时，仅返回 Chat 条目
        let mut q_chat = query("ns", "对话");
        q_chat.scene = Some(mem_core::MemoryScene::Chat);
        let res_chat = r.search(&auth(), &q_chat).await.unwrap();
        assert_eq!(res_chat.len(), 1);
        assert_eq!(res_chat[0].item.scene, Some(mem_core::MemoryScene::Chat));

        // 2. 指定 scene 为 Coding 时，仅返回 Coding 条目
        let mut q_coding = query("ns", "对话");
        q_coding.scene = Some(mem_core::MemoryScene::Coding);
        let res_coding = r.search(&auth(), &q_coding).await.unwrap();
        assert_eq!(res_coding.len(), 1);
        assert_eq!(
            res_coding[0].item.scene,
            Some(mem_core::MemoryScene::Coding)
        );

        // 3. 不指定 scene 时向后兼容，返回所有 scene
        let q_any = query("ns", "对话");
        let res_any = r.search(&auth(), &q_any).await.unwrap();
        assert_eq!(res_any.len(), 2);
    }

    #[tokio::test]
    async fn test_temporal_validity_filtering() {
        use chrono::{TimeZone, Utc};
        let store = Arc::new(TestStore::default());

        let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap();

        // 事实 A：2026上半年有效
        let mut fact_a = entry("ns", "公司默认使用 Python 3.10 运行时");
        fact_a.valid_from = Some(t1);
        fact_a.valid_until = Some(t2);

        // 事实 B：2026下半年有效
        let mut fact_b = entry("ns", "公司默认使用 Python 3.12 运行时");
        fact_b.valid_from = Some(t2);
        fact_b.valid_until = Some(t3);

        store.add(fact_a, None);
        store.add(fact_b, None);

        let r = retriever(store);

        // 查询 2026-03-15 的有效事实
        let mut q_early = query("ns", "Python");
        q_early.valid_at = Some(Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap());
        let res_early = r.search(&auth(), &q_early).await.unwrap();
        assert_eq!(res_early.len(), 1);
        assert!(res_early[0].item.content.contains("3.10"));

        // 查询 2026-08-15 的有效事实
        let mut q_late = query("ns", "Python");
        q_late.valid_at = Some(Utc.with_ymd_and_hms(2026, 8, 15, 0, 0, 0).unwrap());
        let res_late = r.search(&auth(), &q_late).await.unwrap();
        assert_eq!(res_late.len(), 1);
        assert!(res_late[0].item.content.contains("3.12"));
    }

    #[tokio::test]
    async fn test_superseded_fact_exclusion_and_inclusion() {
        let store = Arc::new(TestStore::default());

        let mut active_fact = entry("ns", "数据库主库部署在上海可用区A");
        active_fact.valid_until = None;

        let mut superseded_fact = entry("ns", "数据库主库部署在杭州可用区B");
        superseded_fact.valid_until = Some(chrono::Utc::now() - chrono::Duration::days(1));

        store.add(active_fact, None);
        store.add(superseded_fact, None);

        let r = retriever(store);

        // 1. 默认情况下，已失效的事实必须被过滤，不返回
        let q_default = query("ns", "数据库主库部署");
        let res_default = r.search(&auth(), &q_default).await.unwrap();
        assert_eq!(res_default.len(), 1);
        assert!(res_default[0].item.content.contains("上海可用区A"));

        // 2. 当显式请求 include_superseded 时，允许返回已失效的事实用于演化回溯
        let mut q_hist = query("ns", "数据库主库部署");
        q_hist.include_superseded = true;
        let res_hist = r.search(&auth(), &q_hist).await.unwrap();
        assert_eq!(res_hist.len(), 2);
        // Active 事实排在第一，Superseded 受到 0.5 惩罚分排在后
        assert!(res_hist[0].item.valid_until.is_none());
        assert!(res_hist[1].item.valid_until.is_some());
        assert!(res_hist[0].score > res_hist[1].score);
    }

    #[tokio::test]
    async fn test_min_score_filter() {
        let store = Arc::new(TestStore::default());
        store.add(entry("ns", "完全精确匹配目标查询"), None);
        store.add(entry("ns", "毫无关系的无关文档"), None);

        let r = retriever(store);
        let mut q = query("ns", "目标查询");
        q.min_score = Some(0.01);
        let res = r.search(&auth(), &q).await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].item.content, "完全精确匹配目标查询");
    }
}
