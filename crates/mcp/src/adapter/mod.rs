//! MCP 适配层（拦截器与生态适配中间层）
//!
//! 定位：零侵入式防腐中间层（Anti-Corruption Layer Middleware）
//! 为第三方应用生态（如创界山）提供平滑接入，保持记忆中枢核心存储与协议纯净。

#[cfg(feature = "adapter-cjs")]
pub mod cjs;

/// MCP 工具调用通用拦截器 Trait
#[async_trait::async_trait]
pub trait ToolInterceptor: Send + Sync {
    /// 工具执行拦截
    /// - 返回 `Ok(Some(result))`: 表示拦截器已完全处理并生成响应（如虚拟双路召回合并）
    /// - 返回 `Ok(None)`: 表示放行并继续执行核心 `ToolDispatcher`（`arguments` 可能已被原地改写/丰富）
    /// - 返回 `Err(e)`: 发生严重错误，传输层将中断并返回对应错误
    async fn intercept_tool_call(
        &self,
        dispatcher: &crate::ToolDispatcher,
        auth: &common::AuthCtx,
        headers: Option<&axum::http::HeaderMap>,
        name: &str,
        arguments: &mut serde_json::Value,
    ) -> common::Result<Option<crate::ToolCallResult>>;
}
