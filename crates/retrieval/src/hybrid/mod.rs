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
use std::collections::HashMap;
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
    /// 嵌入器是否提供真实语义信号（伪向量嵌入器为 false）
    semantic_ready: bool,
    /// 嵌入能力是否启用（`HUB_EMBED_PROVIDER=off` 时为 false：不嵌入、不走向量路）
    embedding_enabled: bool,
    /// 默认最小可信分（None = 不启用默认拒绝阈值）
    default_min_score: Option<f32>,
    /// 分场景最小可信分覆盖（缺省回落到全局默认）
    scene_min_scores: std::collections::HashMap<MemoryScene, f32>,
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
        // 伪向量嵌入器的余弦相似度是噪声：此时本检索器退化为**词法检索**，
        // 向量维度不参与打分（见 [`Self::score_items`]）。
        let semantic_ready = embedder.provides_semantic_signal();
        // 显式关闭嵌入时，查询向量与向量召回都不该发生：这是配置选择，不是降级。
        let embedding_enabled = embedder.is_enabled();
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
            semantic_ready,
            embedding_enabled,
            default_min_score: None,
            scene_min_scores: std::collections::HashMap::new(),
        }
    }

    /// 本实例是否具备真实语义检索能力
    pub fn semantic_ready(&self) -> bool {
        self.semantic_ready
    }

    /// 本次实例是否启用了向量路（嵌入被显式关闭时为 false）
    ///
    /// 供 `/capabilities`、`/readyz` 与 `memory_search` 的能力声明使用：
    /// 调用方必须能区分"关闭"与"向量服务故障降级"。
    pub fn vector_path_enabled(&self) -> bool {
        self.embedding_enabled
    }

    /// 设置默认最小可信分（builder）
    ///
    /// 请求未显式给出 `min_score` 时生效：低于该分的候选一律丢弃，
    /// 全部被丢弃时对外返回"无可靠匹配"，而不是强行给出低分结果。
    pub fn with_default_min_score(mut self, min_score: Option<f32>) -> Self {
        self.default_min_score = min_score.map(|v| v.clamp(0.0, 1.0));
        self
    }

    /// 设置分场景最小可信分（builder）：场景专属阈值优先于全局默认
    pub fn with_scene_min_scores(
        mut self,
        scores: std::collections::HashMap<MemoryScene, f32>,
    ) -> Self {
        self.scene_min_scores = scores
            .into_iter()
            .map(|(k, v)| (k, v.clamp(0.0, 1.0)))
            .collect();
        self
    }

    /// 本次请求生效的最小可信分
    ///
    /// 优先级：请求显式 `min_score` > 场景专属阈值 > 全局默认阈值。
    ///
    /// **时间锚定查询（显式 `valid_at`）不叠加服务端阈值**（默认阈值与场景阈值都不叠加）：
    /// 这类查询要的是"某个历史时点的事实"，它天然词法更弱、也天然更旧，
    /// 用按当下相关性标定的阈值去卡它，等于把正确答案挡在门外。
    /// 调用方仍可显式传 `min_score` 自行把关。
    fn effective_min_score(&self, q: &SearchQuery) -> Option<f32> {
        if let Some(m) = q.min_score {
            return Some(m);
        }
        if q.valid_at.is_some() {
            return None;
        }
        if let Some(scene) = q.scene {
            if let Some(m) = self.scene_min_scores.get(&scene) {
                return Some(*m);
            }
        }
        self.default_min_score
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
        // 构建 term -> 索引的 HashMap，避免 idf 闭包内 O(n) 线性查找
        let term_index: HashMap<&str, usize> = terms
            .iter()
            .enumerate()
            .map(|(i, t)| (t.as_str(), i))
            .collect();
        let idf = |term: &str| -> f32 {
            term_index
                .get(term)
                .map(|&i| inverse_document_frequency(pool_size, doc_freq[i]))
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
                // 近期性以**参考时点**为准：查询钉住了 valid_at（时间旅行）时，
                // "距今多久"没有意义——问的是"那个时点的最新事实"。
                // 此前一律按墙上时钟算，把历史事实的分数压到阈值之下，
                // 导致时间锚定查询反而召不回它本该返回的历史版本。
                let rec = recency_score(
                    reference_time,
                    item.occurred_at.unwrap_or(item.updated_at),
                    tau,
                );
                let prior = layer_prior(item.layer);
                // 嵌入不可用（服务故障 / 缺失向量 / 模型不匹配）时按剩余
                // 信号归一化，而不是让该维度按 0 计使分数整体缩水。
                //
                // 伪向量嵌入器（Mock / Test）同样按"不可用"处理：它的余弦相似度
                // 是哈希噪声而非语义，若按 0.50 的权重参与打分，等于把噪声注入排序
                // 并让"最小可信分"阈值失去意义。此处**不改动配置权重**——
                // 换成真实嵌入模型后，sim 维度自动恢复参与（权重仍是配置值）。
                let sim_available = self.semantic_ready && sim.is_some();
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

    /// 候选集中是否存在可用于相似度计算的向量
    ///
    /// 用于决定是否值得跑 MMR：全部不可用时相似度矩阵恒为 0，
    /// MMR 退化为按分排序，跑一遍纯属浪费。
    fn has_usable_embedding(&self, scored: &[Scored<MemoryEntry>]) -> bool {
        scored.iter().any(|s| self.embedding_usable(&s.item))
    }

    /// MMR 重排（嵌入缺失或模型不匹配的项相互相似度按 0 处理）
    ///
    /// 相关性候选先截断到 `k * MMR_POOL_FACTOR` 再做两两相似度矩阵：
    /// MMR 的 O(pool²·dim) 相似度计算在大候选池（默认 200）下会成为检索的主要
    /// 延迟来源；被截断的尾部条目在相关性上已被压制，实践中不会被 MMR 选中
    /// （与 Lucene 等实现一致的有界 MMR 做法）。
    fn mmr_rerank(
        &self,
        mut scored: Vec<Scored<MemoryEntry>>,
        k: usize,
    ) -> Vec<Scored<MemoryEntry>> {
        const MMR_POOL_FACTOR: usize = 4;
        let pool = k.saturating_mul(MMR_POOL_FACTOR).max(k);
        if scored.len() > pool {
            scored.truncate(pool);
        }
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
            // 空查询退化为列表召回，没有相关性信号：只认调用方显式给出的阈值，
            // 默认拒绝阈值不适用（否则会把"列出全部"的语义误判为"不可靠"）
            if let Some(min_s) = q.min_score {
                scored.retain(|s| s.score >= min_s);
            }
            return Ok(scored.into_iter().take(top_k).collect());
        }

        // 本次请求生效的最小可信分（显式 > 场景 > 全局默认）
        let min_score = self.effective_min_score(q);

        // 软超时预算统一起算：查询嵌入与双路召回共享同一 deadline，
        // 避免嵌入服务故障把整体耗时拖到 HTTP 客户端超时（10s）
        let deadline = tokio::time::Instant::now() + self.soft_timeout;

        // 嵌入被显式关闭（个人轻量部署默认）：不产生查询向量、不走向量召回。
        // 这是**配置选择**而非降级：不告警、不计入失败，纯粹省下一次外部/CPU 调用。
        let vector_enabled = self.embedding_enabled;

        // 查询向量只算一次，双路召回与打分共用。
        let query_vec: Option<Vec<f32>> = if !vector_enabled {
            None
        } else {
            match tokio::time::timeout_at(
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
            }
        };

        // 双路并行且各自受剩余预算约束：超时只丢弃超时的一路，已完成的一路保留。
        // 向量路关闭时不发起向量召回，也不等待它。
        let (fts_res, vec_res) = if vector_enabled {
            tokio::join!(
                tokio::time::timeout_at(deadline, self.fts_candidates(q)),
                tokio::time::timeout_at(deadline, self.vec_candidates(q, query_vec.as_deref())),
            )
        } else {
            (
                tokio::time::timeout_at(deadline, self.fts_candidates(q)).await,
                Ok(Vec::new()),
            )
        };
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
            if let Some(min_s) = min_score {
                scored.retain(|s| s.score >= min_s);
            }
            // MMR 的意义是"相关性 + 多样性"。候选集里没有任何可用向量时，
            // 两两相似度矩阵恒为 0，MMR 的选择顺序与"按分排序后截断"逐条相同，
            // 直接截断即可省掉 O(pool²·dim) 的相似度矩阵（结果不变）。
            if this.has_usable_embedding(&scored) {
                this.mmr_rerank(scored, top_k)
            } else {
                scored.into_iter().take(top_k).collect()
            }
        })
        .await
        .map_err(|e| common::HubError::Internal(anyhow::anyhow!("检索重排任务调度失败: {}", e)))?;

        Ok(reranked)
    }
}

#[cfg(test)]
mod tests;
