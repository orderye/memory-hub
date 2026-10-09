#![forbid(unsafe_code)]

//! 记忆中枢嵌入模型适配器（hub-embed）
//!
//! 提供：
//! - `MockEmbedder`：供开发、单元测试与离线环境使用，生成确定性伪向量并保持归一化
//! - 契约测试规范

use async_trait::async_trait;
use common::Result;
use hub_core::Embedder;
use sha2::{Digest, Sha256};

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
}
