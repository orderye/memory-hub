//! 开发与测试用的确定性嵌入器
//!
//! 与 `embed::MockEmbedder` 相同思路（SHA-256 哈希 → 归一化伪向量），
//! 定义在 core 中以避免 core → embed 的循环依赖；生产环境请使用
//! `embed` crate 的 `HttpEmbedder`。

use crate::infra::Embedder;
use async_trait::async_trait;
use common::Result;
use sha2::{Digest, Sha256};

/// 确定性测试嵌入器
#[derive(Debug, Clone)]
pub struct TestEmbedder {
    dimension: usize,
}

impl Default for TestEmbedder {
    fn default() -> Self {
        Self { dimension: 64 }
    }
}

impl TestEmbedder {
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    fn generate_vector(&self, text: &str) -> Vec<f32> {
        let mut vec = Vec::with_capacity(self.dimension);
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash = hasher.finalize();

        for i in 0..self.dimension {
            let byte = hash[i % 32];
            let val = ((f32::from(byte)) / 255.0) * 2.0 - 1.0 + ((i as f32) * 0.001).sin();
            vec.push(val);
        }

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
impl Embedder for TestEmbedder {
    fn model_id(&self) -> &str {
        "test-embedder"
    }

    fn dim(&self) -> usize {
        self.dimension
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.generate_vector(t)).collect())
    }
}

use crate::domain::{ChatCognitiveInput, CognitiveCandidate};
use crate::infra::ChatCognitiveExtractor;
use std::sync::Mutex;

/// 测试用可编程/确定性 Chat 认知抽取器
#[derive(Default)]
pub struct MockChatCognitiveExtractor {
    pub candidates: Mutex<Vec<CognitiveCandidate>>,
    pub failure_mode: Mutex<Option<String>>,
}

impl MockChatCognitiveExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_candidates(self, candidates: Vec<CognitiveCandidate>) -> Self {
        *self.candidates.lock().unwrap() = candidates;
        self
    }

    pub fn with_failure(self, failure_mode: &str) -> Self {
        *self.failure_mode.lock().unwrap() = Some(failure_mode.to_string());
        self
    }
}

#[async_trait]
impl ChatCognitiveExtractor for MockChatCognitiveExtractor {
    async fn extract_candidates(
        &self,
        _input: &ChatCognitiveInput,
    ) -> Result<Vec<CognitiveCandidate>> {
        if let Some(mode) = self.failure_mode.lock().unwrap().as_deref() {
            match mode {
                "timeout" => {
                    return Err(common::HubError::Internal(anyhow::anyhow!(
                        "LLM request timed out"
                    )))
                }
                "invalid_json" => {
                    return Err(common::HubError::InvalidArgument(
                        "LLM response malformed JSON".into(),
                    ))
                }
                _ => {
                    return Err(common::HubError::Internal(anyhow::anyhow!(
                        "LLM provider failure"
                    )))
                }
            }
        }
        let list = self.candidates.lock().unwrap().clone();
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_deterministic_and_normalized() {
        let e = TestEmbedder::default();
        let v1 = e.embed(&["abc".to_string()]).await.unwrap();
        let v2 = e.embed(&["abc".to_string()]).await.unwrap();
        assert_eq!(v1, v2, "相同输入必须得到相同向量");
        assert_ne!(v1[0], e.embed(&["abd".to_string()]).await.unwrap()[0]);

        let norm: f32 = v1[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }
}
