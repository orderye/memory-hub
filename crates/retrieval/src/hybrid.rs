//! 混合检索引擎：FTS + 向量双路召回 → 五维加权融合 → MMR 重排
//!
//! 设计要点：
//! - **双路召回**：关键词（`text_contains` 走存储层 FTS）与语义（`similar`
//!   走 pgvector HNSW）并行执行，任一路失败不影响另一路；
//! - **软超时**：整体受 `soft_timeout_ms` 约束，超时时降级为纯列表召回，
//!   保证检索永不无限期挂起；
//! - **融合打分**：sim / kw / conf / rec / prior 五维加权（权重见配置）；
//! - **MMR 重排**：在相关性与多样性间取平衡，避免 top-k 全是近重复内容。

use async_trait::async_trait;
use common::{AuthCtx, Result};
use mem_core::{
    algorithm::ItemScoreParams, calculate_item_score_with_availability, cosine_similarity,
    document_tokens, extract_terms, inverse_document_frequency, is_superseded_at,
    keyword_score_weighted, l2_normalize, layer_prior, matches_tag_filter, passes_validity_filter,
    recency_score, recency_tau_days_for, retrieval_reference_time, scene_preference_score,
    validity_filter_time, Clock, Embedder, MemoryEntry, MemoryFilter, MemoryScene, MemoryStatus,
    MemoryStore, Retriever, SceneMode, Scored, SearchQuery, SystemClock, SCENE_PREFER_BONUS,
};
use std::sync::Arc;
use std::time::Duration;

/// 混合检索器
#[derive(Clone)]
pub struct HybridRetriever {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn Embedder>,
    clock: Arc<dyn Clock>,
    fts_k: usize,
    vec_k: usize,
    score_weights: [f32; 5],
    mmr_lambda: f32,
    soft_timeout: Duration,
    recency_tau_override: Option<f32>,
    scene_mode: SceneMode,
    /// 分场景权重 profile（按请求 scene 选择；缺省回落到全局权重）
    scene_weight_profiles: std::collections::HashMap<MemoryScene, [f32; 5]>,
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
            clock: Arc::new(SystemClock),
            fts_k: fts_k.max(1),
            vec_k: vec_k.max(1),
            score_weights,
            mmr_lambda: mmr_lambda.clamp(0.0, 1.0),
            soft_timeout: Duration::from_millis(soft_timeout_ms.max(1)),
            recency_tau_override: None,
            scene_mode: SceneMode::Strict,
            scene_weight_profiles: std::collections::HashMap::new(),
        }
    }

    /// 覆盖近期性时间常数（builder）：默认按记忆类型区分（见 [`recency_tau_days_for`]）
    pub fn with_recency_tau(mut self, tau_days: f32) -> Self {
        self.recency_tau_override = Some(tau_days.max(1.0));
        self
    }

    /// 场景过滤模式（builder）：默认 `Strict` 硬过滤，`Prefer` 为软偏好
    pub fn with_scene_mode(mut self, mode: SceneMode) -> Self {
        self.scene_mode = mode;
        self
    }

    /// 分场景权重 profile（builder）：请求指定 scene 时优先使用对应权重
    pub fn with_scene_weight_profiles(
        mut self,
        profiles: std::collections::HashMap<MemoryScene, [f32; 5]>,
    ) -> Self {
        self.scene_weight_profiles = profiles;
        self
    }

    /// 场景过滤下推：strict 模式硬过滤；prefer 模式不下推（由打分降权处理）
    fn scene_pushdown(&self, q: &SearchQuery) -> Option<MemoryScene> {
        match self.scene_mode {
            SceneMode::Strict => q.scene,
            SceneMode::Prefer => None,
        }
    }

    /// 打分语义下的场景保留判定
    fn scene_keeps(&self, q: &SearchQuery, item: &MemoryEntry) -> bool {
        q.scene.is_none() || self.scene_mode == SceneMode::Prefer || item.scene == q.scene
    }

    /// 本次请求生效的五维权重：分场景 profile 优先
    fn weights_for(&self, q: &SearchQuery) -> [f32; 5] {
        q.scene
            .and_then(|s| self.scene_weight_profiles.get(&s).copied())
            .unwrap_or(self.score_weights)
    }

    /// 注入时钟（builder）：评测使用固定时钟，生产使用系统时钟
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// 条目向量是否可用于比较：存在向量，且嵌入模型与当前查询嵌入器一致
    /// （跨模型向量不可比，按"嵌入缺失"处理）
    fn embedding_usable(&self, item: &MemoryEntry) -> bool {
        item.embedding.is_some()
            && item
                .embedding_model
                .as_deref()
                .is_none_or(|m| m == self.embedder.model_id())
    }

    /// 关键词路召回
    async fn fts_candidates(&self, q: &SearchQuery) -> Vec<Scored<MemoryEntry>> {
        if q.query_text.trim().is_empty() {
            return Vec::new();
        }
        let filter = MemoryFilter {
            namespace: Some(q.namespace.clone()),
            kinds: q.kinds.clone(),
            statuses: Some(self.statuses_for(q)),
            tags: q.tags.clone(),
            text_contains: Some(q.query_text.clone()),
            scene: self.scene_pushdown(q),
            layer: q.layer,
            // 时间有效性过滤下推到存储层（与打分路径同一语义）
            valid_at: validity_filter_time(q.valid_at, q.include_superseded, self.clock.now()),
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
        // 过滤条件下推到 SQL/存储层：此前 scene/valid_at 只在 Rust 侧事后过滤，
        // 其它场景或已失效条目会先占满 top-k，导致目标场景候选严重缺失
        let filter = MemoryFilter {
            namespace: Some(q.namespace.clone()),
            kinds: q.kinds.clone(),
            statuses: Some(self.statuses_for(q)),
            tags: q.tags.clone(),
            scene: self.scene_pushdown(q),
            layer: q.layer,
            valid_at: validity_filter_time(q.valid_at, q.include_superseded, self.clock.now()),
            ..Default::default()
        };
        let items = match self
            .store
            .similar(&filter, qv, self.vec_k.max(q.top_k * 2))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, namespace = %q.namespace, "向量召回失败，该路结果为空");
                Vec::new()
            }
        };

        // 存储层过滤是同一套条件，这里只做防御性复核（存储实现差异兜底）
        let now = self.clock.now();
        items
            .into_iter()
            .filter(|scored| {
                passes_validity_filter(&scored.item, q.valid_at, q.include_superseded, now)
                    && matches_tag_filter(&scored.item, &q.tags)
                    && (q.scene.is_none() || scored.item.scene == q.scene)
                    && (q.layer.is_none() || scored.item.layer == q.layer)
            })
            .collect()
    }

    /// 状态集合：默认只召回 active，显式请求归档/回溯时纳入 archived
    fn statuses_for(&self, q: &SearchQuery) -> Vec<MemoryStatus> {
        let mut statuses = vec![MemoryStatus::Active];
        if q.include_archived || q.include_superseded {
            statuses.push(MemoryStatus::Archived);
        }
        statuses
    }

    /// 兜底：纯列表召回（超时或查询为空时）
    async fn fallback_candidates(&self, q: &SearchQuery) -> Vec<Scored<MemoryEntry>> {
        let filter = MemoryFilter {
            namespace: Some(q.namespace.clone()),
            kinds: q.kinds.clone(),
            statuses: Some(self.statuses_for(q)),
            tags: q.tags.clone(),
            scene: q.scene,
            layer: q.layer,
            valid_at: validity_filter_time(q.valid_at, q.include_superseded, self.clock.now()),
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
        let now = self.clock.now();
        // 过滤与惩罚共用同一参考时点：显式 valid_at 优先，否则墙上时钟
        let reference_time = retrieval_reference_time(q.valid_at, now);

        // 关键词命中度按池内 IDF 加权：稀有词元主导命中度，常见 bigram
        // （"的数""们的"）权重自然衰减；词元与 FTS 索引同源（词元集合匹配，
        // 不再用裸子串，避免 api 命中 rapid）
        let terms = extract_terms(&q.query_text);
        let candidate_tokens: Vec<std::collections::HashSet<String>> = candidates
            .iter()
            .map(|(item, _)| document_tokens(&item.content))
            .collect();
        let pool_size = candidate_tokens.len();
        let doc_freq: Vec<usize> = terms
            .iter()
            .map(|t| {
                candidate_tokens
                    .iter()
                    .filter(|set| set.contains(t))
                    .count()
            })
            .collect();
        let idf = |term: &str| -> f32 {
            terms
                .iter()
                .position(|t| t == term)
                .map(|i| inverse_document_frequency(pool_size, doc_freq[i]))
                .unwrap_or(1.0)
        };

        // 分场景权重 profile：指定场景且配置了 profile 时优先使用
        let weights = self.weights_for(q);
        let mut scored: Vec<Scored<MemoryEntry>> = candidates
            .into_iter()
            .zip(candidate_tokens)
            .filter(|((item, _), _)| {
                passes_validity_filter(item, q.valid_at, q.include_superseded, now)
                    && matches_tag_filter(item, &q.tags)
                    && self.scene_keeps(q, item)
                    && (q.layer.is_none() || item.layer == q.layer)
            })
            .map(|((item, vec_sim), tokens)| {
                // 关键词路命中此前一律记 sim=0，而 sim 权重高达 0.50 —— 于是
                // "全文精确命中"最高只能得 0.5 分，被任何中等向量命中压过，
                // 双路融合名不副实。这里用条目自带向量与查询向量现算相似度。
                let embedding_usable = self.embedding_usable(&item);
                let sim = if embedding_usable {
                    vec_sim.or_else(|| {
                        let qv = query_vec?;
                        let e = item.embedding.as_deref()?;
                        Some(cosine_similarity(qv, e).clamp(0.0, 1.0))
                    })
                } else {
                    None
                };
                let kw = keyword_score_weighted(&tokens, &terms, &idf);
                // 近期性以事件时间为准（缺失回落写入时间），衰减常数按记忆类型区分
                let tau = self
                    .recency_tau_override
                    .unwrap_or_else(|| recency_tau_days_for(item.kind));
                let rec = recency_score(now, item.occurred_at.unwrap_or(item.updated_at), tau);
                let prior = layer_prior(item.layer);
                // 嵌入不可用（服务故障 / 缺失向量 / 模型不匹配）时按剩余
                // 信号归一化，而不是让该维度按 0 计使分数整体缩水
                let sim_available = sim.is_some();
                let mut score = calculate_item_score_with_availability(
                    &ItemScoreParams {
                        sim: sim.unwrap_or(0.0),
                        kw,
                        conf: item.confidence,
                        rec,
                        prior,
                    },
                    &weights,
                    [sim_available, true, true, true, true],
                );
                // 场景处理：strict 模式命中即 +0.05（与既有行为一致）；
                // prefer 模式按"同场景加成 / 其它场景降权 / 未标注中性"调整
                match self.scene_mode {
                    SceneMode::Strict => {
                        if let Some(scene) = &q.scene {
                            if item.scene.as_ref() == Some(scene) {
                                score = (score + SCENE_PREFER_BONUS).min(1.0);
                            }
                        }
                    }
                    SceneMode::Prefer => {
                        score = scene_preference_score(score, q.scene, item.scene);
                    }
                }
                // 演化回溯时，参考时点上已失效的事实打折（* 0.5），
                // 使参考时点内有效的事实自然胜出
                if is_superseded_at(&item, reference_time) {
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

    /// MMR 重排（嵌入缺失或模型不匹配的项相互相似度按 0 处理）
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
                    .map(|b| {
                        if self.embedding_usable(&a.item) && self.embedding_usable(&b.item) {
                            match (&a.item.embedding, &b.item.embedding) {
                                (Some(x), Some(y)) => cosine_similarity(x, y).max(0.0),
                                _ => 0.0,
                            }
                        } else {
                            0.0
                        }
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

        // 软超时预算统一起算：查询嵌入与双路召回共享同一 deadline，
        // 避免嵌入服务故障把整体耗时拖到 HTTP 客户端超时（10s）
        let deadline = tokio::time::Instant::now() + self.soft_timeout;

        // 查询向量只算一次，双路召回与打分共用
        let query_vec: Option<Vec<f32>> = match tokio::time::timeout_at(
            deadline,
            self.embedder.embed(std::slice::from_ref(&q.query_text)),
        )
        .await
        {
            Ok(Ok(mut vectors)) => vectors
                .pop()
                .map(|mut v| {
                    l2_normalize(&mut v);
                    v
                })
                .or_else(|| {
                    tracing::warn!(namespace = %q.namespace, "嵌入服务返回空向量列表");
                    None
                }),
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    namespace = %q.namespace,
                    "查询嵌入失败：向量路不可用，仅关键词召回"
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = self.soft_timeout.as_millis(),
                    namespace = %q.namespace,
                    "查询嵌入超出软超时预算：向量路不可用，仅关键词召回"
                );
                None
            }
        };

        // 双路并行且各自受剩余预算约束：超时只丢弃超时的一路，已完成的一路保留
        let (fts_res, vec_res) = tokio::join!(
            tokio::time::timeout_at(deadline, self.fts_candidates(q)),
            tokio::time::timeout_at(deadline, self.vec_candidates(q, query_vec.as_deref())),
        );
        let path_timed_out = fts_res.is_err() || vec_res.is_err();
        let merged =
            Self::merge_candidates(fts_res.unwrap_or_default(), vec_res.unwrap_or_default());

        let merged = if merged.is_empty() && path_timed_out {
            tracing::warn!(
                timeout_ms = self.soft_timeout.as_millis(),
                namespace = %q.namespace,
                "检索软超时且无已完成结果，降级为列表召回"
            );
            self.fallback_candidates(q)
                .await
                .into_iter()
                .map(|s| (s.item, None))
                .collect::<Vec<_>>()
        } else {
            merged
        };

        if merged.is_empty() {
            return Ok(Vec::new());
        }

        let this = self.clone();
        let q_clone = q.clone();
        let qv_clone = query_vec;
        let reranked = tokio::task::spawn_blocking(move || {
            let mut scored = this.score_items(merged, &q_clone, qv_clone.as_deref());
            // min_score 必须在 MMR 之前应用：否则 MMR 先按 top_k 选满、
            // 再被阈值删掉低分项，结果数会少于高阈值下本可返回的数量
            if let Some(min_s) = q_clone.min_score {
                scored.retain(|s| s.score >= min_s);
            }
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
        /// FTS 路的模拟耗时（用于软超时测试）
        list_delay: Option<Duration>,
        /// 记录每次 list 调用的 text_contains 过滤条件（None 即兜底列表召回）
        list_filters: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl TestStore {
        fn add(&self, mut e: MemoryEntry, embedding: Option<Vec<f32>>) {
            e.embedding = embedding;
            self.items
                .write()
                .unwrap()
                .insert((e.namespace.clone(), e.id), e);
        }

        fn list_filter_log(&self) -> Vec<Option<String>> {
            self.list_filters.lock().unwrap().clone()
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
            self.list_filters
                .lock()
                .unwrap()
                .push(filter.text_contains.clone());
            if let Some(delay) = self.list_delay {
                tokio::time::sleep(delay).await;
            }
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
                    // 与生产存储层同一套词法（分词部分匹配），避免测试桩与生产漂移
                    filter
                        .text_contains
                        .as_ref()
                        .is_none_or(|t| mem_core::fts_query_hits(&e.content, t) > 0)
                })
                .filter(|e| {
                    filter
                        .statuses
                        .as_ref()
                        .is_none_or(|statuses| statuses.contains(&e.status))
                })
                .filter(|e| {
                    filter
                        .tags
                        .as_ref()
                        .is_none_or(|tags| tags.iter().all(|t| e.tags.contains(t)))
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
            filter: &MemoryFilter,
            qv: &[f32],
            k: usize,
        ) -> Result<Vec<Scored<MemoryEntry>>> {
            let mut out: Vec<Scored<MemoryEntry>> = self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| {
                    filter
                        .namespace
                        .as_deref()
                        .is_none_or(|ns| e.namespace == ns)
                        && e.status == MemoryStatus::Active
                })
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
        item.embedding_model = Some("test-embedder".to_string());

        // 没有查询向量时（如嵌入服务故障），sim 维度不可用，按剩余信号归一化
        let without = r.score_items(vec![(item.clone(), None)], &query("ns", "东京"), None);
        // 带查询向量时同一命中应现算出 sim≈1.0，而不是被当作零相似度判死
        let with = r.score_items(vec![(item, None)], &query("ns", "东京"), Some(&qv));

        assert!(
            with[0].score > without[0].score,
            "关键词命中的相似度不应恒为 0：{:#?} vs {:#?}",
            with[0].score,
            without[0].score
        );
        // 缺嵌入时不应整体缩水到 0.5 上限：其余信号归一化后仍应保持高分
        assert!(
            without[0].score >= 0.85,
            "缺嵌入时按可用信号归一化，不应被 sim 权重整体压低：{:#?}",
            without[0].score
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

    /// 模拟崩溃的嵌入器（下游 503 / 网络故障）
    #[derive(Debug, Clone, Default)]
    struct FailingEmbedder;

    #[async_trait]
    impl Embedder for FailingEmbedder {
        fn model_id(&self) -> &str {
            "failing-embedder"
        }

        fn dim(&self) -> usize {
            64
        }

        async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Err(HubError::Internal(anyhow::anyhow!("模拟嵌入服务 503")))
        }
    }

    #[tokio::test]
    async fn test_historical_query_include_superseded_respects_valid_at() {
        // 回归：include_superseded=true 曾整段跳过 valid_at 约束，
        // 且失效惩罚按墙上时钟计算 —— 历史时点查询会选错版本
        let store = Arc::new(TestStore::default());
        let now = chrono::Utc::now();

        // v1：[-200d, -100d) 有效；v2：[-100d, +100d) 有效
        // 两条都带向量：故障出在向量路与打分路径不应用 valid_at 约束
        let mut v1 = entry("ns", "主节点是 redis-master-1（当时有效）");
        v1.valid_from = Some(now - chrono::Duration::days(200));
        v1.valid_until = Some(now - chrono::Duration::days(100));
        let mut v2 = entry("ns", "主节点是 redis-master-2（现行有效）");
        v2.valid_from = Some(now - chrono::Duration::days(100));
        v2.valid_until = Some(now + chrono::Duration::days(100));
        store.add(v1, Some(vec![1.0, 0.0]));
        store.add(v2, Some(vec![0.0, 1.0]));

        let r = retriever(store);
        let mut q = query("ns", "主节点");
        q.valid_at = Some(now - chrono::Duration::days(150)); // 查询 150 天前的事实
        q.include_superseded = true;
        let res = r.search(&auth(), &q).await.unwrap();

        assert_eq!(
            res.len(),
            1,
            "显式 valid_at 下必须按时间区间过滤（include_superseded 不绕过）：{:#?}",
            res.iter().map(|s| &s.item.content).collect::<Vec<_>>()
        );
        assert!(
            res[0].item.content.contains("redis-master-1"),
            "历史时点查询必须命中当时有效的事实"
        );
    }

    #[tokio::test]
    async fn test_injected_clock_drives_penalty_reference_time() {
        use chrono::TimeZone;
        let store = Arc::new(TestStore::default());
        let fixed_now = chrono::Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();

        // 在注入时钟的参考时点上：E1 仍未失效，E2 已失效
        let mut e1 = entry("ns", "旧事实仍在有效期内");
        e1.valid_until = Some(fixed_now + chrono::Duration::days(30));
        let mut e2 = entry("ns", "已过期事实");
        e2.valid_until = Some(fixed_now - chrono::Duration::days(30));
        store.add(e1, None);
        store.add(e2, None);

        let r =
            retriever(store).with_clock(Arc::new(mem_core::testing::FixedClock::new(fixed_now)));
        let mut q = query("ns", "事实");
        q.include_superseded = true;
        let res = r.search(&auth(), &q).await.unwrap();

        assert_eq!(res.len(), 2);
        assert!(
            res[0].item.content.contains("仍在有效期"),
            "惩罚必须按注入时钟的参考时点计算：{:#?}",
            res.iter()
                .map(|s| (&s.item.content, s.score))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_entry_tags_do_not_dilute_keyword_score() {
        // 回归：条目标签曾计入 kw 分母，标签越多命中分越低
        let store = Arc::new(TestStore::default());
        let mut tagged = entry("ns", "服务部署在东京区域");
        tagged.tags = vec![
            "运维".to_string(),
            "基础设施".to_string(),
            "东京".to_string(),
            "部署".to_string(),
        ];
        let untagged = entry("ns", "服务部署在东京区域");
        store.add(tagged, None);
        store.add(untagged, None);

        let r = retriever(store);
        let res = r.search(&auth(), &query("ns", "东京")).await.unwrap();
        assert_eq!(res.len(), 2);
        assert!(
            (res[0].score - res[1].score).abs() < 1e-6,
            "标签不应参与正文覆盖分母，两条同内容条目得分应一致：{:#?}",
            res.iter()
                .map(|s| (s.item.tags.len(), s.score))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_search_tags_filter_narrows_candidates() {
        // 回归：memory_search 的 tags 参数曾被解析后静默丢弃
        let store = Arc::new(TestStore::default());
        let mut db_item = entry("ns", "数据库主从切换流程");
        db_item.tags = vec!["db".to_string()];
        let mut other_item = entry("ns", "数据库慢查询排查");
        other_item.tags = vec!["ops".to_string()];
        store.add(db_item, None);
        store.add(other_item, None);

        let r = retriever(store);
        let mut q = query("ns", "数据库");
        q.tags = Some(vec!["db".to_string()]);
        let res = r.search(&auth(), &q).await.unwrap();

        assert_eq!(res.len(), 1, "tags 过滤必须生效");
        assert_eq!(res[0].item.tags, vec!["db".to_string()]);
    }

    #[tokio::test]
    async fn test_min_score_is_applied_before_mmr() {
        // 回归：min_score 曾在 MMR 之后过滤，结果数可能少于高阈值下本可返回的数量。
        // 构造：Z/Y 高相关且互相冗余，X 中等相关且与其余低相似。
        let store = Arc::new(TestStore::default());
        let mut z = entry("ns", "东京架构部署缓存网关方案");
        z.embedding = Some(vec![1.0, 0.0]);
        let mut y = entry("ns", "东京架构部署缓存网关方案");
        y.embedding = Some(vec![1.0, 0.0]);
        let mut x = entry("ns", "东京架构部署");
        x.embedding = Some(vec![0.0, 1.0]);
        store.add(z, Some(vec![1.0, 0.0]));
        store.add(y, Some(vec![1.0, 0.0]));
        store.add(x, Some(vec![0.0, 1.0]));

        // 仅以 kw 为信号：Z/Y=1.0，X=0.6
        let r = HybridRetriever::new(
            store,
            Arc::new(TestEmbedder::default()),
            20,
            20,
            [0.0, 1.0, 0.0, 0.0, 0.0],
            0.7,
            500,
        );
        let mut q = query("ns", "东京 架构 部署 缓存 网关");
        q.top_k = 2;
        q.min_score = Some(0.7);
        let res = r.search(&auth(), &q).await.unwrap();

        assert_eq!(
            res.len(),
            2,
            "阈值之上的候选数 ≥ top_k 时必须返回 top_k 条：{:#?}",
            res.iter()
                .map(|s| (&s.item.content, s.score))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_embed_failure_renormalizes_instead_of_shrinking_scores() {
        // 回归：查询嵌入失败后所有分数被 sim 权重整体压低，min_score 直接清空结果
        let store = Arc::new(TestStore::default());
        store.add(entry("ns", "东京部署方案"), None);

        let r = HybridRetriever::new(
            store,
            Arc::new(FailingEmbedder),
            20,
            20,
            [0.5, 0.2, 0.1, 0.1, 0.1],
            0.7,
            500,
        );
        let mut q = query("ns", "东京");
        q.min_score = Some(0.6);
        let res = r.search(&auth(), &q).await.unwrap();

        assert_eq!(
            res.len(),
            1,
            "嵌入故障时按可用信号归一化，高分命中不应被阈值误杀"
        );
    }

    #[tokio::test]
    async fn test_model_mismatched_embedding_not_used_for_similarity() {
        // 回归：embedding_model 从不参与检索 —— 换模型后新旧向量被直接比较
        let r = retriever(Arc::new(TestStore::default()));
        let embedder = TestEmbedder::default();
        let mut qv = embedder
            .embed(&["东京".to_string()])
            .await
            .unwrap()
            .remove(0);
        l2_normalize(&mut qv);

        let mut matching = entry("ns", "东京");
        matching.embedding = Some(qv.clone());
        matching.embedding_model = Some("test-embedder".to_string());
        matching.confidence = 0.5;

        let mut mismatched = entry("ns", "东京");
        mismatched.embedding = Some(qv.clone());
        mismatched.embedding_model = Some("other-embedder".to_string());
        mismatched.confidence = 0.5;

        let q = query("ns", "东京");
        let with_matching = r.score_items(vec![(matching, None)], &q, Some(&qv));
        let with_mismatched = r.score_items(vec![(mismatched, None)], &q, Some(&qv));

        assert!(
            with_matching[0].score > with_mismatched[0].score + 0.05,
            "跨模型向量不可比，模型不匹配时不得计入 sim 维度：{:#?} vs {:#?}",
            with_matching[0].score,
            with_mismatched[0].score
        );
    }

    #[tokio::test]
    async fn test_rare_term_outweighs_common_term_by_pool_idf() {
        // 回归：kw 曾是无差别覆盖率 —— 稀有词元与常见 bigram 等权。
        // 池内有 1 条命中"东京"、6 条命中"缓存"时，命中稀有词元者分更高。
        let store = Arc::new(TestStore::default());
        let mut rare = entry("ns", "东京节点上线");
        rare.embedding = Some(vec![1.0, 0.0]);
        store.add(rare, Some(vec![1.0, 0.0]));
        for i in 0..6 {
            let mut common = entry("ns", &format!("缓存策略说明 {}", i));
            common.embedding = Some(vec![0.0, 1.0]);
            store.add(common, Some(vec![0.0, 1.0]));
        }

        let r = HybridRetriever::new(
            store,
            Arc::new(TestEmbedder::default()),
            20,
            20,
            [0.0, 1.0, 0.0, 0.0, 0.0],
            0.7,
            500,
        );
        let res = r.search(&auth(), &query("ns", "东京 缓存")).await.unwrap();
        let rare_score = res
            .iter()
            .find(|s| s.item.content.contains("东京"))
            .map(|s| s.score)
            .expect("命中稀有词元的条目必须被召回");
        let common_score = res
            .iter()
            .find(|s| s.item.content.contains("缓存"))
            .map(|s| s.score)
            .expect("命中常见词元的条目必须被召回");
        assert!(
            rare_score > common_score,
            "稀有词元命中应比常见词元命中得分更高（IDF 加权）：{rare_score} vs {common_score}"
        );
    }

    #[tokio::test]
    async fn test_recency_uses_event_time_and_kind_specific_tau() {
        let store = Arc::new(TestStore::default());
        let now = chrono::Utc::now();

        // 同一天写入：一条事件发生在 60 天前，一条发生在今天
        let mut stale_event = entry("ns", "东京节点上线");
        stale_event.occurred_at = Some(now - chrono::Duration::days(60));
        let mut fresh_event = entry("ns", "缓存策略说明");
        fresh_event.occurred_at = Some(now);

        // 同为今天写入、事件时间一致：一条稳定偏好，一条临时笔记
        let mut preference = entry("ns", "节点扩容流程");
        preference.kind = MemoryKind::Preference;
        let mut note = entry("ns", "扩容脚本备注");
        note.kind = MemoryKind::Note;

        store.add(stale_event, None);
        store.add(fresh_event, None);
        store.add(preference, None);
        store.add(note, None);

        // 仅 recency 维度有权重
        let r = HybridRetriever::new(
            store,
            Arc::new(TestEmbedder::default()),
            20,
            20,
            [0.0, 0.0, 0.0, 1.0, 0.0],
            0.7,
            500,
        );

        let by_event = r.score_items(
            vec![
                {
                    let mut e = entry("ns", "事件时间久远");
                    e.occurred_at = Some(now - chrono::Duration::days(60));
                    (e, None)
                },
                {
                    let mut e = entry("ns", "事件时间新鲜");
                    e.occurred_at = Some(now);
                    (e, None)
                },
            ],
            &query("ns", "事件"),
            None,
        );
        assert!(
            by_event[0].item.content.contains("新鲜"),
            "近期性必须以事件时间（occurred_at）为准，而不是写入时间"
        );

        let by_kind = r.score_items(
            vec![
                {
                    let mut e = entry("ns", "稳定偏好条目");
                    e.kind = MemoryKind::Preference;
                    (e, None)
                },
                {
                    let mut e = entry("ns", "临时笔记条目");
                    e.kind = MemoryKind::Note;
                    (e, None)
                },
            ],
            &query("ns", "条目"),
            None,
        );
        assert!(
            by_kind[0].item.content.contains("稳定偏好"),
            "稳定型记忆（偏好的 τ=180 天）衰减应慢于短期型（笔记 τ=7 天）"
        );
    }

    #[tokio::test]
    async fn test_layer_prior_replaces_version_freshness() {
        let store = Arc::new(TestStore::default());
        // 仅 prior 维度有权重
        let r = HybridRetriever::new(
            store,
            Arc::new(TestEmbedder::default()),
            20,
            20,
            [0.0, 0.0, 0.0, 0.0, 1.0],
            0.7,
            500,
        );

        let by_layer = r.score_items(
            vec![
                {
                    let mut e = entry("ns", "实体画像条目");
                    e.layer = Some(mem_core::CognitiveLayer::Identity);
                    (e, None)
                },
                {
                    let mut e = entry("ns", "原始输入条目");
                    e.layer = Some(mem_core::CognitiveLayer::Raw);
                    (e, None)
                },
            ],
            &query("ns", "条目"),
            None,
        );
        assert!(
            by_layer[0].item.content.contains("实体画像"),
            "结构化知识应优先于原始观测（层级先验）"
        );

        // 回归：编辑次数多（version > 1）不再构成评分优势
        let by_version = r.score_items(
            vec![
                {
                    let mut e = entry("ns", "多次修订的条目");
                    e.version = 5;
                    (e, None)
                },
                {
                    let mut e = entry("ns", "一次写入的条目");
                    e.version = 1;
                    (e, None)
                },
            ],
            &query("ns", "条目"),
            None,
        );
        assert!(
            (by_version[0].score - by_version[1].score).abs() < 1e-6,
            "版本次数不应参与打分（旧的 fresh 信号已移除）：{:#?}",
            by_version
                .iter()
                .map(|s| (&s.item.content, s.score))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_scene_prefer_keeps_other_scenes_with_penalty() {
        let store = Arc::new(TestStore::default());
        let mut chat = entry("ns", "对话中记录的偏好");
        chat.scene = Some(MemoryScene::Chat);
        let mut coding = entry("ns", "对话中讨论的代码重构");
        coding.scene = Some(MemoryScene::Coding);
        let unscoped = entry("ns", "对话摘要未标注场景");
        store.add(chat, None);
        store.add(coding, None);
        store.add(unscoped, None);

        let mut q = query("ns", "对话");
        q.scene = Some(MemoryScene::Chat);

        // strict（默认）：硬过滤，只召回指定场景
        let strict = retriever(store.clone()).search(&auth(), &q).await.unwrap();
        assert_eq!(strict.len(), 1, "strict 模式保持既有硬过滤行为");
        assert_eq!(strict[0].item.scene, Some(MemoryScene::Chat));

        // prefer：其它场景降权保留、未标注场景中性
        let prefer = retriever(store.clone())
            .with_scene_mode(SceneMode::Prefer)
            .search(&auth(), &q)
            .await
            .unwrap();
        assert_eq!(
            prefer.len(),
            3,
            "prefer 模式必须保留其它场景与未标注场景：{prefer:#?}"
        );
        assert_eq!(
            prefer[0].item.scene,
            Some(MemoryScene::Chat),
            "同场景应排首位"
        );
        let coding_score = prefer
            .iter()
            .find(|s| s.item.scene == Some(MemoryScene::Coding))
            .unwrap()
            .score;
        let unscoped_score = prefer
            .iter()
            .find(|s| s.item.scene.is_none())
            .unwrap()
            .score;
        assert!(
            coding_score < unscoped_score,
            "其它场景应被降权到未标注（中性）场景之下：{coding_score} vs {unscoped_score}"
        );
    }

    #[tokio::test]
    async fn test_scene_weight_profile_overrides_global_weights() {
        let store = Arc::new(TestStore::default());
        // E1：词法精确命中（层级 Raw）；E2：无语义/词法命中但层级 Identity
        let mut lexical = entry("ns", "东京节点上线");
        lexical.scene = Some(MemoryScene::Chat);
        let mut prior = entry("ns", "缓存策略说明");
        prior.scene = Some(MemoryScene::Chat);
        prior.layer = Some(mem_core::CognitiveLayer::Identity);
        store.add(lexical, Some(vec![1.0, 0.0]));
        store.add(prior, Some(vec![0.0, 1.0]));

        let mut q = query("ns", "东京");
        q.scene = Some(MemoryScene::Chat);

        let build = |profiles: Option<std::collections::HashMap<MemoryScene, [f32; 5]>>| {
            let mut r = HybridRetriever::new(
                store.clone(),
                Arc::new(TestEmbedder::default()),
                20,
                20,
                [0.0, 0.0, 0.0, 0.0, 1.0],
                0.7,
                500,
            );
            if let Some(p) = profiles {
                r = r.with_scene_weight_profiles(p);
            }
            r
        };

        // 全局权重只奖励层级先验 → 先验条目在前
        let global = build(None).search(&auth(), &q).await.unwrap();
        assert!(
            global[0].item.content.contains("缓存"),
            "全局权重下层级先验应主导：{global:#?}"
        );

        // chat 场景 profile 只奖励关键词 → 词法命中条目在前
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(MemoryScene::Chat, [0.0, 1.0, 0.0, 0.0, 0.0]);
        let with_profile = build(Some(profiles)).search(&auth(), &q).await.unwrap();
        assert!(
            with_profile[0].item.content.contains("东京"),
            "分场景权重 profile 应改由关键词主导：{with_profile:#?}"
        );
    }

    #[tokio::test]
    async fn test_soft_timeout_keeps_completed_path_results() {
        // 回归：软超时曾整体丢弃双路结果并退回无查询的列表召回
        let slow_store = Arc::new(TestStore {
            list_delay: Some(Duration::from_millis(200)),
            ..TestStore::default()
        });
        let mut matched = entry("ns", "东京部署方案");
        matched.embedding = Some(vec![1.0, 0.0]);
        slow_store.add(matched, Some(vec![1.0, 0.0]));
        slow_store.add(entry("ns", "完全无关的另一条内容"), None);

        let r = HybridRetriever::new(
            slow_store.clone(),
            Arc::new(TestEmbedder::default()),
            20,
            20,
            [0.5, 0.2, 0.1, 0.1, 0.1],
            0.7,
            30, // 30ms 软超时 < 200ms 的 FTS 耗时
        );
        let res = r.search(&auth(), &query("ns", "东京")).await.unwrap();

        assert_eq!(res.len(), 1, "向量路已完成的结果必须保留：{:#?}", res);
        assert!(res[0].item.content.contains("东京"));
        let filters = slow_store.list_filter_log();
        assert!(
            !filters.is_empty() && filters.iter().all(|f| f.is_some()),
            "有已完成结果时不应退回无查询条件的列表召回：{:#?}",
            filters
        );
    }
}
