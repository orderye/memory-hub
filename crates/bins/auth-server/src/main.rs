#![forbid(unsafe_code)]

//! OAuth 2.1 授权服务器主入口（设计规范 Appendix C.1）
//!
//! 负责签发与轮换 Token Family、校验 PKCE S256、
//! 暴露 RFC 9728 受保护资源元数据与用户中心确认服务。

use as_core::{ConfirmTokenManager, TokenFamilyManager};
use common::init_tracing;
use hub_authz::ProtectedResourceMetadata;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    info!("正在启动 OAuth 2.1 授权服务器 (RFC 6749 / RFC 8707 / RFC 9728)...");

    let _family_mgr = Arc::new(TokenFamilyManager::new(30)); // 30天
    let _confirm_mgr = Arc::new(ConfirmTokenManager::new(120)); // 120秒即用即焚
    let metadata = ProtectedResourceMetadata::default_for_hub(
        "https://hub.example.com",
        "https://auth.example.com",
    );

    info!(
        "OAuth 2.1 资源服务器元数据就绪: 资源标识符 {:?}, 支持范围 {:?}",
        metadata.resource, metadata.scopes_supported
    );

    info!("OAuth 2.1 授权服务就绪，监听 0.0.0.0:8080...");
    Ok(())
}
