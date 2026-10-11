#![forbid(unsafe_code)]

//! 多智能体长期记忆服务嵌入模型适配器（embed）
//!
//! 提供：
//! - `MockEmbedder`：供开发、单元测试与离线环境使用，生成确定性伪向量并保持归一化
//! - `HttpEmbedder`：调用 OpenAI 兼容 `/embeddings` 远程服务（生产路径）
//! - `build_embedder`：按配置选择实现
//! - 契约测试规范

mod http;

pub use http::HttpEmbedder;

use async_trait::async_trait;
use common::Result;
use mem_core::Embedder;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// 确定性 Mock 嵌入模型
#[derive(Debug, Clone)]
pub struct MockEmbedder {
    model_name: String,
    dimension: usize,
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self::new("mock-embedder-v1", 1024)
    }
}

impl MockEmbedder {
    pub fn new(model_name: impl Into<String>, dimension: usize) -> Self {
        Self {
            model_name: model_name.into(),
            dimension,
        }
    }

    /// 将文本转为确定性且 L2 模长为 1.0 的向量
    fn generate_vector(&self, text: &str) -> Vec<f32> {
        let mut vec = Vec::with_capacity(self.dimension);
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash = hasher.finalize();

        // 基于哈希生成伪随机连续浮点数
        for i in 0..self.dimension {
            let byte = hash[i % 32];
            let val = ((byte as f32) / 255.0) * 2.0 - 1.0 + ((i as f32) * 0.001).sin();
            vec.push(val);
        }

        // L2 归一化
        let norm_sq: f32 = vec.iter().map(|x| x * x).sum();
        let norm = norm_sq.sqrt();
        if norm > 0.0 {
            for x in &mut vec {
                *x /= norm;
            }
        }

        vec
    }
}

#[async_trait]
impl Embedder for MockEmbedder {
    fn model_id(&self) -> &str {
        &self.model_name
    }

    fn dim(&self) -> usize {
        self.dimension
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.generate_vector(text));
        }
        Ok(results)
    }
}

/// 嵌入器提供方选择
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderKind {
    /// 确定性 Mock（开发/测试/无外部依赖）
    Mock,
    /// 远程 HTTP 嵌入服务
    Http,
}

impl EmbedderKind {
    /// 从配置字符串解析（`mock` / `http`）
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mock" => Some(Self::Mock),
            "http" => Some(Self::Http),
            _ => None,
        }
    }
}

/// 按配置构建嵌入器；未提供完整 HTTP 配置时回退 Mock 并打印告警
pub fn build_embedder(
    kind: EmbedderKind,
    model: &str,
    dim: usize,
    endpoint: Option<&str>,
    api_key: Option<&str>,
) -> Result<Arc<dyn Embedder>> {
    match kind {
        EmbedderKind::Mock => Ok(Arc::new(MockEmbedder::new(model, dim))),
        EmbedderKind::Http => match endpoint {
            Some(ep) if !ep.trim().is_empty() => {
                Ok(Arc::new(HttpEmbedder::new(ep, model, dim, api_key)?))
            }
            _ => {
                tracing::warn!(
                    "embedder=http 但未配置 HUB_EMBED_ENDPOINT，回退到 MockEmbedder（仅限开发环境）"
                );
                Ok(Arc::new(MockEmbedder::new(model, dim)))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_embedder_contract() {
        let embedder = MockEmbedder::new("test-embedder", 1024);
        assert_eq!(embedder.model_id(), "test-embedder");
        assert_eq!(embedder.dim(), 1024);

        let texts = vec![
            "偏好简短直接的表达".to_string(),
            "同事小王，平级关系".to_string(),
            "偏好简短直接的表达".to_string(), // 重复文本
        ];

        let vectors = embedder.embed(&texts).await.expect("Embed should succeed");
        assert_eq!(vectors.len(), 3);
        assert_eq!(vectors[0].len(), 1024);

        // 相同文本生成完全相同的确定性向量
        assert_eq!(vectors[0], vectors[2]);

        // 不同文本生成不同向量
        assert_ne!(vectors[0], vectors[1]);

        // L2 归一化检验 (norm 应当极其接近 1.0)
        for vec in vectors {
            let norm_sq: f32 = vec.iter().map(|x| x * x).sum();
            let norm = norm_sq.sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "Vector must be L2 normalized");
        }
    }

    #[test]
    fn test_embedder_kind_parse() {
        assert_eq!(EmbedderKind::parse("mock"), Some(EmbedderKind::Mock));
        assert_eq!(EmbedderKind::parse("HTTP"), Some(EmbedderKind::Http));
        assert_eq!(EmbedderKind::parse("bogus"), None);
    }

    #[tokio::test]
    async fn test_build_embedder_falls_back_to_mock_without_endpoint() {
        let e = build_embedder(EmbedderKind::Http, "bge-m3", 8, None, None).unwrap();
        assert_eq!(e.dim(), 8);
        assert_eq!(e.model_id(), "bge-m3");
    }

    #[tokio::test]
    async fn test_build_embedder_mock_path() {
        let e = build_embedder(EmbedderKind::Mock, "m", 4, None, None).unwrap();
        let v = e.embed(&["abc".to_string()]).await.unwrap();
        assert_eq!(v[0].len(), 4);
    }
}
