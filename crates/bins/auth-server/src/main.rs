#![forbid(unsafe_code)]

//! 多智能体长期记忆服务授权服务器主入口
//!
//! 职责：
//! - 加载配置并执行迁移（与 MCP Server 共享同一份 PostgreSQL）；
//! - 装配 `AccountService`（注册/登录/会话）并确保引导账户存在；
//! - 装配 `AgentOAuthServer`（DCR / client_credentials / 授权码+PKCE / 刷新轮换）；
//! - 启动 axum 路由：元数据、账户页面、注册、授权、令牌、内省、撤销、账户控制台。

use anyhow::{anyhow, Result};
use auth_web::oauth_router;
use auth_web::{ensure_bootstrap_account, AccountService, AgentOAuthServer};
use common::init_tracing;
use mem_core::{AccountStore, MemoryHubStore};
use std::sync::Arc;
use tracing::info;

/// 同一底层实现的两个视角：`MemoryHubStore`（命名空间/Agent/授权）与
/// `AccountStore`（账户/会话）指向同一个存储对象
struct StoreHandle {
    memory: Arc<dyn MemoryHubStore>,
    accounts: Arc<dyn AccountStore>,
}

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
    init_tracing();

    let cfg = common::AppConfig::from_env()?;
    info!(config = %cfg.redacted_summary(), "记忆中枢 Auth Server 启动中");

    // 授权服务器与 MCP Server 共享存储（同一 HUB_DB_URL）
    let handle: StoreHandle = build_store(&cfg).await?;

    // 引导账户：存量数据归属；设置口令后控制台可登录管理
    let bootstrap = ensure_bootstrap_account(
        handle.accounts.as_ref(),
        &cfg.bootstrap_username,
        cfg.bootstrap_password.as_deref(),
    )
    .await
    .map_err(|e| anyhow!("引导账户初始化失败: {}", e))?;
    info!(
        username = %bootstrap.username,
        has_password = bootstrap.has_password,
        "引导账户已就绪"
    );
    // 账户服务：注册/登录/会话 + 注册令牌（注册令牌 aud = 本服务器 register 端点）
    let external_base = std::env::var("AUTH_EXTERNAL_BASE_URL")
        .unwrap_or_else(|_| format!("http://{}", cfg.auth_bind));
    let register_audience = format!("{}/oauth/register", external_base.trim_end_matches('/'));
    let accounts = Arc::new(AccountService::new(
        handle.accounts.clone(),
        cfg.invite_code.clone(),
        cfg.auth_session_ttl_h,
        cfg.token_secret.clone(),
        cfg.auth_issuer.clone(),
        register_audience,
    ));
    // 清理过期会话（启动维护；运行期会话读取时也会惰性清理）
    let pruned = accounts.prune_sessions().await?;
    info!(pruned = pruned, "过期会话清理完成");

    let oauth = Arc::new(AgentOAuthServer::new(
        handle.memory.clone(),
        cfg.token_secret.clone(),
        cfg.auth_issuer.clone(),
        cfg.resource_uri.clone(),
        cfg.access_token_ttl_s,
        cfg.confirm_token_ttl_s,
        30, // 刷新令牌有效期（天）
    ));

    // 自检：确认令牌管理器可正常签发/校验
    let probe = oauth
        .token_manager()
        .issue(
            &"__selfcheck__".to_string(),
            common::BOOTSTRAP_ACCOUNT_ID,
            vec![common::Scope::Read],
            vec![],
        )
        .map_err(|e| anyhow!("令牌管理器自检失败: {}", e))?;
    oauth
        .token_manager()
        .verify(&probe)
        .map_err(|e| anyhow!("令牌管理器自检（校验阶段）失败: {}", e))?;
    info!("令牌管理器自检通过");

    let state = auth_web::AsWebState {
        oauth,
        accounts,
        external_base_url: external_base,
        allowed_origins: cfg.allowed_origins.clone(),
    };

    let app = oauth_router(state);
    let listener = tokio::net::TcpListener::bind(cfg.auth_bind)
        .await
        .map_err(|e| anyhow!("绑定 {} 失败: {}", cfg.auth_bind, e))?;
    info!(bind = %cfg.auth_bind, "Auth Server 监听中");
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow!("HTTP 服务退出: {}", e))?;

    Ok(())
}

/// 构建存储：PostgreSQL 优先；开发模式可退回内存
async fn build_store(cfg: &common::AppConfig) -> Result<StoreHandle> {
    let is_pg = cfg.db_url.starts_with("postgres://") || cfg.db_url.starts_with("postgresql://");
    if is_pg {
        match store::PostgresHubStore::connect(&cfg.db_url, cfg.db_max_connections).await {
            Ok(pg) => {
                info!("PostgreSQL 存储连接成功（含迁移执行）");
                let pg = Arc::new(pg);
                return Ok(StoreHandle {
                    memory: pg.clone(),
                    accounts: pg,
                });
            }
            Err(e) => {
                let allow_fallback = std::env::var("HUB_STORE_FALLBACK_MEMORY")
                    .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
                    .unwrap_or(false);
                if allow_fallback {
                    tracing::error!(
                        error = %e,
                        "PostgreSQL 连接失败，按 HUB_STORE_FALLBACK_MEMORY=true 退回内存存储（Agent 数据不持久！）"
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
    info!("使用内存存储（开发/测试模式）");
    let mem = Arc::new(store::InMemoryHubStore::new());
    Ok(StoreHandle {
        memory: mem.clone(),
        accounts: mem,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_selfcheck_token_shape() {
        // 签发 → 校验的往返形状（三段 HS256）
        let manager = auth_core::AccessTokenManager::new(
            "test-secret",
            "https://auth.example.com",
            "https://hub.example.com",
            900,
        );
        let token = manager
            .issue(
                &"agent-a".to_string(),
                common::BOOTSTRAP_ACCOUNT_ID,
                vec![common::Scope::Read],
                vec![],
            )
            .unwrap();
        assert_eq!(token.split('.').count(), 3);
        assert!(manager.verify(&token).is_ok());
    }
}
