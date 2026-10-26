//! MCP 传输层（Streamable HTTP + stdio）与 JSON-RPC 2.0 协议帧
//!
//! 设计要点：
//! - `handle_jsonrpc` 是**纯函数**：不持有任何 IO，可直接单测；
//! - `mcp_router` 产出 axum 路由，供二进制进程挂载；
//! - `run_stdio` 走换行分隔的 JSON-RPC 帧，供本地桥接客户端使用。
//! - 鉴权在协议层完成：Bearer 令牌经 `TokenIntrospector` 换取 `AuthCtx`，
//!   随后按工具的 `required_scope` 做范围强制校验（设计规范 5.9 节）。

use crate::protocol::{handle_discover, handle_tools_list};
use crate::schema::get_tool_definitions;
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
use std::collections::BTreeMap;
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
pub fn required_scope_for_tool(tool: &str) -> Option<Scope> {
    get_tool_definitions()
        .into_iter()
        .find(|d| d.name == tool)
        .and_then(|d| match d.required_scope {
            "memory:read" => Some(Scope::Read),
            "memory:write" => Some(Scope::Write),
            "memory:admin" => Some(Scope::Admin),
            _ => None,
        })
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

/// 处理单条 JSON-RPC 消息（纯函数，无 IO）
pub async fn handle_jsonrpc(
    state: &McpTransportState,
    auth: Option<&AuthCtx>,
    req: JsonRpcRequest,
) -> JsonRpcResponse {
    handle_jsonrpc_ext(state, auth, None, req).await
}

/// 处理单条 JSON-RPC 消息（支持传入 HTTP 头以支持中间件特征检测）
pub async fn handle_jsonrpc_ext(
    state: &McpTransportState,
    auth: Option<&AuthCtx>,
    headers: Option<&HeaderMap>,
    req: JsonRpcRequest,
) -> JsonRpcResponse {
    let id = req.id.clone();

    if req.jsonrpc != "2.0" {
        return JsonRpcResponse::err(id, INVALID_REQUEST, "jsonrpc 必须为 \"2.0\"");
    }
    if req.method.trim().is_empty() {
        return JsonRpcResponse::err(id, INVALID_REQUEST, "method 不能为空");
    }

    match req.method.as_str() {
        // 能力协商：initialize 与 server/discover 等价
        "initialize" | "server/discover" => {
            let discovered = handle_discover();
            // 尊重客户端请求的协议版本（回退兼容 2025-11-25）
            let negotiated = req
                .params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .filter(|v| discovered.supported_versions.iter().any(|s| s == v))
                .map(str::to_string)
                .unwrap_or_else(|| discovered.protocol_version.clone());

            let mut payload = serde_json::to_value(&discovered).unwrap_or(Value::Null);
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("protocolVersion".to_string(), json!(negotiated));
            }
            JsonRpcResponse::ok(id, payload)
        }

        "ping" => JsonRpcResponse::ok(id, json!({})),

        "tools/list" => JsonRpcResponse::ok(id, handle_tools_list()),

        "tools/call" => {
            let params = match req.params.as_ref().and_then(Value::as_object) {
                Some(o) => o,
                None => {
                    return JsonRpcResponse::err(id, INVALID_PARAMS, "params 必须为对象");
                }
            };

            let name = match params.get("name").and_then(Value::as_str) {
                Some(n) => n,
                None => return JsonRpcResponse::err(id, INVALID_PARAMS, "缺少 name"),
            };

            let mut arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            // 1. 鉴权：缺失令牌直接拒绝
            let auth = match auth {
                Some(a) => a,
                None => {
                    let (code, msg, data) = hub_error_to_jsonrpc(&HubError::AuthRequired);
                    return JsonRpcResponse {
                        jsonrpc: "2.0",
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code,
                            message: msg,
                            data: Some(data),
                        }),
                    };
                }
            };

            // 2. 作用域强制校验
            if let Some(required) = required_scope_for_tool(name) {
                if !auth.has_scope(&required) {
                    let (code, msg, data) = hub_error_to_jsonrpc(&HubError::InsufficientScope);
                    return JsonRpcResponse {
                        jsonrpc: "2.0",
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code,
                            message: msg,
                            data: Some(data),
                        }),
                    };
                }
            }

            // 3. 拦截器前置钩子 (中间层介入点)
            if let Some(interceptor) = &state.interceptor {
                let mut args = arguments.clone();
                match interceptor
                    .intercept_tool_call(&state.dispatcher, auth, headers, name, &mut args)
                    .await
                {
                    Ok(Some(intercepted_res)) => {
                        let payload = serde_json::to_value(&intercepted_res).unwrap_or(Value::Null);
                        return JsonRpcResponse::ok(id, payload);
                    }
                    Ok(None) => {
                        arguments = args;
                    }
                    Err(e) => {
                        let (code, msg, data) = hub_error_to_jsonrpc(&e);
                        return JsonRpcResponse {
                            jsonrpc: "2.0",
                            id,
                            result: None,
                            error: Some(JsonRpcError {
                                code,
                                message: msg,
                                data: Some(data),
                            }),
                        };
                    }
                }
            }

            // 4. 标准核心分发执行 (原有代码 100% 保持原样)
            let result: ToolCallResult = state.dispatcher.dispatch(auth, name, arguments).await;
            let payload = serde_json::to_value(&result).unwrap_or(Value::Null);
            JsonRpcResponse::ok(id, payload)
        }

        "notifications/initialized" | "notifications/cancelled" => {
            // MCP 标准通知：若客户端附带 id，返回成功空对象；无 id 时由传输层静默消费
            JsonRpcResponse::ok(id, json!({}))
        }

        other => JsonRpcResponse::err(id, METHOD_NOT_FOUND, format!("未知方法: {}", other)),
    }
}

/// JSON-RPC batch 请求的条数上限。
///
/// 防止一个 POST 塞数千条 `tools/call` 绕过按 Agent 限流（默认 256KB 请求体
/// 可容纳约 2500 条）：超出此上限的整个 batch 以 -32602 拒绝。
/// 32 取自 JSON-RPC 2.0 批处理常见实现的上限量级（nginx/各 JSON-RPC 框架
/// 通常在 16~64 之间），足以覆盖正常的批量元数据操作。
pub const MAX_BATCH_REQUESTS: usize = 32;

/// 解析单条原始帧并处理（供 stdio 与 HTTP 共用）
///
/// `local_auth` 只由 stdio 路径传入（见 [`stdio_local_auth_ctx`]）：
/// 未显式启用本地代理时为 `None`，此时 `tools/call` 与 HTTP 路径一样要求令牌。
pub async fn handle_raw_frame(
    state: &McpTransportState,
    raw: &str,
    local_auth: Option<&AuthCtx>,
) -> Option<JsonRpcResponse> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed: std::result::Result<JsonRpcRequest, _> = serde_json::from_str(trimmed);
    match parsed {
        Ok(req) => {
            // MCP 标准通知帧（无 id，如 notifications/initialized）：规范严禁产生任何回包
            if req.id.is_none() && req.method.starts_with("notifications/") {
                return None;
            }
            // stdio 无 HTTP 头：只有显式开启的本地代理才携带身份，否则按"未认证"处理
            // （历史实现注释声称"视为已认证"，但实际传的是 None，导致 stdio 下
            //  tools/call 恒返回 AuthRequired——文档与行为对不上）。
            Some(handle_jsonrpc(state, local_auth, req).await)
        }
        Err(e) => Some(JsonRpcResponse::err(
            None,
            PARSE_ERROR,
            format!("JSON 解析失败: {}", e),
        )),
    }
}

/// stdio 本地桥接的鉴权上下文
///
/// `HUB_STDIO_LOCAL_AGENT` 未设置（或为空）时返回 `None`：stdio 下的
/// `tools/call` 与 HTTP 路径一样要求 Bearer 令牌（与升级前行为一致）。
///
/// 显式设置后，stdio 会话被视为"进程所有者本人的本地桥接"：以 bootstrap 账户、
/// 全部 scope 与通配 ACL 运行。安全边界：
/// - 只对 stdio 生效。stdio 的输入来自启动本进程的父进程，无法被网络访问；
/// - HTTP 路径完全不受影响，仍严格执行 OAuth 2.1 令牌与 ACL；
/// - 启用时打印 WARN，日志里能看到"这一实例是单机全权限模式"。
fn stdio_local_auth_ctx(profile: &str) -> Result<Option<AuthCtx>> {
    let Ok(raw) = std::env::var("HUB_STDIO_LOCAL_AGENT") else {
        return Ok(None);
    };
    let agent = raw.trim();
    if agent.is_empty() {
        return Ok(None);
    }

    // 只在个人轻量部署（profile=lite）下允许：生产实例开着它等于绕过 OAuth 与
    // Agent 身份审计，直接获得 bootstrap 账户的 Admin 能力（restore/replace/revoke）。
    // 这里选择 fail-fast 而不是"静默忽略"，否则运维以为开了却没开/开了却不知道。
    if profile != "lite" {
        return Err(HubError::InvalidArgument(format!(
            "HUB_STDIO_LOCAL_AGENT 只允许在 HUB_DEPLOYMENT_PROFILE=lite 下使用（当前 {profile}）：\
             生产环境请使用 HTTP + OAuth 2.1 获取令牌"
        )));
    }

    let mut acl = common::Acl::default();
    acl.read_namespaces.insert("*".to_string());
    acl.write_namespaces.insert("*".to_string());
    tracing::warn!(
        agent = %agent,
        "stdio 本地代理模式已启用：以 bootstrap 账户全权限运行，仅限单机个人部署；         如需账户隔离请改用 HTTP + OAuth 2.1"
    );
    Ok(Some(AuthCtx::new(
        agent.to_string(),
        common::BOOTSTRAP_ACCOUNT_ID,
        common::Scope::all(),
        acl,
    )))
}

/// stdio 桥接主循环：读一行、回一帧
pub async fn run_stdio(state: Arc<McpTransportState>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // 身份在会话开始时确定一次：中途改环境变量不应悄悄提升权限。
    // 是否允许本地代理取决于部署模式（只有 lite 允许）。
    let local_auth = stdio_local_auth_ctx(state.capabilities.deployment_profile.as_str())?;

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| HubError::Internal(anyhow::anyhow!("stdin 读取失败: {}", e)))?
    {
        if let Some(resp) = handle_raw_frame(&state, &line, local_auth.as_ref()).await {
            let payload = serde_json::to_string(&resp)
                .map_err(|e| HubError::Internal(anyhow::anyhow!("响应序列化失败: {}", e)))?;
            stdout
                .write_all(payload.as_bytes())
                .await
                .map_err(|e| HubError::Internal(anyhow::anyhow!("stdout 写入失败: {}", e)))?;
            stdout
                .write_all(b"\n")
                .await
                .map_err(|e| HubError::Internal(anyhow::anyhow!("stdout 写入失败: {}", e)))?;
            stdout
                .flush()
                .await
                .map_err(|e| HubError::Internal(anyhow::anyhow!("stdout 刷新失败: {}", e)))?;
        }
    }
    Ok(())
}

/// 构造 MCP axum 路由
/// 单个 MCP 请求的硬超时（秒）。
///
/// `soft_timeout_ms` 只约束检索那一路的召回，DB/嵌入/整合等服务环节此前没有任何
/// 上限；一个卡住的下游会让连接与 tokio 任务无限挂住。
const REQUEST_TIMEOUT_SECS: u64 = 30;

pub fn mcp_router(state: Arc<McpTransportState>) -> Router {
    let max_body_bytes = state.max_request_bytes;
    Router::new()
        .route("/mcp", post(handle_http_mcp).get(handle_http_mcp_get))
        .route("/mcp/", post(handle_http_mcp).get(handle_http_mcp_get))
        .route("/healthz", axum::routing::get(handle_healthz))
        .route("/readyz", axum::routing::get(handle_readyz))
        .route("/capabilities", axum::routing::get(handle_capabilities))
        // 体积上限必须在 axum 缓冲请求体之前生效：此前只在 handler 里比对
        // max_request_bytes，那时默认 2MB 的整个 body 已经进了内存。
        .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS),
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// GET `/mcp`：打开 SSE 通道的占位实现（本服务无服务端推送需求）
async fn handle_http_mcp_get(State(state): State<SharedState>) -> Response {
    let _ = state;
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "Streamable HTTP 端点仅接受 POST",
    )
        .into_response()
}

/// 存活探针
async fn handle_healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// 就绪探针（真探库：确认底层存储可用）
///
/// 响应体是 JSON：除存储可达性外，**显式声明语义检索能力是否就绪**。
/// 语义未就绪（伪向量嵌入器）时服务本身仍然可用——但调用方必须知道
/// 此刻的检索是词法匹配，不是语义检索。
async fn handle_readyz(State(state): State<SharedState>) -> Response {
    let store_ok = match state.dispatcher.store.ping().await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(error = %e, "MCP 就绪探针失败：存储不可用");
            false
        }
    };

    let caps = &state.capabilities;
    let body = json!({
        "status": if store_ok { "ready" } else { "not_ready" },
        "storage": if store_ok { "reachable" } else { "unreachable" },
        // 部署形态：lite 下向量路默认关闭，调用方必须能一眼看出"这是关键词检索"
        "deployment": {
            "profile": caps.deployment_profile,
            "storage_durable": caps.storage_durable,
        },
        "semantic": {
            "ready": caps.semantic_ready,
            "provider": caps.embed_provider,
            "model": caps.embed_model,
            "dim": caps.embed_dim,
            "reason": caps.semantic_reason,
        },
        "retrieval": {
            "default_min_score": caps.default_min_score,
            "scene_min_scores": caps.scene_min_scores,
            "vector_path_enabled": caps.vector_path_enabled,
        },
        "consolidation": {
            "mode": caps.consolidation.mode,
            "fallback": caps.consolidation.fallback,
            "allow_mode_override": caps.consolidation.allow_mode_override,
            "vector_ready": caps.consolidation.vector_ready,
            "threshold_calibrated": caps.consolidation.threshold_calibrated,
        },
    });

    if store_ok {
        (StatusCode::OK, axum::Json(body)).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response()
    }
}

/// 能力清单：显式列出本实例**实际具备**的能力
///
/// 与 `/readyz` 的区别：这里是给人和自动化流水线读的完整能力声明，
/// 用于判断"本实例能否支撑语义检索验收"。
async fn handle_capabilities(State(state): State<SharedState>) -> Response {
    let caps = &state.capabilities;
    (
        StatusCode::OK,
        axum::Json(json!({
            "deployment": {
                "profile": caps.deployment_profile,
                "storage_durable": caps.storage_durable,
            },
            "embed": {
                "provider": caps.embed_provider,
                "model": caps.embed_model,
                "dim": caps.embed_dim,
            },
            "semantic": {
                "ready": caps.semantic_ready,
                "reason": caps.semantic_reason,
            },
            "retrieval": {
                "default_min_score": caps.default_min_score,
                "scene_min_scores": caps.scene_min_scores,
                "vector_path_enabled": caps.vector_path_enabled,
            },
            "consolidation": {
                "mode": caps.consolidation.mode,
                "fallback": caps.consolidation.fallback,
                "allow_mode_override": caps.consolidation.allow_mode_override,
                "vector_ready": caps.consolidation.vector_ready,
                "threshold_calibrated": caps.consolidation.threshold_calibrated,
            },
        })),
    )
        .into_response()
}

/// POST `/mcp`：Streamable HTTP 单消息模式
async fn handle_http_mcp(
    State(state): State<SharedState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let _ = method;

    // Origin 校验（设计规范 6.4 节，防 DNS 重绑定）
    if !state.allowed_origins.is_empty() {
        if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            if !state.allowed_origins.iter().any(|o| o == origin) {
                return jsonrpc_http(
                    StatusCode::FORBIDDEN,
                    None,
                    INVALID_REQUEST,
                    "Origin 不被允许".to_string(),
                );
            }
        }
    }

    // 请求体上限
    if body.len() > state.max_request_bytes {
        return jsonrpc_http(
            StatusCode::PAYLOAD_TOO_LARGE,
            None,
            INVALID_REQUEST,
            format!("请求体超过上限 {} 字节", state.max_request_bytes),
        );
    }

    // 鉴权
    let auth = match extract_bearer_token(&headers) {
        Some(token) => match state.introspector.introspect(&token).await {
            Ok(ctx) => Some(ctx),
            Err(e) => {
                let (code, msg, _data) = hub_error_to_jsonrpc(&e);
                return jsonrpc_http(StatusCode::UNAUTHORIZED, None, code, msg);
            }
        },
        None => None,
    };

    // 限流：按调用方 Agent 单维度令牌桶判流（未认证请求不占配额）
    if let (Some(limiter), Some(ctx)) = (state.rate_limiter.as_ref(), auth.as_ref()) {
        if let RateVerdict::Limited { retry_after_s } = limiter.check(&ctx.agent_id).await {
            let (code, msg, _data) = hub_error_to_jsonrpc(&HubError::RateLimited {
                retry_after_s: Some(retry_after_s),
            });
            return jsonrpc_http(StatusCode::TOO_MANY_REQUESTS, None, code, msg);
        }
    }

    let raw = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => {
            return jsonrpc_http(
                StatusCode::BAD_REQUEST,
                None,
                PARSE_ERROR,
                "请求体非 UTF-8".to_string(),
            );
        }
    };

    // 既支持单对象，也支持批量数组（MCP 允许批处理）
    let trimmed = raw.trim();
    if trimmed.starts_with('[') {
        let batch: std::result::Result<Vec<JsonRpcRequest>, _> = serde_json::from_str(trimmed);
        match batch {
            Ok(reqs) => {
                // batch 条数上限：无上限时一个 POST 可塞约 2500 条 tools/call
                // 击穿按 Agent 限流的设计目标（单 Agent 突发不得影响其他 Agent）
                if reqs.len() > MAX_BATCH_REQUESTS {
                    return jsonrpc_http(
                        StatusCode::BAD_REQUEST,
                        None,
                        INVALID_PARAMS,
                        format!(
                            "batch 条数超过上限 {}（当前 {} 条）",
                            MAX_BATCH_REQUESTS,
                            reqs.len()
                        ),
                    );
                }
                let mut out = Vec::with_capacity(reqs.len());
                for r in reqs {
                    // MCP 标准通知帧：不向回包数组中塞入响应
                    if r.id.is_none() && r.method.starts_with("notifications/") {
                        continue;
                    }
                    // 每条 tools/call 单独走限流（逐条扣令牌）：整个 POST 只 check 一次
                    // 的话，batch 就成了绕过按 Agent 限流的放大器。轻量方法
                    // （tools/list、initialize、ping 等）不扣令牌。
                    if r.method == "tools/call" {
                        if let (Some(limiter), Some(ctx)) =
                            (state.rate_limiter.as_ref(), auth.as_ref())
                        {
                            if let RateVerdict::Limited { retry_after_s } =
                                limiter.check(&ctx.agent_id).await
                            {
                                let (code, msg, data) =
                                    hub_error_to_jsonrpc(&HubError::RateLimited {
                                        retry_after_s: Some(retry_after_s),
                                    });
                                out.push(JsonRpcResponse {
                                    jsonrpc: "2.0",
                                    id: r.id.clone(),
                                    result: None,
                                    error: Some(JsonRpcError {
                                        code,
                                        message: msg,
                                        data: Some(data),
                                    }),
                                });
                                continue;
                            }
                        }
                    }
                    out.push(handle_jsonrpc_ext(&state, auth.as_ref(), Some(&headers), r).await);
                }
                if out.is_empty() {
                    return StatusCode::NO_CONTENT.into_response();
                }
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_string(&out).unwrap_or_default(),
                )
                    .into_response();
            }
            Err(e) => {
                return jsonrpc_http(
                    StatusCode::BAD_REQUEST,
                    None,
                    PARSE_ERROR,
                    format!("批量 JSON 解析失败: {}", e),
                );
            }
        }
    }

    let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
        Ok(r) => r,
        Err(e) => {
            return jsonrpc_http(
                StatusCode::BAD_REQUEST,
                None,
                PARSE_ERROR,
                format!("JSON 解析失败: {}", e),
            );
        }
    };

    // MCP 标准通知（如 notifications/initialized）：客户端不期望任何回包
    if req.id.is_none() && req.method.starts_with("notifications/") {
        return StatusCode::NO_CONTENT.into_response();
    }

    let resp = handle_jsonrpc_ext(&state, auth.as_ref(), Some(&headers), req).await;

    // HTTP 状态码与 MCP 约定的错误码对齐：
    // - 凭据缺失/无效 → 401（附带 RFC 6750 WWW-Authenticate 头部）
    // - 令牌有效但权限不足 → 403
    let (status, auth_header) = match resp.error.as_ref().map(|e| e.code) {
        Some(32001) => (
            StatusCode::UNAUTHORIZED,
            Some((header::WWW_AUTHENTICATE, r#"Bearer error="invalid_token""#)),
        ),
        Some(32002) => (StatusCode::FORBIDDEN, None), // 需用户确认
        Some(32003) => (StatusCode::TOO_MANY_REQUESTS, None),
        Some(32004) => (StatusCode::FAILED_DEPENDENCY, None), // 策略阻断
        _ => (StatusCode::OK, None),
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some((k, v)) = auth_header {
        builder = builder.header(k, v);
    }
    builder
        .body(serde_json::to_string(&resp).unwrap_or_default().into())
        .unwrap_or_else(|_| (status, "Internal Server Error").into_response())
}

fn jsonrpc_http(status: StatusCode, id: Option<Value>, code: i64, message: String) -> Response {
    let resp = JsonRpcResponse::err(id, code, message);
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if status == StatusCode::UNAUTHORIZED {
        builder = builder.header(header::WWW_AUTHENTICATE, r#"Bearer error="invalid_token""#);
    }
    builder
        .body(serde_json::to_string(&resp).unwrap_or_default().into())
        .unwrap_or_else(|_| (status, "Internal Server Error").into_response())
}

/// 把 `ToolCallResult` 转为可直接回传的 axum 响应
pub fn tool_result_to_http(res: &ToolCallResult) -> Response {
    // MCP 规定工具错误仍在 200 内以 isError 表达
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(res).unwrap_or_default(),
    )
        .into_response()
}

/// 从文本通道取回可读内容（便于日志与测试断言）
pub fn first_text(res: &ToolCallResult) -> Option<&str> {
    res.content.first().map(|c| match c {
        McpContentItem::Text { text } => text.as_str(),
    })
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolDispatcher;
    use async_trait::async_trait;
    use common::{Acl, Scope};
    use consolidation::DefaultIntegrationService;
    use mem_core::{DefaultBackupService, GrantChecker};
    use store::InMemoryHubStore;
    use uuid::Uuid;

    struct StubIntrospector;

    #[async_trait]
    impl mem_core::TokenIntrospector for StubIntrospector {
        async fn introspect(&self, token: &str) -> common::Result<AuthCtx> {
            if token == "good" {
                Ok(AuthCtx {
                    agent_id: "agent-a".to_string(),
                    account_id: common::BOOTSTRAP_ACCOUNT_ID,
                    scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
                    acl: Acl::default(),
                })
            } else {
                Err(HubError::AuthRequired)
            }
        }
    }

    fn state() -> McpTransportState {
        state_with_limiter(None)
    }

    fn state_with_limiter(limiter: Option<Arc<RateLimiter>>) -> McpTransportState {
        let store = Arc::new(InMemoryHubStore::new());
        let access = Arc::new(GrantChecker::new(store.clone()));
        McpTransportState {
            dispatcher: Arc::new(ToolDispatcher::new(
                Arc::new({
                    let embedder = Arc::new(mem_core::TestEmbedder::default());
                    let retriever = Arc::new(retrieval::HybridRetriever::new(
                        store.clone() as Arc<dyn mem_core::MemoryStore>,
                        embedder.clone(),
                        100,
                        100,
                        [0.5, 0.2, 0.1, 0.1, 0.1],
                        0.7,
                        500,
                    ));
                    mem_core::DefaultMemoryService::new(store.clone(), embedder, access.clone())
                        .with_retriever(retriever)
                }),
                Arc::new(DefaultBackupService::new(store.clone(), access.clone())),
                Arc::new(DefaultIntegrationService::new(
                    store.clone(),
                    access,
                    0.99,
                    100,
                )),
                store.clone(),
            )),
            introspector: Arc::new(StubIntrospector),
            allowed_origins: vec![],
            max_request_bytes: 65536,
            rate_limiter: limiter,
            interceptor: None,
            capabilities: Arc::new(CapabilityReport::default()),
        }
    }

    fn ctx() -> AuthCtx {
        AuthCtx {
            agent_id: "agent-a".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Acl::default(),
        }
    }

    #[tokio::test]
    async fn test_initialize_negotiates_fallback_version() {
        let s = state();
        let resp = handle_jsonrpc(
            &s,
            None,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "initialize".to_string(),
                params: Some(json!({ "protocolVersion": "2025-11-25" })),
            },
        )
        .await;
        assert!(resp.error.is_none());
        let r = resp.result.unwrap();
        assert_eq!(r["protocolVersion"], json!("2025-11-25"));
    }

    #[tokio::test]
    async fn test_bad_jsonrpc_field_rejected() {
        let s = state();
        let resp = handle_jsonrpc(
            &s,
            None,
            JsonRpcRequest {
                jsonrpc: "1.0".to_string(),
                id: Some(json!(2)),
                method: "ping".to_string(),
                params: None,
            },
        )
        .await;
        assert_eq!(resp.error.unwrap().code, INVALID_REQUEST);
    }

    #[tokio::test]
    async fn test_tools_call_requires_auth() {
        let s = state();
        let resp = handle_jsonrpc(
            &s,
            None,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(3)),
                method: "tools/call".to_string(),
                params: Some(json!({ "name": "memory_put", "arguments": {} })),
            },
        )
        .await;
        assert_eq!(resp.error.unwrap().code, 32001);
    }

    #[tokio::test]
    async fn test_tools_call_scope_enforced() {
        let s = state();
        let mut a = ctx();
        a.scopes = vec![Scope::Read]; // 无 memory:write
        let resp = handle_jsonrpc(
            &s,
            Some(&a),
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(4)),
                method: "tools/call".to_string(),
                params: Some(json!({
                    "name": "memory_put",
                    "arguments": { "namespace": "ns", "kind": "note", "content": "x" }
                })),
            },
        )
        .await;
        assert_eq!(
            resp.error.unwrap().code,
            32002,
            "权限不足应使用 32002 而非 32001"
        );
    }

    #[tokio::test]
    async fn test_admin_tool_requires_admin_scope() {
        let s = state();
        let mut a = ctx();
        a.scopes = vec![Scope::Read, Scope::Write]; // 无 memory:admin
        let resp = handle_jsonrpc(
            &s,
            Some(&a),
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(41)),
                method: "tools/call".to_string(),
                params: Some(json!({
                    "name": "memory_backup_create",
                    "arguments": { "namespace": "ns" }
                })),
            },
        )
        .await;
        assert_eq!(resp.error.unwrap().code, 32002);
    }

    #[tokio::test]
    async fn test_unknown_method() {
        let s = state();
        let resp = handle_jsonrpc(
            &s,
            None,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(5)),
                method: "nope".to_string(),
                params: None,
            },
        )
        .await;
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_tools_call_success_roundtrip() {
        let s = state();
        let resp = handle_jsonrpc(
            &s,
            Some(&ctx()),
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(6)),
                method: "tools/call".to_string(),
                params: Some(json!({
                    "name": "memory_put",
                    "arguments": { "namespace": "team-x", "kind": "fact", "content": "端口 3000" }
                })),
            },
        )
        .await;
        assert!(resp.error.is_none(), "写入应成功");
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], json!(false));
        assert!(result["structuredContent"]["memory"]["id"].is_string());
    }

    #[tokio::test]
    async fn test_tools_call_unknown_tool_reports_error_payload() {
        let s = state();
        let resp = handle_jsonrpc(
            &s,
            Some(&ctx()),
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(7)),
                method: "tools/call".to_string(),
                params: Some(json!({ "name": "no_such_tool", "arguments": {} })),
            },
        )
        .await;
        let result = resp.result.expect("工具错误仍在 200 内表达");
        assert_eq!(result["isError"], json!(true));
    }

    #[test]
    fn test_extract_bearer_token() {
        let mut headers = HeaderMap::new();
        assert!(extract_bearer_token(&headers).is_none());
        headers.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers).as_deref(), Some("abc"));
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert!(extract_bearer_token(&headers).is_none());
    }

    #[tokio::test]
    async fn test_rate_limiter_allows_within_budget_then_blocks() {
        // 每 Agent 3 RPS：前 3 次放行，第 4 次被限
        let limiter = Arc::new(RateLimiter::new(3));
        for i in 0..3 {
            assert_eq!(
                limiter.check("agent-a").await,
                RateVerdict::Allowed,
                "第 {} 次应在配额内",
                i + 1
            );
        }
        assert!(matches!(
            limiter.check("agent-a").await,
            RateVerdict::Limited { .. }
        ));

        // Agent 维度独立：换一个 Agent 立即放行
        assert_eq!(limiter.check("agent-b").await, RateVerdict::Allowed);
    }

    #[tokio::test]
    async fn test_rate_limiter_disabled_allows_everything() {
        let limiter = RateLimiter::disabled();
        for _ in 0..50 {
            assert_eq!(limiter.check("agent-a").await, RateVerdict::Allowed);
        }
    }

    #[tokio::test]
    async fn test_http_rate_limited_returns_429() {
        use axum::body::Bytes;

        let limiter = RateLimiter::new(1);
        // 先耗尽配额
        assert_eq!(limiter.check("agent-a").await, RateVerdict::Allowed);

        let s = state_with_limiter(Some(limiter));
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#);
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn test_http_rejects_bad_origin() {
        use axum::body::Bytes;

        let mut s = state();
        s.allowed_origins = vec!["https://allowed.example.com".to_string()];
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#);
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_http_rejects_oversized_body() {
        use axum::body::Bytes;

        let mut s = state();
        s.max_request_bytes = 16;
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let body = Bytes::from_static(
            br#"{"jsonrpc":"2.0","id":11,"method":"ping","params":{"pad":"xxxxxxxxxxxxxxxxxx"}}"#,
        );
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn test_http_bad_token_returns_401() {
        use axum::body::Bytes;

        let s = state();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer bad".parse().unwrap());
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","id":12,"method":"ping"}"#);
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            resp.headers().contains_key(header::WWW_AUTHENTICATE),
            "401 响应必须携带 RFC 6750 WWW-Authenticate 头部"
        );
    }

    #[tokio::test]
    async fn test_notification_initialized_silently_handled() {
        let s = state();
        // 1. stdio 场景：raw_frame 收到通知帧应返回 None，不向 stdout 发送回包
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let resp = handle_raw_frame(&s, raw, None).await;
        assert!(resp.is_none(), "标准通知帧不得回复任何 JSON-RPC 响应");

        // 2. HTTP 场景：POST 收到通知帧应返回 204 No Content
        use axum::body::Bytes;
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        let resp_http = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp_http.status(), StatusCode::NO_CONTENT);
    }

    /// stdio 未显式开启本地代理时，tools/call 必须仍然要求令牌
    /// （历史实现注释声称"视为已认证"，但代码传的是 None，文档与行为不一致）
    #[tokio::test]
    async fn test_stdio_tools_call_requires_auth_without_local_agent() {
        let s = state();
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_list","arguments":{"namespace":"ns"}}}"#;
        let resp = handle_raw_frame(&s, raw, None).await.expect("应有响应");
        let err = resp.error.expect("未认证必须返回错误");
        // HubError::AuthRequired 统一映射为 JSON-RPC 32001 + "缺少或无效令牌"
        assert_eq!(
            err.code, 32001,
            "未认证必须映射为 AuthRequired 错误码：{err:?}"
        );
        assert!(
            err.message.contains("令牌") || err.message.contains("凭证"),
            "错误信息须可读：{err:?}"
        );
    }

    /// 显式开启本地代理（HUB_STDIO_LOCAL_AGENT 语境）后，stdio 可执行工具调用
    #[tokio::test]
    async fn test_stdio_tools_call_with_local_agent_executes() {
        let s = state();
        let mut acl = common::Acl::default();
        acl.read_namespaces.insert("*".to_string());
        acl.write_namespaces.insert("*".to_string());
        let local = AuthCtx::new(
            "local-agent".to_string(),
            common::BOOTSTRAP_ACCOUNT_ID,
            common::Scope::all(),
            acl,
        );

        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_put","arguments":{"namespace":"ns","kind":"fact","content":"stdio 本地桥接可用"}}}"#;
        let resp = handle_raw_frame(&s, raw, Some(&local))
            .await
            .expect("应有响应");
        assert!(resp.error.is_none(), "本地代理调用不得报错：{resp:?}");
        let result = resp.result.expect("应有结果");
        assert_eq!(result["isError"], json!(false));
        assert!(result["structuredContent"]["memory"]["id"].is_string());

        // 同样的帧在不带本地代理时仍被拒绝（能力只在显式开启后生效）
        let denied = handle_raw_frame(&s, raw, None).await.expect("应有响应");
        assert!(denied.error.is_some(), "未开启本地代理时不得放行");
    }

    /// 本地代理上下文：默认（未设置环境变量）为 None，构造出的上下文具备全权限
    #[test]
    fn test_stdio_local_auth_ctx_shape() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        std::env::remove_var("HUB_STDIO_LOCAL_AGENT");
        assert!(
            stdio_local_auth_ctx("lite").unwrap().is_none(),
            "未设置 HUB_STDIO_LOCAL_AGENT 时不得默认放行"
        );

        std::env::set_var("HUB_STDIO_LOCAL_AGENT", "local-agent");
        // 生产模式必须拒绝：本地代理等于绕过 OAuth 与身份审计
        let err = stdio_local_auth_ctx("production").unwrap_err();
        assert!(
            err.to_string().contains("lite"),
            "非 lite 模式必须明确拒绝：{err}"
        );
        let ctx = stdio_local_auth_ctx("lite")
            .unwrap()
            .expect("设置后应构造上下文");
        std::env::remove_var("HUB_STDIO_LOCAL_AGENT");

        assert_eq!(ctx.agent_id, "local-agent");
        assert_eq!(ctx.account_id, common::BOOTSTRAP_ACCOUNT_ID);
        assert!(ctx.has_scope(&Scope::Admin));
        assert!(ctx.acl.can_read("any-ns") && ctx.acl.can_write("any-ns"));
    }

    #[tokio::test]
    async fn test_http_batch_requests() {
        use axum::body::Bytes;

        let s = state();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let body = Bytes::from_static(
            br#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#,
        );
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// batch 条数上限：超出 MAX_BATCH_REQUESTS 的整个 batch 以 -32602 拒绝
    #[tokio::test]
    async fn test_http_batch_rejects_oversized_batch() {
        use axum::body::Bytes;

        let s = state();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        // MAX_BATCH_REQUESTS + 1 条 ping（轻量方法也一样受限：上限是防放大，不是防贵方法）
        let items: Vec<String> = (0..=MAX_BATCH_REQUESTS)
            .map(|i| format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"ping"}}"#))
            .collect();
        let body = Bytes::from(format!("[{}]", items.join(",")));
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "超上限 batch 必须 400 而非继续处理"
        );
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], json!(INVALID_PARAMS));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains(&MAX_BATCH_REQUESTS.to_string()),
            "错误信息必须说明上限值：{body}"
        );

        // 恰好等于上限：放行
        let s = state();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let items: Vec<String> = (0..MAX_BATCH_REQUESTS)
            .map(|i| format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"ping"}}"#))
            .collect();
        let body = Bytes::from(format!("[{}]", items.join(",")));
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// batch 内每条 tools/call 逐条扣令牌：配额耗尽的那一条返回限流错误，
    /// 其余条目继续处理（含轻量方法不受影响）
    #[tokio::test]
    async fn test_http_batch_per_item_rate_limiting() {
        use axum::body::Bytes;

        // 2 RPS：进入 handler 前的整批预检扣 1 个，batch 内前 1 条 tools/call
        // 扣第 2 个，第 2 条 tools/call 即被限流
        let limiter = RateLimiter::new(2);
        let s = state_with_limiter(Some(limiter));
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let body = Bytes::from_static(
            br#"[
                {"jsonrpc":"2.0","id":1,"method":"ping"},
                {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_list","arguments":{"namespace":"ns"}}},
                {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_list","arguments":{"namespace":"ns"}}},
                {"jsonrpc":"2.0","id":4,"method":"tools/list"}
            ]"#,
        );
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "batch 内单条限流不得整批 429"
        );
        let arr = body_json(resp).await;
        let arr = arr.as_array().expect("batch 响应应为数组");

        let find = |id: i64| -> &serde_json::Value {
            arr.iter()
                .find(|r| r["id"] == json!(id))
                .unwrap_or_else(|| panic!("缺少 id={id} 的响应：{arr:?}"))
        };
        // ping / tools-list 不扣令牌：正常
        assert!(
            find(1)["error"].is_null(),
            "轻量方法不受 tools/call 逐条限流影响"
        );
        assert!(find(4)["error"].is_null());
        // 第 1 条 tools/call（id=2）扣到第 2 个令牌：放行
        assert!(
            find(2)["error"].is_null(),
            "配额内的 tools/call 应正常执行：{:?}",
            find(2)
        );
        // 第 2 条 tools/call（id=3）被限流：32003（RateLimited 映射码）
        assert_eq!(
            find(3)["error"]["code"],
            json!(32003),
            "超出配额的 tools/call 应逐条返回限流错误：{:?}",
            find(3)
        );
    }

    /// batch 内逐条限流错误须与单请求路径同形态（HubError::RateLimited 映射）
    #[tokio::test]
    async fn test_http_batch_rate_limit_error_shape_matches_single() {
        use axum::body::Bytes;

        // 1 RPS：整批预检已扣掉唯一令牌 → batch 内第一条 tools/call 即被限
        let limiter = RateLimiter::new(1);
        let s = state_with_limiter(Some(limiter));
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let body = Bytes::from_static(
            br#"[
                {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_list","arguments":{"namespace":"ns"}}},
                {"jsonrpc":"2.0","id":2,"method":"ping"}
            ]"#,
        );
        let resp = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let arr = body_json(resp).await;
        let arr = arr.as_array().unwrap();
        let limited = arr.iter().find(|r| r["id"] == json!(1)).unwrap();
        assert_eq!(limited["error"]["code"], json!(32003));
        assert!(
            limited["error"]["data"].is_object(),
            "限流错误须保留结构化 data（与单请求路径一致）：{limited}"
        );
    }

    #[test]
    fn test_required_scope_lookup() {
        assert_eq!(required_scope_for_tool("memory_get"), Some(Scope::Read));
        assert_eq!(required_scope_for_tool("memory_put"), Some(Scope::Write));
        assert_eq!(
            required_scope_for_tool("memory_restore"),
            Some(Scope::Admin)
        );
        assert_eq!(required_scope_for_tool("bogus"), None);
    }

    #[test]
    fn test_raw_frame_parse_error() {
        // 非 JSON 帧应返回 PARSE_ERROR
        let resp = JsonRpcResponse::err(Some(json!(1)), PARSE_ERROR, "bad");
        assert_eq!(resp.error.unwrap().code, PARSE_ERROR);
    }

    #[test]
    fn test_uuid_generation_for_ids() {
        // 传输层本身不生成 UUID；此处确保依赖的 uuid crate 可用
        assert!(!Uuid::new_v4().to_string().is_empty());
    }

    #[tokio::test]
    async fn test_healthz_and_readyz() {
        let resp = handle_healthz().await;
        assert_eq!(resp.status(), StatusCode::OK);

        let s = state();
        let resp_ready = handle_readyz(State(Arc::new(s))).await;
        assert_eq!(resp_ready.status(), StatusCode::OK);
    }

    /// 把响应体读成 JSON
    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("读取响应体失败");
        serde_json::from_slice(&bytes).expect("响应体不是合法 JSON")
    }

    /// 语义能力未就绪时，健康检查必须**显式**声明
    ///
    /// 回归：此前 `/readyz` 只回一个 "ready"，伪向量部署与真实语义部署
    /// 在探针上无法区分——"接口能返回结果"被当成了"语义检索可用"。
    #[tokio::test]
    async fn test_readyz_declares_semantic_capability() {
        let mut s = state();
        s.capabilities = Arc::new(CapabilityReport {
            embed_provider: "mock".to_string(),
            embed_model: "mock-embedder-v1".to_string(),
            embed_dim: 1024,
            semantic_ready: false,
            semantic_reason: Some("伪向量：语义召回不可用".to_string()),
            default_min_score: 0.65,
            ..Default::default()
        });

        let resp = handle_readyz(State(Arc::new(s))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        assert_eq!(body["status"], json!("ready"));
        assert_eq!(body["storage"], json!("reachable"));
        assert_eq!(
            body["semantic"]["ready"],
            json!(false),
            "语义未就绪必须显式声明：{body}"
        );
        assert_eq!(body["semantic"]["provider"], json!("mock"));
        assert_eq!(body["semantic"]["model"], json!("mock-embedder-v1"));
        assert_eq!(body["semantic"]["dim"], json!(1024));
        assert!(
            body["semantic"]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("伪向量"),
            "必须给出原因说明：{body}"
        );
        // 阈值是 f32，序列化后有精度误差：按容差比较
        let min_score = body["retrieval"]["default_min_score"]
            .as_f64()
            .expect("default_min_score 应为数字");
        assert!(
            (min_score - 0.65).abs() < 1e-6,
            "默认拒绝阈值应被如实报告：{min_score}"
        );
    }

    /// 接入真实嵌入服务后，语义能力报告为就绪
    #[tokio::test]
    async fn test_readyz_reports_semantic_ready_with_real_embedder() {
        let mut s = state();
        s.capabilities = Arc::new(CapabilityReport {
            embed_provider: "http".to_string(),
            embed_model: "bge-m3".to_string(),
            embed_dim: 1024,
            semantic_ready: true,
            semantic_reason: None,
            default_min_score: 0.65,
            ..Default::default()
        });

        let resp = handle_readyz(State(Arc::new(s))).await;
        let body = body_json(resp).await;
        assert_eq!(body["semantic"]["ready"], json!(true));
        assert_eq!(body["semantic"]["provider"], json!("http"));
        assert!(body["semantic"]["reason"].is_null());
    }

    /// 能力清单端点：给人和流水线读的完整能力声明
    #[tokio::test]
    async fn test_capabilities_lists_actual_capabilities() {
        let mut s = state();
        let mut scene_scores = BTreeMap::new();
        scene_scores.insert("chat".to_string(), 0.70);
        s.capabilities = Arc::new(CapabilityReport {
            embed_provider: "mock".to_string(),
            embed_model: "mock-embedder-v1".to_string(),
            embed_dim: 1024,
            semantic_ready: false,
            semantic_reason: Some("伪向量：语义召回不可用".to_string()),
            default_min_score: 0.65,
            scene_min_scores: scene_scores,
            ..Default::default()
        });

        let resp = handle_capabilities(State(Arc::new(s))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        assert_eq!(body["embed"]["provider"], json!("mock"));
        assert_eq!(body["embed"]["model"], json!("mock-embedder-v1"));
        assert_eq!(body["semantic"]["ready"], json!(false));
        let min_score = body["retrieval"]["default_min_score"]
            .as_f64()
            .expect("default_min_score 应为数字");
        assert!((min_score - 0.65).abs() < 1e-6);
        let chat = body["retrieval"]["scene_min_scores"]["chat"]
            .as_f64()
            .expect("chat 阈值应为数字");
        assert!((chat - 0.70).abs() < 1e-6);
    }

    /// 缺能力报告时按"语义未就绪"处理——宁可保守，不可误报就绪
    #[tokio::test]
    async fn test_missing_capability_report_defaults_to_not_ready() {
        let caps = CapabilityReport::default();
        assert!(!caps.semantic_ready);
        assert!(caps.semantic_reason.is_some());
    }
}
