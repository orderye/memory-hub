#![forbid(unsafe_code)]

//! 多智能体长期记忆服务 MCP 服务器主入口
//!
//! 装配链路：
//! 配置 → 脱敏日志 → 存储（PostgreSQL / 内存）→ 嵌入器 → 混合检索 →
//! 三大服务（存取 / 备份 / 整合）→ MCP 工具分发 → 传输层（HTTP + stdio）。

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use common::{init_tracing, AppConfig, AuthCtx, HubError};
use embed::HttpEmbedder;
use mcp::transport::{mcp_router, run_stdio, McpTransportState};
use mcp::ToolDispatcher;
use mem_core::testing::TestEmbedder;
use mem_core::{
    DefaultBackupService, DefaultIntegrationService, DefaultMemoryService, Embedder, GrantChecker,
    MemoryHubStore, TokenIntrospector,
};
use std::sync::Arc;
use store::{InMemoryHubStore, PostgresHubStore};
use tracing::info;

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
use tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = common::build_runtime()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    // 强制脱敏日志（LOG_REDACTION=always，不可关闭）
    init_tracing();

    let cfg = AppConfig::from_env()?;
    info!(config = %cfg.redacted_summary(), "记忆中枢 MCP Server 启动中");

    // ===== 存储 =====
    let store: Arc<dyn MemoryHubStore> = build_store(&cfg).await?;
    info!(mode = %store::storage_mode_label(cfg_is_pg(&cfg)), "存储已就绪");

    // ===== 嵌入器 =====
    let embedder: Arc<dyn Embedder> = build_embedder(&cfg)?;
    info!(provider = %cfg.embed_provider, dim = cfg.embed_dim, "嵌入器已就绪");

    // ===== 访问控制 =====
    let access: Arc<dyn mem_core::AccessChecker> = Arc::new(GrantChecker::new(store.clone()));

    // ===== 检索与服务 =====
    let retriever = Arc::new(retrieval::HybridRetriever::new(
        store.clone(),
        embedder.clone(),
        cfg.retrieval_fts_k,
        cfg.retrieval_vec_k,
        cfg.score_weights,
        cfg.mmr_lambda,
        cfg.soft_timeout_ms,
    ));
    let memory_service = Arc::new(
        DefaultMemoryService::new(
            store.clone(),
            embedder.clone(),
            access.clone(),
            cfg.score_weights,
        )
        .with_retriever(retriever),
    );
    let backup_service = Arc::new(DefaultBackupService::new(store.clone(), access.clone()));
    let integration_service = Arc::new(DefaultIntegrationService::new(
        store.clone(),
        access.clone(),
        cfg.integrate_sim_threshold,
        cfg.integrate_max_proposals,
    ));

    let dispatcher = Arc::new(ToolDispatcher::new(
        memory_service,
        backup_service,
        integration_service,
        store.clone(),
    ));

    // ===== 鉴权 =====
    let introspector: Arc<dyn TokenIntrospector> = Arc::new(BearerIntrospector::new(
        cfg.token_secret.clone(),
        cfg.auth_issuer.clone(),
        cfg.resource_uri.clone(),
    ));

    // ===== 限流（内存桶；多副本部署可注入 PostgreSQL 桶）=====
    let rate_limiter = Some(mcp::transport::RateLimiter::new(
        cfg.rate_limit_per_agent_rps,
    ));

    let state = McpTransportState {
        dispatcher,
        introspector,
        allowed_origins: cfg.allowed_origins.clone(),
        max_request_bytes: cfg.max_request_bytes,
        rate_limiter,
    };

    // ===== 启动方式 =====
    let mode = std::env::var("HUB_SERVE_MODE").unwrap_or_else(|_| "http".to_string());
    match mode.as_str() {
        "stdio" => {
            info!(bind = %cfg.mcp_bind, "以 stdio 模式运行（换行分隔 JSON-RPC）");
            // stdio 模式：不带 Bearer 的本地桥接（仅限回环地址）
            let local_auth = std::env::var("HUB_STDIO_LOCAL_AGENT")
                .unwrap_or_else(|_| "local-agent".to_string());
            let _ = local_auth;
            run_stdio(Arc::new(state)).await?;
        }
        _ => {
            let app = mcp_router(Arc::new(state));
            let listener = tokio::net::TcpListener::bind(cfg.mcp_bind)
                .await
                .map_err(|e| anyhow!("绑定 {} 失败: {}", cfg.mcp_bind, e))?;
            info!(bind = %cfg.mcp_bind, "MCP Server 监听中（Streamable HTTP）");
            axum::serve(listener, app)
                .await
                .map_err(|e| anyhow!("HTTP 服务退出: {}", e))?;
        }
    }

    Ok(())
}

/// 布尔辅助：是否配置了 PostgreSQL 连接串
fn cfg_is_pg(cfg: &AppConfig) -> bool {
    cfg.db_url.starts_with("postgres://") || cfg.db_url.starts_with("postgresql://")
}

/// 构建存储：PostgreSQL 优先，连接失败时以显式环境变量开关决定是否退回内存
async fn build_store(cfg: &AppConfig) -> anyhow::Result<Arc<dyn MemoryHubStore>> {
    if cfg_is_pg(cfg) {
        match PostgresHubStore::connect(&cfg.db_url, cfg.db_max_connections).await {
            Ok(pg) => {
                info!("PostgreSQL 存储连接成功（含迁移执行）");
                return Ok(Arc::new(pg));
            }
            Err(e) => {
                let allow_fallback = std::env::var("HUB_STORE_FALLBACK_MEMORY")
                    .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
                    .unwrap_or(false);
                if allow_fallback {
                    tracing::error!(
                        error = %e,
                        "PostgreSQL 连接失败，按 HUB_STORE_FALLBACK_MEMORY=true 退回内存存储（数据不持久！）"
                    );
                } else {
                    return Err(anyhow!(
                        "PostgreSQL 连接失败（可设 HUB_STORE_FALLBACK_MEMORY=true 退回内存）: {}",
                        e
                    ));
                }
            }
        }
    }
    info!("使用内存存储（开发/测试模式；重启即失忆）");
    Ok(Arc::new(InMemoryHubStore::new()))
}

/// 构建嵌入器：http（OpenAI 兼容 /embeddings）或 mock
fn build_embedder(cfg: &AppConfig) -> Result<Arc<dyn Embedder>> {
    match cfg.embed_provider.as_str() {
        "http" => {
            let endpoint = cfg
                .embed_endpoint
                .clone()
                .ok_or_else(|| HubError::InvalidArgument("缺少 HUB_EMBED_ENDPOINT".to_string()))?;
            let embedder = HttpEmbedder::new(
                endpoint,
                std::env::var("HUB_EMBED_MODEL").unwrap_or_else(|_| "bge-m3".to_string()),
                cfg.embed_dim,
                cfg.embed_api_key.clone(),
            )?;
            Ok(Arc::new(embedder))
        }
        _ => Ok(Arc::new(TestEmbedder::new(cfg.embed_dim))),
    }
}

/// Bearer 令牌 → AuthCtx 的内省器（离线验签，无需回调授权服务器）
struct BearerIntrospector {
    tokens: auth_core::AccessTokenManager,
    validator: authz::TokenValidator,
}

impl BearerIntrospector {
    fn new(
        secret: impl Into<String>,
        issuer: impl Into<String>,
        audience: impl Into<String>,
    ) -> Self {
        let issuer: String = issuer.into();
        let audience: String = audience.into();
        Self {
            tokens: auth_core::AccessTokenManager::new(
                secret,
                issuer.clone(),
                audience.clone(),
                3600,
            ),
            validator: authz::TokenValidator::new(audience, issuer),
        }
    }
}

#[async_trait]
impl TokenIntrospector for BearerIntrospector {
    async fn introspect(&self, token: &str) -> common::Result<AuthCtx> {
        let claims = self.tokens.verify(token)?;
        self.validator.validate_claims(&claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cfg_is_pg_detection() {
        let pg: AppConfig = AppConfig {
            db_url: "postgresql://u:p@localhost/db".to_string(),
            ..Default::default()
        };
        assert!(cfg_is_pg(&pg));

        let pg2: AppConfig = AppConfig {
            db_url: "postgres://u:p@localhost/db".to_string(),
            ..Default::default()
        };
        assert!(cfg_is_pg(&pg2));

        let other: AppConfig = AppConfig {
            db_url: "sqlite::memory:".to_string(),
            ..Default::default()
        };
        assert!(!cfg_is_pg(&other));
    }

    #[tokio::test]
    async fn test_introspector_roundtrip() {
        let introspector = BearerIntrospector::new(
            "test-secret",
            "https://auth.example.com",
            "https://hub.example.com",
        );
        let token = introspector
            .tokens
            .issue(
                &"agent-a".to_string(),
                vec![common::Scope::Read],
                vec![common::NamespaceGrant::read_only("ns")],
            )
            .unwrap();

        let ctx = introspector.introspect(&token).await.unwrap();
        assert_eq!(ctx.agent_id, "agent-a");
        assert!(ctx.can_read_namespace("ns"));

        // 伪造令牌必须被拒
        let forged = format!("{}.{}.{}", &token[..10], &token[11..15], &token[20..]);
        assert!(introspector.introspect(&forged).await.is_err());
    }

    #[tokio::test]
    async fn test_introspector_rejects_foreign_audience() {
        let introspector = BearerIntrospector::new(
            "test-secret",
            "https://auth.example.com",
            "https://hub.example.com",
        );
        // 用另一个受众签发的令牌
        let foreign = auth_core::AccessTokenManager::new(
            "test-secret",
            "https://auth.example.com",
            "https://other-resource.example.com",
            3600,
        );
        let token = foreign
            .issue(&"agent-a".to_string(), vec![common::Scope::Read], vec![])
            .unwrap();
        assert!(introspector.introspect(&token).await.is_err());
    }
}
