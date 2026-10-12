//! 创界山生态智能接入适配器 (CjsAdapter)
//!
//! 提供面向创界山客户端的防腐层中间件，实现：
//! - 客户端特征指纹探测 (FingerprintDetector)
//! - 虚拟多路召回与三级命名空间路由 (SmartNamespaceRouter)
//! - 数据启发式自萃取与 AIGC 合规水印注入 (AutoClassifier)
//! - 角色扮演主观视角防冲突策略 (RoleplayConsolidationPolicy)
//! - 创界山备份清单透明联动 (BackupManifestCoordinator)

pub mod backup;
pub mod classifier;
pub mod consolidation;
pub mod fingerprint;
pub mod router;

pub use fingerprint::{ChuangjieshanFingerprint, FingerprintDetector};

use crate::adapter::ToolInterceptor;
use crate::tools::ToolCallResult;
use crate::ToolDispatcher;
use common::{AuthCtx, Result};
use serde_json::Value;

/// 创界山接入层运行时配置
#[derive(Debug, Clone)]
pub struct CjsAdapterConfig {
    /// 是否启用适配器（默认 true，若为 false 则零开销完全穿透）
    pub enabled: bool,
    /// 默认世界标识（默认 "default"）
    pub default_world: String,
    /// 是否开启启发式 Kind 自动分类（默认 true）
    pub auto_classify: bool,
    /// 是否注入 AIGC 国家合规水印（默认 true）
    pub aigc_watermark: bool,
}

impl Default for CjsAdapterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_world: "default".to_string(),
            auto_classify: true,
            aigc_watermark: true,
        }
    }
}

impl CjsAdapterConfig {
    /// 从环境变量读取配置
    pub fn from_env() -> Self {
        let enabled = std::env::var("HUB_ENABLE_CJS_ADAPTER")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);

        let default_world = std::env::var("HUB_CJS_DEFAULT_WORLD")
            .unwrap_or_else(|_| "default".to_string());

        let auto_classify = std::env::var("HUB_CJS_AUTO_CLASSIFY")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);

        let aigc_watermark = std::env::var("HUB_CJS_AIGC_WATERMARK")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);

        Self {
            enabled,
            default_world,
            auto_classify,
            aigc_watermark,
        }
    }
}

/// 创界山适配层主拦截器
pub struct CjsAdapter {
    config: CjsAdapterConfig,
}

impl CjsAdapter {
    pub fn new(config: CjsAdapterConfig) -> Self {
        Self { config }
    }

    pub fn from_env() -> Self {
        Self::new(CjsAdapterConfig::from_env())
    }

    pub fn config(&self) -> &CjsAdapterConfig {
        &self.config
    }
}

#[async_trait::async_trait]
impl ToolInterceptor for CjsAdapter {
    async fn intercept_tool_call(
        &self,
        dispatcher: &ToolDispatcher,
        auth: &AuthCtx,
        headers: Option<&axum::http::HeaderMap>,
        name: &str,
        arguments: &mut Value,
    ) -> Result<Option<ToolCallResult>> {
        // 1. 全局开关检查：未启用则 1 纳秒级直接穿透核心
        if !self.config.enabled {
            return Ok(None);
        }

        // 2. 指纹探测：未命中创界山指纹则零开销放行
        let fp = FingerprintDetector::detect(headers, Some(arguments), &self.config.default_world);
        if !fp.is_chuangjieshan {
            return Ok(None);
        }

        // 3. 数据自萃取与合规水印 (AutoClassifier)
        classifier::AutoClassifier::process(arguments, &fp, &self.config);

        // 4. 角色扮演防冲突整合策略 (RoleplayConsolidationPolicy)
        if let Some(res) = consolidation::RoleplayConsolidationPolicy::intercept(name, arguments, &fp)? {
            return Ok(Some(res));
        }

        // 5. 备份清单元数据透明联动 (BackupManifestCoordinator)
        if let Some(res) = backup::BackupManifestCoordinator::intercept(dispatcher, auth, name, arguments, &fp).await? {
            return Ok(Some(res));
        }

        // 6. 智能命名空间路由与双路召回 (SmartNamespaceRouter)
        router::SmartNamespaceRouter::intercept_or_route(dispatcher, auth, name, arguments, &fp).await
    }
}
