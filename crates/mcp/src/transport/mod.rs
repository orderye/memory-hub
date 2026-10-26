//! MCP 传输层（Streamable HTTP + stdio）与 JSON-RPC 2.0 协议帧
//!
//! 设计要点：
//! - `handle_jsonrpc` 是**纯函数**：不持有任何 IO，可直接单测；
//! - `mcp_router` 产出 axum 路由，供二进制进程挂载；
//! - `run_stdio` 走换行分隔的 JSON-RPC 帧，供本地桥接客户端使用。
//! - 鉴权在协议层完成：Bearer 令牌经 `TokenIntrospector` 换取 `AuthCtx`，
//!   随后按工具的 `required_scope` 做范围强制校验（设计规范 5.9 节）。

use crate::protocol::{handle_discover, handle_tools_list};
use crate::schema::{get_tool_definitions, TOOL_SCOPE_MAP};
use crate::tools::{McpContentItem, ToolCallResult, ToolDispatcher};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use common::{AuthCtx, HubError, Result, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// MCP 标准协议版本（设计规范 5.2 节）
pub const PROTOCOL_VERSION_PRIMARY: &str = "2026-07-28";
pub const PROTOCOL_VERSION_FALLBACK: &str = "2025-11-25";

/// JSON-RPC 2.0 标准错误码
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// JSON-RPC 2.0 请求帧
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 响应帧
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 错误对象
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// `HubError` → JSON-RPC 错误码映射（保留结构化 `data`）
pub fn hub_error_to_jsonrpc(err: &HubError) -> (i64, String, Value) {
    let detail = err.to_error_detail();
    let data = serde_json::to_value(&detail).unwrap_or(Value::Null);
    let code = match err {
        HubError::InvalidArgument(_) => INVALID_PARAMS,
        HubError::NotFound | HubError::MethodNotFound(_) => METHOD_NOT_FOUND,
        // 32001：凭据缺失或无效；32002：权限不足（需 step-up 授权）
        HubError::AuthRequired => 32001,
        HubError::InsufficientScope => 32002,
        HubError::ConfirmRequired { .. } | HubError::ConfirmExpired => 32002,
        HubError::RateLimited { .. } | HubError::Unavailable => 32003,
        HubError::Conflict => 32005,
        HubError::Internal(_) => INTERNAL_ERROR,
    };
    (code, detail.message_zh, data)
}

/// 按工具名查找其声明的作用域
/// 缓存的工具名 → 作用域查找表（OnceLock 初始化一次）
pub fn required_scope_for_tool(tool: &str) -> Option<Scope> {
    TOOL_SCOPE_MAP
        .get_or_init(|| {
            let mut m = HashMap::new();
            for d in get_tool_definitions() {
                let scope = match d.required_scope {
                    "memory:read" => Scope::Read,
                    "memory:write" => Scope::Write,
                    "memory:admin" => Scope::Admin,
                    _ => continue,
                };
                m.insert(d.name, scope);
            }
            m
        })
        .get(tool)
        .cloned()
}

/// 从 Authorization 头提取 Bearer 令牌
pub fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let mut parts = raw.splitn(2, ' ');
    let scheme = parts.next()?.trim();
    let token = parts.next()?.trim();
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token.to_string())
    } else {
        None
    }
}

/// 传输层共享状态
pub struct McpTransportState {
    pub dispatcher: Arc<ToolDispatcher>,
    pub introspector: Arc<dyn mem_core::TokenIntrospector>,
    /// 允许的 Origin（为空表示不校验本地桥接）
    pub allowed_origins: Vec<String>,
    /// 单请求体上限（字节）
    pub max_request_bytes: usize,
    /// 限流器（None 表示不限流）
    pub rate_limiter: Option<Arc<RateLimiter>>,
    /// 工具拦截器（None 表示不拦截直接穿透）
    pub interceptor: Option<Arc<dyn crate::ToolInterceptor>>,
    /// 运行能力报告（语义能力是否就绪等）
    pub capabilities: Arc<CapabilityReport>,
}

/// 运行能力报告
///
/// 语义检索能力**必须**在健康检查里显式暴露：伪向量嵌入器（mock）下
/// 检索退化为词法匹配，把它与"真实语义检索"混为一谈会让验收结论失真。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityReport {
    /// 部署模式：`production` | `lite`
    pub deployment_profile: String,
    /// 存储是否持久化（`false` = 内存临时模式，重启即失忆）
    pub storage_durable: bool,
    /// 向量路是否启用（嵌入被显式关闭时为 `false`）
    pub vector_path_enabled: bool,
    /// 嵌入提供方：`mock` | `http` | `off`
    pub embed_provider: String,
    /// 嵌入模型标识
    pub embed_model: String,
    /// 嵌入维度
    pub embed_dim: usize,
    /// 语义检索能力是否就绪（仅真实嵌入服务为 true）
    pub semantic_ready: bool,
    /// 语义未就绪的原因（就绪时为空）
    pub semantic_reason: Option<String>,
    /// 默认最小可信分（0 表示未启用）
    pub default_min_score: f32,
    /// 分场景最小可信分
    pub scene_min_scores: BTreeMap<String, f32>,
    /// 记忆整合能力（与检索语义能力是两回事）
    pub consolidation: ConsolidationCapability,
}

/// 记忆整合能力声明
///
/// **与检索的 `semantic.ready` 明确区分**：整合可以在纯规则模式下完全正常工作，
/// 不依赖向量服务；反过来，检索具备语义能力也不代表整合已启用向量模式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationCapability {
    /// 部署级候选发现模式（`rules` | `hybrid`）
    pub mode: String,
    /// 向量模式不可用时的降级策略（`rules` | `error`）
    pub fallback: String,
    /// 是否允许逐请求覆盖整合模式
    pub allow_mode_override: bool,
    /// 本实例是否具备运行向量整合的能力（真实语义嵌入器已就绪）
    pub vector_ready: bool,
    /// 向量候选的相似度阈值是否经过真实模型标定
    pub threshold_calibrated: bool,
}

impl Default for ConsolidationCapability {
    fn default() -> Self {
        Self {
            mode: "rules".to_string(),
            fallback: "rules".to_string(),
            allow_mode_override: false,
            vector_ready: false,
            threshold_calibrated: false,
        }
    }
}

impl Default for CapabilityReport {
    fn default() -> Self {
        Self {
            deployment_profile: "production".to_string(),
            storage_durable: true,
            vector_path_enabled: true,
            embed_provider: "unknown".to_string(),
            embed_model: "unknown".to_string(),
            embed_dim: 0,
            semantic_ready: false,
            semantic_reason: Some("未提供能力报告：语义能力按未就绪处理".to_string()),
            default_min_score: 0.0,
            scene_min_scores: BTreeMap::new(),
            consolidation: ConsolidationCapability::default(),
        }
    }
}

/// 基于令牌桶的限流器：按调用方 Agent 单维度限流
///
/// 多智能体场景下每个 Agent（OAuth client）都是独立配额主体，
/// 单个 Agent 的突发流量不得影响其他 Agent。
///
/// 存储可插拔：默认内存实现（单副本），生产可注入 PostgreSQL 实现
/// 以在**多副本部署下保持配额一致**——否则每个副本各自放行，限流失效。
pub struct RateLimiter {
    store: Arc<dyn common::RateLimitStore>,
    per_agent_rps: u32,
}

/// 限流判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateVerdict {
    Allowed,
    /// 超出限制，附带建议重试秒数
    Limited {
        retry_after_s: u64,
    },
}

pub use common::InMemoryRateLimitStore;

impl RateLimiter {
    /// 以进程内存储构造（单副本部署的默认选择）
    pub fn new(per_agent_rps: u32) -> Arc<Self> {
        Self::with_store(Arc::new(InMemoryRateLimitStore::new()), per_agent_rps)
    }

    /// 以共享存储构造（PostgreSQL 实现，用于多副本部署）
    pub fn with_store(store: Arc<dyn common::RateLimitStore>, per_agent_rps: u32) -> Arc<Self> {
        Arc::new(Self {
            store,
            per_agent_rps,
        })
    }

    /// 禁用限流（测试与本地开发用）
    pub fn disabled() -> Arc<Self> {
        Self::with_store(Arc::new(InMemoryRateLimitStore::new()), 0)
    }

    /// 是否已禁用限流（阈值为 0）
    pub fn is_disabled(&self) -> bool {
        self.per_agent_rps == 0
    }

    /// 为一次请求判定是否放行（按 agent_id 消耗一个令牌）
    ///
    /// 存储错误时按放行处理（fail-open）：限流不应成为可用性瓶颈。
    pub async fn check(&self, agent_id: &str) -> RateVerdict {
        if self.is_disabled() {
            return RateVerdict::Allowed;
        }

        let ok = self
            .store
            .try_acquire(common::RateDimension::Agent, agent_id, self.per_agent_rps)
            .await
            .unwrap_or(true);
        if !ok {
            return RateVerdict::Limited { retry_after_s: 1 };
        }

        RateVerdict::Allowed
    }
}

type SharedState = Arc<McpTransportState>;


mod dispatch;
mod http;
mod stdio;

pub use dispatch::{handle_jsonrpc, handle_jsonrpc_ext, handle_raw_frame};
pub use http::{first_text, mcp_router, tool_result_to_http};
pub use stdio::run_stdio;

#[cfg(test)]
mod tests;
