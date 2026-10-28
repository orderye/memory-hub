//! 旧版 HTTP + SSE 传输（MCP 规范 2024-11-05 的 "HTTP with SSE"）
//!
//! 部分 MCP 客户端只支持旧版 SSE 接入，握手流程与 Streamable HTTP 不同：
//!
//! 1. 客户端 `GET /sse` 建立下行事件流（可带 Bearer 令牌）；
//! 2. 服务端在流上先推一条 `endpoint` 事件，告知上行 POST 地址
//!    （`/messages?sessionId=<uuid>`）；
//! 3. 此后客户端把 JSON-RPC 帧 `POST /messages?sessionId=...`，
//!    服务端处理后把响应作为 `message` 事件从同一下行流推回，
//!    POST 本身只回 `202 Accepted`。
//!
//! 新客户端请优先使用 Streamable HTTP（POST /mcp），本传输只为兼容保留。
//!
//! 会话生命周期：SSE 流断开后，注册表里的发送端变成"僵尸"（接收端已随
//! 响应体丢弃）。这里采用惰性回收，不引入后台清扫任务：建立新连接时
//! 清理 `is_closed()` 的旧会话，POST 投递失败时移除对应会话。

use super::http::jsonrpc_http;
use super::*;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio::sync::mpsc;
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use uuid::Uuid;

/// SSE 会话表：sessionId → 下行事件发送端
#[derive(Default)]
pub(super) struct SseSessions {
    sessions: std::sync::Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>,
}

impl SseSessions {
    fn insert(&self, id: String, tx: mpsc::UnboundedSender<Event>) {
        self.sessions.lock().unwrap().insert(id, tx);
    }

    fn get(&self, id: &str) -> Option<mpsc::UnboundedSender<Event>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    fn remove(&self, id: &str) {
        self.sessions.lock().unwrap().remove(id);
    }

    /// 回收接收端已掉（SSE 流已断开）的僵尸会话
    pub(super) fn prune_closed(&self) {
        self.sessions
            .lock()
            .unwrap()
            .retain(|_, tx| !tx.is_closed());
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

/// SSE 路由的独立状态：共享传输状态 + 本路由私有的会话表
#[derive(Clone)]
pub(super) struct SseState {
    pub(super) shared: SharedState,
    pub(super) sessions: Arc<SseSessions>,
}

impl SseState {
    pub(super) fn new(shared: SharedState) -> Self {
        Self {
            shared,
            sessions: Arc::default(),
        }
    }
}

/// 挂载旧版 SSE 传输路由（/sse + /messages）
///
/// 调用方必须在应用请求超时层**之后** merge 本路由：SSE 是长连接事件流，
/// 不能被请求硬超时掐断（`Router::layer` 只包裹此前已注册的路由）。
pub(super) fn sse_routes(shared: SharedState) -> Router {
    Router::new()
        .route("/sse", axum::routing::get(handle_sse_connect))
        .route("/messages", axum::routing::post(handle_sse_message))
        .with_state(SseState::new(shared))
}

/// GET `/sse`：建立下行事件流，首帧推送 endpoint 事件
pub(super) async fn handle_sse_connect(State(st): State<SseState>, headers: HeaderMap) -> Response {
    // 与 POST /mcp 一致的令牌校验：带了就验，无效即 401；
    // 不带则留给 tools/call 返回 AuthRequired（经流推回）
    if let Some(token) = extract_bearer_token(&headers) {
        if let Err(e) = st.shared.introspector.introspect(&token).await {
            let (code, msg, _data) = hub_error_to_jsonrpc(&e);
            return jsonrpc_http(StatusCode::UNAUTHORIZED, None, code, msg);
        }
    }

    st.sessions.prune_closed();

    let session_id = Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::unbounded_channel::<Event>();
    st.sessions.insert(session_id.clone(), tx);

    // 旧版规范握手：首帧必须是 endpoint 事件，客户端据此得到上行地址
    let endpoint = Event::default()
        .event("endpoint")
        .data(format!("/messages?sessionId={session_id}"));
    // axum 0.8 的 Sse 要求 TryStream（Item = Result<Event, _>）：
    // mpsc 通道不会失败，错误类型取 Infallible
    use std::convert::Infallible;
    let stream = tokio_stream::iter(vec![Ok::<_, Infallible>(endpoint)])
        .chain(UnboundedReceiverStream::new(rx).map(Ok::<_, Infallible>));

    let mut resp = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    // nginx 等反代默认缓冲响应，会卡住事件流直到缓冲区满
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-accel-buffering"),
        header::HeaderValue::from_static("no"),
    );
    resp
}

/// POST `/messages?sessionId=...`：接收一条 JSON-RPC 帧，响应经下行流推回
pub(super) async fn handle_sse_message(
    State(st): State<SseState>,
    Query(q): Query<MessagesQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(tx) = st.sessions.get(&q.session_id) else {
        return jsonrpc_http(
            StatusCode::NOT_FOUND,
            None,
            INVALID_PARAMS,
            format!("未知或已断开的 SSE 会话: {}", q.session_id),
        );
    };

    // Origin 校验与 /mcp 一致（设计规范 6.4 节，防 DNS 重绑定）
    if !st.shared.allowed_origins.is_empty() {
        if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            if !st.shared.allowed_origins.iter().any(|o| o == origin) {
                return jsonrpc_http(
                    StatusCode::FORBIDDEN,
                    None,
                    INVALID_REQUEST,
                    "Origin 不被允许".to_string(),
                );
            }
        }
    }

    if body.len() > st.shared.max_request_bytes {
        return jsonrpc_http(
            StatusCode::PAYLOAD_TOO_LARGE,
            None,
            INVALID_REQUEST,
            format!("请求体超过上限 {} 字节", st.shared.max_request_bytes),
        );
    }

    // 鉴权与 /mcp 一致：无效令牌 401；未带令牌则留给 tools/call 报错
    let auth = match extract_bearer_token(&headers) {
        Some(token) => match st.shared.introspector.introspect(&token).await {
            Ok(ctx) => Some(ctx),
            Err(e) => {
                let (code, msg, _data) = hub_error_to_jsonrpc(&e);
                return jsonrpc_http(StatusCode::UNAUTHORIZED, None, code, msg);
            }
        },
        None => None,
    };

    // 限流与 /mcp 单请求一致：按调用方 Agent 逐 POST 扣令牌
    if let (Some(limiter), Some(ctx)) = (st.shared.rate_limiter.as_ref(), auth.as_ref()) {
        if let RateVerdict::Limited { retry_after_s } = limiter.check(&ctx.agent_id).await {
            let (code, msg, _data) = hub_error_to_jsonrpc(&HubError::RateLimited {
                retry_after_s: Some(retry_after_s),
            });
            return jsonrpc_http(StatusCode::TOO_MANY_REQUESTS, None, code, msg);
        }
    }

    // 旧版协议每条 POST 只承载一条 JSON-RPC 消息（不解析批量数组）
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
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

    // 标准通知帧：静默消费，不往下行流推任何东西
    if req.id.is_none() && req.method.starts_with("notifications/") {
        return StatusCode::ACCEPTED.into_response();
    }

    let resp = handle_jsonrpc_ext(&st.shared, auth.as_ref(), Some(&headers), req).await;
    let payload = serde_json::to_string(&resp).unwrap_or_default();

    // 投递失败 = SSE 流已断开：移除僵尸会话并告知调用方
    if tx
        .send(Event::default().event("message").data(payload))
        .is_err()
    {
        st.sessions.remove(&q.session_id);
        return jsonrpc_http(
            StatusCode::NOT_FOUND,
            None,
            INVALID_REQUEST,
            "SSE 会话已断开".to_string(),
        );
    }
    StatusCode::ACCEPTED.into_response()
}

/// `/messages` 的查询参数
#[derive(Debug, Deserialize)]
pub(super) struct MessagesQuery {
    #[serde(rename = "sessionId")]
    pub(super) session_id: String,
}
