use super::dispatch::{stdio_local_auth_ctx, MAX_BATCH_REQUESTS};
use super::http::{handle_capabilities, handle_healthz, handle_http_mcp, handle_readyz};
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
