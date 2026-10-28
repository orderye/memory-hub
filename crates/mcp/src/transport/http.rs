use super::dispatch::MAX_BATCH_REQUESTS;
use super::*;

/// 单个 MCP 请求的硬超时（秒）。
///
/// `soft_timeout_ms` 只约束检索那一路的召回，DB/嵌入/整合等服务环节此前没有任何
/// 上限；一个卡住的下游会让连接与 tokio 任务无限挂住。
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// 构造 MCP axum 路由
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
        .with_state(state.clone())
        // 旧版 SSE 传输（/sse + /messages）。merge 必须放在各 layer 之后：
        // SSE 是长连接事件流，不能被上面的 30s 请求硬超时层掐断
        // （Router::layer 只包裹此前已注册的路由，后 merge 的路由不受影响）。
        .merge(super::sse::sse_routes(state))
}

/// GET `/mcp`：打开 SSE 通道的占位实现（本服务无服务端推送需求）
pub(super) async fn handle_http_mcp_get(State(state): State<SharedState>) -> Response {
    let _ = state;
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "Streamable HTTP 端点仅接受 POST",
    )
        .into_response()
}

/// 存活探针
pub(super) async fn handle_healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// 就绪探针（真探库：确认底层存储可用）
///
/// 响应体是 JSON：除存储可达性外，**显式声明语义检索能力是否就绪**。
/// 语义未就绪（伪向量嵌入器）时服务本身仍然可用——但调用方必须知道
/// 此刻的检索是词法匹配，不是语义检索。
pub(super) async fn handle_readyz(State(state): State<SharedState>) -> Response {
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
pub(super) async fn handle_capabilities(State(state): State<SharedState>) -> Response {
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
pub(super) async fn handle_http_mcp(
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

pub(super) fn jsonrpc_http(
    status: StatusCode,
    id: Option<Value>,
    code: i64,
    message: String,
) -> Response {
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
