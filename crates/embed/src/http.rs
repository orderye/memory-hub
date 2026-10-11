//! 远程嵌入服务客户端（OpenAI 兼容 `/embeddings` 协议）
//!
//! 生产环境通过本客户端调用境内合规的中文嵌入模型（如 BGE 系列）；
//! 维度由配置 `HUB_EMBED_DIM` 固定，服务端返回维度不符时直接报错，
//! 避免把错维向量写入 pgvector 列。

use async_trait::async_trait;
use common::{HubError, Result};
use mem_core::Embedder;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

/// `/embeddings` 请求体
#[derive(Debug, Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding_format: Option<&'a str>,
}

/// `/embeddings` 响应体
#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
    #[allow(dead_code)]
    index: usize,
}

/// 远程嵌入器
#[derive(Debug, Clone)]
pub struct HttpEmbedder {
    endpoint: String,
    model: String,
    dimension: usize,
    api_key: Option<SecretString>,
    client: reqwest::Client,
}

impl HttpEmbedder {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        dimension: usize,
        api_key: Option<impl Into<String>>,
    ) -> Result<Self> {
        if dimension == 0 {
            return Err(HubError::InvalidArgument("嵌入维度必须大于 0".to_string()));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| HubError::Internal(anyhow::anyhow!("HTTP 客户端构建失败: {}", e)))?;

        Ok(Self {
            endpoint: endpoint.into(),
            model: model.into(),
            dimension,
            api_key: api_key.map(|k| SecretString::new(k.into().into())),
            client,
        })
    }

    /// 从环境变量构造（`HUB_EMBED_ENDPOINT` / `HUB_EMBED_MODEL` / `HUB_EMBED_API_KEY`）
    pub fn from_env() -> Result<Self> {
        let endpoint = std::env::var("HUB_EMBED_ENDPOINT").map_err(|_| {
            HubError::InvalidArgument("缺少 HUB_EMBED_ENDPOINT 环境变量".to_string())
        })?;
        let model = std::env::var("HUB_EMBED_MODEL").unwrap_or_else(|_| "bge-m3".to_string());
        let dimension = std::env::var("HUB_EMBED_DIM")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1024);
        let api_key = std::env::var("HUB_EMBED_API_KEY").ok();

        Self::new(endpoint, model, dimension, api_key)
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dimension
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = EmbeddingsRequest {
            model: &self.model,
            input: texts,
            encoding_format: Some("float"),
        };

        let mut req = self.client.post(&self.endpoint).json(&body);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key.expose_secret());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| HubError::Internal(anyhow::anyhow!("嵌入服务请求失败: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(HubError::Internal(anyhow::anyhow!(
                "嵌入服务返回非成功状态 {}: {}",
                status,
                text
            )));
        }

        let parsed: EmbeddingsResponse = resp
            .json()
            .await
            .map_err(|e| HubError::Internal(anyhow::anyhow!("嵌入服务响应解析失败: {}", e)))?;

        if parsed.data.len() != texts.len() {
            return Err(HubError::Internal(anyhow::anyhow!(
                "嵌入服务返回条数不匹配: 期望 {}, 实际 {}",
                texts.len(),
                parsed.data.len()
            )));
        }

        let mut out = Vec::with_capacity(parsed.data.len());
        for d in parsed.data {
            if d.embedding.len() != self.dimension {
                return Err(HubError::Internal(anyhow::anyhow!(
                    "嵌入服务返回维度不匹配: 期望 {}, 实际 {}",
                    self.dimension,
                    d.embedding.len()
                )));
            }
            out.push(d.embedding);
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_http_embedder_success_and_normalization_contract() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "index": 0, "embedding": [3.0, 4.0, 0.0] },
                    { "index": 1, "embedding": [0.0, 1.0, 0.0] }
                ]
            })))
            .mount(&server)
            .await;

        let embedder = HttpEmbedder::new(
            format!("{}/embeddings", server.uri()),
            "bge-m3",
            3,
            Option::<String>::None,
        )
        .unwrap();

        let vecs = embedder
            .embed(&["短文本".to_string(), "长一些的文本内容".to_string()])
            .await
            .unwrap();

        assert_eq!(vecs.len(), 2);
        assert_eq!(embedder.dim(), 3);
        assert_eq!(embedder.model_id(), "bge-m3");
        assert_eq!(vecs[0], vec![3.0, 4.0, 0.0]);
    }

    #[tokio::test]
    async fn test_http_embedder_dimension_mismatch_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "index": 0, "embedding": [1.0, 2.0] }]
            })))
            .mount(&server)
            .await;

        let embedder = HttpEmbedder::new(
            format!("{}/embeddings", server.uri()),
            "m",
            5,
            Option::<String>::None,
        )
        .unwrap();

        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(format!("{}", err).contains("维度不匹配"));
    }

    #[tokio::test]
    async fn test_http_embedder_count_mismatch_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "index": 0, "embedding": [1.0, 0.0, 0.0] }]
            })))
            .mount(&server)
            .await;

        let embedder = HttpEmbedder::new(
            format!("{}/embeddings", server.uri()),
            "m",
            3,
            Option::<String>::None,
        )
        .unwrap();

        let err = embedder
            .embed(&["a".to_string(), "b".to_string()])
            .await
            .unwrap_err();
        assert!(format!("{}", err).contains("条数不匹配"));
    }

    #[tokio::test]
    async fn test_http_embedder_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let embedder = HttpEmbedder::new(
            format!("{}/embeddings", server.uri()),
            "m",
            3,
            Option::<String>::None,
        )
        .unwrap();

        assert!(embedder.embed(&["x".to_string()]).await.is_err());
    }

    #[tokio::test]
    async fn test_empty_input_short_circuits() {
        let embedder = HttpEmbedder::new(
            "http://127.0.0.1:1/embeddings",
            "m",
            3,
            Option::<String>::None,
        )
        .unwrap();
        // 空输入不应发起任何网络请求
        assert!(embedder.embed(&[]).await.unwrap().is_empty());
    }
}
