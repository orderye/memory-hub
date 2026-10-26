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
use std::time::Duration;

/// 单次嵌入请求的超时。
///
/// 上游 AppConfig 没有 `HUB_EMBED_TIMEOUT_MS` 一类的超时配置项；为避免本次改动
/// 膨胀配置面，先提取为常量。注意：单次 `embed()` 的总耗时上限约为
/// `ATTEMPTS × 超时 + 退避和`（约 31s），上层把嵌入跑在写入路径上时需容许该量级。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 总尝试次数（首发 + 最多 2 次重试）
const ATTEMPTS: u32 = 3;

/// 重试退避基数（第 1/2 次重试分别以此为基数，±50% 抖动）
const RETRY_BACKOFF_BASE_MS: u64 = 200;

/// 错误响应体最多读取的字节数：只用于诊断日志与错误信息，
/// 无界读会把服务端异常时的大 body 整个拉进内存
const ERROR_BODY_LIMIT: usize = 4 * 1024;

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
            .timeout(REQUEST_TIMEOUT)
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

    /// 指数退避 + ±50% 抖动：第 1 次重试约 200ms、第 2 次约 400ms（各带抖动）。
    async fn backoff_sleep(attempt: u32) {
        let base = RETRY_BACKOFF_BASE_MS * (1 << (attempt - 1));
        let jitter = jitter_factor();
        tokio::time::sleep(Duration::from_millis((base as f32 * jitter) as u64)).await;
    }

    fn validate_response(
        &self,
        parsed: EmbeddingsResponse,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>> {
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

        let mut last_err: Option<HubError> = None;
        for attempt in 1..=ATTEMPTS {
            let mut req = self.client.post(&self.endpoint).json(&body);
            if let Some(ref key) = self.api_key {
                req = req.bearer_auth(key.expose_secret());
            }

            let resp = match req.send().await {
                Ok(r) => r,
                // 网络层错误（超时、连接重置、DNS 抖动）按可重试处理；
                // 响应成功后不再有网络类错误
                Err(e) => {
                    let err = HubError::Internal(anyhow::anyhow!("嵌入服务请求失败: {}", e));
                    if attempt < ATTEMPTS {
                        tracing::warn!(
                            attempt,
                            of = ATTEMPTS,
                            endpoint = %self.endpoint,
                            error = %e,
                            "嵌入服务请求失败，准备重试"
                        );
                        Self::backoff_sleep(attempt).await;
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let text = read_error_body_limited(resp).await;
                let err = HubError::Internal(anyhow::anyhow!(
                    "嵌入服务返回非成功状态 {}: {}",
                    status,
                    text
                ));
                // 5xx 是服务端瞬时故障，允许用满 2 次重试；429 只重试一次
                // （持续限流说明该客户端配额耗尽，多重试只会加剧排队）；
                // 其余 4xx 是请求本身的问题，重试无意义
                let max_attempts = if status.is_server_error() {
                    ATTEMPTS
                } else if status.as_u16() == 429 {
                    2
                } else {
                    1
                };
                if attempt < max_attempts {
                    tracing::warn!(
                        attempt,
                        of = max_attempts,
                        endpoint = %self.endpoint,
                        status = status.as_u16(),
                        "嵌入服务返回可重试状态，准备重试"
                    );
                    Self::backoff_sleep(attempt).await;
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }

            let parsed: EmbeddingsResponse = match resp.json().await {
                Ok(p) => p,
                Err(e) => {
                    return Err(HubError::Internal(anyhow::anyhow!(
                        "嵌入服务响应解析失败: {}",
                        e
                    )))
                }
            };

            return self.validate_response(parsed, texts);
        }

        Err(last_err.unwrap_or_else(|| HubError::Internal(anyhow::anyhow!("嵌入服务请求重试耗尽"))))
    }
}

/// 读取错误响应体，最多 [`ERROR_BODY_LIMIT`] 字节（按 UTF-8 有损解码）。
async fn read_error_body_limited(resp: reqwest::Response) -> String {
    match resp.bytes().await {
        Ok(bytes) => {
            if bytes.len() > ERROR_BODY_LIMIT {
                let cut = &bytes[..ERROR_BODY_LIMIT];
                let head = String::from_utf8_lossy(cut);
                format!("{}…(截断，共 {} 字节)", head, bytes.len())
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            }
        }
        Err(e) => format!("<错误响应体读取失败: {}>", e),
    }
}

/// 0.5..=1.5 的抖动系数，用 CSPRNG 低 32 位映射（workspace 已有 getrandom，不引入 rand）。
///
/// 只取低位再除以 2^32 归一：getrandom 输出均匀，低位同样均匀，
/// 模偏差在这种粗粒度抖动场景可忽略。
fn jitter_factor() -> f32 {
    let mut raw = [0u8; 4];
    getrandom::getrandom(&mut raw).expect("CSPRNG failure");
    let v = u32::from_ne_bytes(raw) as f32 / u32::MAX as f32;
    0.5 + v
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
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
    async fn test_retry_recovers_from_transient_500() {
        // 两个 mock 以相同 matcher 挂载：常驻 200 先挂载，有限次的 500 用更高
        // 优先级压过它（同优先级时 wiremock 取先挂载者）。500 mock 耗尽 2 次
        // 配额后不再匹配，落到 200 mock——验证瞬时 5xx 被重试吸收。
        // matcher 附带请求体，与同文件其他用例的通用 mock 区分开。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_json(serde_json::json!({
                "model": "m",
                "input": ["x"],
                "encoding_format": "float"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "index": 0, "embedding": [1.0, 0.0, 0.0] }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_json(serde_json::json!({
                "model": "m",
                "input": ["x"],
                "encoding_format": "float"
            })))
            .respond_with(ResponseTemplate::new(500).set_body_string("transient"))
            .up_to_n_times(2)
            .expect(1..)
            .with_priority(1)
            .mount(&server)
            .await;

        let embedder = HttpEmbedder::new(
            format!("{}/embeddings", server.uri()),
            "m",
            3,
            Option::<String>::None,
        )
        .unwrap();

        let vecs = embedder.embed(&["x".to_string()]).await.unwrap();
        assert_eq!(vecs, vec![vec![1.0, 0.0, 0.0]]);
        // 首发 1 次 + 重试 2 次
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_retry_exhausts_on_persistent_500() {
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

        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(
            format!("{}", err).contains("500"),
            "错误应带状态码: {}",
            err
        );
        // 3 次尝试后放弃，不无限重试
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_client_error_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let embedder = HttpEmbedder::new(
            format!("{}/embeddings", server.uri()),
            "m",
            3,
            Option::<String>::None,
        )
        .unwrap();

        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(format!("{}", err).contains("401"));
        // 4xx（除 429）请求本身有问题，只尝试 1 次
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_too_many_requests_retried_once_then_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let embedder = HttpEmbedder::new(
            format!("{}/embeddings", server.uri()),
            "m",
            3,
            Option::<String>::None,
        )
        .unwrap();

        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(format!("{}", err).contains("429"));
        // 429 可重试一次：共 2 次尝试
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
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
