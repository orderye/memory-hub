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

/// 解析单条原始帧并处理（供 stdio 与 HTTP 共用）
pub async fn handle_raw_frame(state: &McpTransportState, raw: &str) -> Option<JsonRpcResponse> {
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
            // stdio 场景无 HTTP 头，鉴权由调用方通过 params 或环境注入的
            // 服务主体承担；此处视为"已认证的本地桥接"。
            Some(handle_jsonrpc(state, None, req).await)
        }
        Err(e) => Some(JsonRpcResponse::err(
            None,
            PARSE_ERROR,
            format!("JSON 解析失败: {}", e),
        )),
    }
}

/// stdio 桥接主循环：读一行、回一帧
pub async fn run_stdio(state: Arc<McpTransportState>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| HubError::Internal(anyhow::anyhow!("stdin 读取失败: {}", e)))?
    {
        if let Some(resp) = handle_raw_frame(&state, &line).await {
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
async fn handle_readyz(State(state): State<SharedState>) -> Response {
    match state.dispatcher.store.ping().await {
        Ok(()) => (StatusCode::OK, "ready").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "MCP 就绪探针失败：存储不可用");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable").into_response()
        }
    }
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
                let mut out = Vec::with_capacity(reqs.len());
                for r in reqs {
                    // MCP 标准通知帧：不向回包数组中塞入响应
                    if r.id.is_none() && r.method.starts_with("notifications/") {
                        continue;
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
                Arc::new(mem_core::DefaultMemoryService::new(
                    store.clone(),
                    Arc::new(mem_core::TestEmbedder::default()),
                    access.clone(),
                    [0.5, 0.2, 0.1, 0.1, 0.1],
                )),
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
        let resp = handle_raw_frame(&s, raw).await;
        assert!(resp.is_none(), "标准通知帧不得回复任何 JSON-RPC 响应");

        // 2. HTTP 场景：POST 收到通知帧应返回 204 No Content
        use axum::body::Bytes;
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        let resp_http = handle_http_mcp(State(Arc::new(s)), Method::POST, headers, body).await;
        assert_eq!(resp_http.status(), StatusCode::NO_CONTENT);
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
}
