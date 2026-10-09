//! 多路混合检索与重排实现（基于设计规范 17.4 A 节）

use async_trait::async_trait;
use chrono::Utc;
use common::Result;
use hub_core::{
    calculate_item_score, mmr_rerank, ItemScoreParams, ItemStatus, MemoryItem, MemoryStore,
    RetrievalQuery, Retriever, Scored, Sensitivity, StoreQuery, UsePolicy,
};
use std::sync::Arc;

/// 混合检索引擎配置
#[derive(Debug, Clone)]
pub struct HybridRetrieverConfig {
    pub score_weights: [f32; 5], // sim, kw, conf, rec, prov
    pub mmr_lambda: f32,
    pub top_k: usize,
}

impl Default for HybridRetrieverConfig {
    fn default() -> Self {
        Self {
            score_weights: [0.40, 0.20, 0.15, 0.10, 0.15],
            mmr_lambda: 0.7,
            top_k: 20,
        }
    }
}

/// 双路检索（全文关键词 + 向量相似度）与多样性重排器
pub struct HybridRetriever {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn hub_core::Embedder>,
    config: HybridRetrieverConfig,
}

impl HybridRetriever {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        embedder: Arc<dyn hub_core::Embedder>,
        config: HybridRetrieverConfig,
    ) -> Self {
        Self {
            store,
            embedder,
            config,
        }
    }

    /// 计算两向量余弦相似度（假定已归一化，点积即为余弦值）
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        dot.clamp(0.0, 1.0)
    }

    /// 简单的关键词/标签匹配度计算 (0.0 ~ 1.0)
    fn keyword_score(text: &str, query: &str, scene_tags: &[String]) -> f32 {
        let mut hits = 0;
        let mut total = 1;

        if !query.is_empty() {
            total += 1;
            if text.contains(query) {
                hits += 1;
            }
        }

        for tag in scene_tags {
            total += 1;
            if text.contains(tag) {
                hits += 1;
            }
        }

        (hits as f32) / (total as f32)
    }
}

#[async_trait]
impl Retriever for HybridRetriever {
    async fn retrieve(&self, q: RetrievalQuery) -> Result<Vec<Scored<MemoryItem>>> {
        // 未成年人模式：禁止下发任何个人记忆
        if q.minor_mode {
            return Ok(Vec::new());
        }

        // 从存储层获取所有活跃条目
        let all_items = self
            .store
            .query(StoreQuery {
                user_id: q.user_id,
                status: Some(ItemStatus::Active),
                ..Default::default()
            })
            .await?;

        let now = Utc::now();

        // 过滤层：敏感度、过期时间、TTL、never_mention
        let candidates: Vec<MemoryItem> = all_items
            .into_iter()
            .filter(|item| {
                if item.sensitivity == Sensitivity::Blocked {
                    return false;
                }
                if !q.allow_sensitive && item.sensitivity == Sensitivity::Sensitive {
                    return false;
                }
                // never_mention 不进入主记忆包（由包装配器放入 do_not_mention）
                if item.use_policy == UsePolicy::NeverMention {
                    return false;
                }
                // 检查 TTL
                if let Some(ttl) = item.ttl {
                    if now.signed_duration_since(item.created_at) > ttl {
                        return false;
                    }
                }
                true
            })
            .collect();

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // 嵌入计算
        let query_vec = self
            .embedder
            .embed(std::slice::from_ref(&q.query_text))
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();

        let item_texts: Vec<String> = candidates.iter().map(|it| it.text.clone()).collect();
        let item_vectors = self.embedder.embed(&item_texts).await?;

        let mut relevance_scores = Vec::with_capacity(candidates.len());
        for (i, item) in candidates.iter().enumerate() {
            let sim = Self::cosine_similarity(&query_vec, &item_vectors[i]);
            let kw = Self::keyword_score(&item.text, &q.query_text, &q.scene_tags);

            // 近期性打分 rec
            let delta = now.signed_duration_since(item.updated_at);
            let delta_days = (delta.num_seconds() as f32 / 86400.0).max(0.0);
            let tau = hub_core::algorithm::decay::get_decay_tau_days(item.layer).unwrap_or(365.0);
            let rec = (-delta_days / tau).exp().clamp(0.0, 1.0);

            let score = calculate_item_score(
                &ItemScoreParams {
                    sim,
                    kw,
                    conf: item.confidence,
                    rec,
                    prov: item.provenance,
                },
                &self.config.score_weights,
            );
            relevance_scores.push(score);
        }

        // 构建条目两两相似度矩阵
        let n = candidates.len();
        let mut sim_matrix = vec![vec![0.0f32; n]; n];
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    sim_matrix[i][j] = 1.0;
                } else {
                    let s = Self::cosine_similarity(&item_vectors[i], &item_vectors[j]);
                    sim_matrix[i][j] = s;
                }
            }
        }

        // MMR 多样性重排
        let top_k = self.config.top_k.min(n);
        let ranked_indices = mmr_rerank(
            &relevance_scores,
            &sim_matrix,
            top_k,
            self.config.mmr_lambda,
        );

        let mut results = Vec::with_capacity(ranked_indices.len());
        for idx in ranked_indices {
            results.push(Scored {
                item: candidates[idx].clone(),
                score: relevance_scores[idx],
            });
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_core::{Layer, NewMemory, Provenance};
    use hub_embed::MockEmbedder;
    use hub_store::InMemoryMemoryStore;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_retriever_minor_mode_and_filtering() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let embedder = Arc::new(MockEmbedder::default());
        let retriever =
            HybridRetriever::new(store.clone(), embedder, HybridRetrieverConfig::default());

        let user_id = Uuid::new_v4();

        // 插入一条普通记忆，一条敏感记忆，一条被封禁记忆
        store
            .upsert(NewMemory {
                user_id,
                layer: Layer::Preference,
                text: "偏好简短直接".to_string(),
                provenance: Provenance::Stated,
                confidence: 0.9,
                sensitivity: Sensitivity::Normal,
                use_policy: UsePolicy::Free,
                source_client: "c1".to_string(),
                ttl_seconds: None,
                attrs: serde_json::json!({}),
            })
            .await
            .unwrap();

        store
            .upsert(NewMemory {
                user_id,
                layer: Layer::Fact,
                text: "个人敏感医疗偏好".to_string(),
                provenance: Provenance::Stated,
                confidence: 0.9,
                sensitivity: Sensitivity::Sensitive,
                use_policy: UsePolicy::Ask,
                source_client: "c1".to_string(),
                ttl_seconds: None,
                attrs: serde_json::json!({}),
            })
            .await
            .unwrap();

        // 1. 未成年人模式：结果必为空
        let q_minor = RetrievalQuery {
            user_id,
            query_text: "简短".to_string(),
            scene_tags: vec![],
            token_budget: 600,
            allow_sensitive: true,
            minor_mode: true,
        };
        let res_minor = retriever.retrieve(q_minor).await.unwrap();
        assert!(res_minor.is_empty());

        // 2. 正常模式，但不可用敏感层：只应返回第 1 条
        let q_normal = RetrievalQuery {
            user_id,
            query_text: "简短".to_string(),
            scene_tags: vec![],
            token_budget: 600,
            allow_sensitive: false,
            minor_mode: false,
        };
        let res_normal = retriever.retrieve(q_normal).await.unwrap();
        assert_eq!(res_normal.len(), 1);
        assert_eq!(res_normal[0].item.text, "偏好简短直接");
    }
}
