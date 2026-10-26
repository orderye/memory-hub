use super::*;

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
pub(super) fn stdio_local_auth_ctx(profile: &str) -> Result<Option<AuthCtx>> {
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

