use super::account::extract_client_ip;
use super::*;

pub(super) async fn handle_authorization_server_metadata(State(s): State<AsWebState>) -> Response {
    let base = s.external_base_url.trim_end_matches('/');
    json_response(json!({
        "issuer": s.oauth.issuer(),
        "authorization_endpoint": format!("{}/oauth/authorize", base),
        "token_endpoint": format!("{}/oauth/token", base),
        "registration_endpoint": format!("{}/oauth/register", base),
        "introspection_endpoint": format!("{}/oauth/introspect", base),
        "revocation_endpoint": format!("{}/oauth/revoke", base),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token", "client_credentials"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
        "scopes_supported": ["memory:read", "memory:write", "memory:admin"],
        "registration_requires_account": true,
    }))
}

/// RFC 9728 受保护资源元数据
pub(super) async fn handle_protected_resource_metadata(State(s): State<AsWebState>) -> Response {
    let base = s.external_base_url.trim_end_matches('/');
    json_response(json!({
        "resource": s.oauth.audience(),
        "authorization_servers": [s.oauth.issuer()],
        "scopes_supported": ["memory:read", "memory:write", "memory:admin"],
        "bearer_methods_supported": ["header"],
        "resource_documentation": format!("{}/console", base),
    }))
}

// ===== 动态注册（RFC 7591）：必须携带账户身份 =====

/// POST /oauth/register（DCR）
///
/// 归属账户来自登录会话 Cookie 或 `Authorization: Bearer <注册令牌>`，
/// 请求体中的任何账户信息都被忽略——沙箱归属不可伪造。
pub(super) async fn handle_register(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    body: axum::extract::Json<crate::oauth_flow::RegisterAgentRequest>,
) -> Response {
    let account = match requester_account(&s, &headers).await {
        Some(a) => a,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                json_response(json!({
                    "error": "account_required",
                    "error_description": "注册 Agent 需要账户身份：登录控制台，或使用注册令牌（Authorization: Bearer）",
                })),
            )
                .into_response()
        }
    };
    match s.oauth.register(body.0, account.id).await {
        Ok(record) => (
            StatusCode::CREATED,
            json_response(json!({
                "agent_id": record.agent_id,
                "name": record.name,
                "kind": record.kind,
                "owner_account": record.owner_account_id,
                "confidential": record.confidential,
                "redirect_uris": record.redirect_uris,
                "max_scopes": record.max_scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            json_response(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// 解析请求者账户：会话 Cookie 优先，其次注册令牌
async fn requester_account(s: &AsWebState, headers: &HeaderMap) -> Option<AccountRecord> {
    if let Some(token) = cookie_token(headers) {
        if let Ok(Some(account)) = s.accounts.session_account(&token).await {
            return Some(account);
        }
    }
    if let Some(bearer) = bearer_token(headers) {
        if let Ok(account) = s.accounts.verify_registration_token(&bearer).await {
            return Some(account);
        }
    }
    None
}

/// GET /oauth/authorize：渲染同意页（需登录，且 Agent 必须属于本账户）
pub(super) async fn handle_authorize(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    let Some(account) = session_required(&s, &headers, "/oauth/authorize").await else {
        return login_redirect(&s);
    };

    let agent = match s.oauth.validate_authorize(&params).await {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                json_response(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };
    if agent.owner_account_id != account.id {
        return (
            StatusCode::FORBIDDEN,
            json_response(json!({ "error": "该 Agent 不属于当前账户" })),
        )
            .into_response();
    }

    let state_hidden = params.state.clone().unwrap_or_default();
    let requested = params
        .scope
        .clone()
        .unwrap_or_else(|| "memory:read memory:write".to_string());

    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>授权确认 · 记忆中枢</title>
<style>
  body {{ font-family: -apple-system, "PingFang SC", sans-serif; background: #f6f7f9; margin: 0; display: flex; justify-content: center; padding: 48px 16px; }}
  .card {{ background: #fff; border-radius: 12px; box-shadow: 0 2px 12px rgba(0,0,0,.08); padding: 32px; max-width: 420px; width: 100%; }}
  h1 {{ font-size: 20px; margin: 0 0 8px; }}
  p {{ color: #555; font-size: 14px; line-height: 1.6; }}
  .agent {{ background: #f0f4ff; border-radius: 8px; padding: 12px 16px; margin: 16px 0; font-size: 14px; }}
  label {{ display: flex; gap: 8px; align-items: center; font-size: 14px; margin: 16px 0; }}
  button {{ width: 100%; padding: 12px; border: 0; border-radius: 8px; background: #2563eb; color: #fff; font-size: 15px; cursor: pointer; }}
  button:hover {{ background: #1d4ed8; }}
  .muted {{ color: #888; font-size: 12px; margin-top: 12px; }}
</style>
</head>
<body>
<div class="card">
  <h1>授权确认</h1>
  <p>账户 <strong>{username}</strong> 正在为以下 Agent 授权访问长期记忆服务：</p>
  <div class="agent"><strong>{agent_id}</strong>（{name} · {kind}）</div>
  <form method="post" action="/oauth/consent/submit">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="agent_id" value="{agent_id}">
    <input type="hidden" name="scopes" value="{requested}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="code_challenge" value="{code_challenge}">
    <input type="hidden" name="state" value="{state}">
    <label><input type="checkbox" name="allow" value="true" checked> 允许访问本账户的记忆命名空间（范围：{requested}）</label>
    <button type="submit">同意并继续</button>
  </form>
  <p class="muted">仅授权请求的范围；可随时在控制台撤销。访问仅限本账户沙箱内的命名空间。</p>
</div>
</body>
</html>"#,
        username = esc(&account.username),
        agent_id = esc(&agent.agent_id),
        name = esc(&agent.name),
        kind = esc(&agent.kind),
        client_id = esc(&params.client_id),
        redirect_uri = esc(&params.redirect_uri),
        code_challenge = esc(&params.code_challenge),
        state = esc(&state_hidden),
        requested = esc(&requested),
    );

    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// POST /oauth/consent/submit：记录同意换发授权码
pub(super) async fn handle_consent(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }
    let Some(account) = session_required(&s, &headers, "/oauth/authorize").await else {
        return login_redirect(&s);
    };
    if !form.allowed() {
        return (
            StatusCode::BAD_REQUEST,
            json_response(json!({ "error": "用户未同意授权" })),
        )
            .into_response();
    }

    let agent = match s.oauth.get_agent(&form.agent_id).await {
        Ok(Some(a)) => a,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                json_response(json!({ "error": "agent 未注册" })),
            )
                .into_response()
        }
    };
    if agent.owner_account_id != account.id {
        return (
            StatusCode::FORBIDDEN,
            json_response(json!({ "error": "该 Agent 不属于当前账户" })),
        )
            .into_response();
    }

    // 重新构造授权参数（服务端以注册信息为准，不信表单自述）
    let params = AuthorizeParams {
        response_type: "code".to_string(),
        client_id: form.client_id.clone(),
        redirect_uri: form.redirect_uri.clone(),
        code_challenge: form.code_challenge.clone(),
        code_challenge_method: "S256".to_string(),
        state: Some(form.state.clone().unwrap_or_default()),
        scope: Some(form.scopes.clone()),
    };
    if s.oauth.validate_authorize(&params).await.is_err() {
        return (
            StatusCode::BAD_REQUEST,
            json_response(json!({ "error": "授权参数校验失败" })),
        )
            .into_response();
    }

    match s
        .oauth
        .issue_authorization_code(&params, &agent, form.scope_list())
        .await
    {
        Ok(code) => {
            let mut location = format!("{}?code={}", form.redirect_uri, urlencoded(&code));
            if let Some(st) = form.state.as_ref().filter(|x| !x.is_empty()) {
                location.push_str(&format!("&state={}", urlencoded(st)));
            }
            (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            json_response(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// POST /oauth/token
///
/// 未认证端点且每次验证都跑 64MiB 内存硬度的 Argon2id：必须双层限流，
/// 否则攻击者可用纯 HTTP 请求耗尽认证节点的 CPU/内存（DoS 入口）。
/// - 按客户端 IP：30/min（同一出口的正常重试不会触顶）
/// - 按 client_id：10/min（单个 Agent 的令牌轮换频率远低于此）
pub(super) async fn handle_token(
    State(s): State<AsWebState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<crate::oauth_flow::TokenRequest>,
) -> Response {
    let client_ip = extract_client_ip(&headers, remote, &s.trusted_proxies);
    // 键加端点前缀：限流桶按 dimension+key 存储，裸 IP 键会让 login(15/min) 与
    // token(30/min) 共用一个桶并互相钳制容量
    if !s
        .rate_limiter
        .try_acquire_in_window(
            common::RateDimension::Ip,
            &format!("token:{client_ip}"),
            TOKEN_IP_LIMIT_PER_MIN,
            60,
        )
        .await
        .unwrap_or(true)
    {
        return too_many_requests("请求过于频繁，请 1 分钟后重试");
    }

    // client_id 优先取 Basic 头，其次表单字段；都没有时按 "anonymous" 维度限流，
    // 防止不带 client_id 的请求绕开第二维
    let client_key = client_credentials(
        &headers,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    )
    .map(|(id, _)| id)
    .unwrap_or_else(|| "anonymous".to_string());
    if !s
        .rate_limiter
        .try_acquire_in_window(
            common::RateDimension::User,
            &format!("token-endpoint:{client_key}"),
            TOKEN_CLIENT_LIMIT_PER_MIN,
            60,
        )
        .await
        .unwrap_or(true)
    {
        return too_many_requests("该客户端令牌请求过于频繁，请 1 分钟后重试");
    }

    match s.oauth.token(form).await {
        Ok(resp) => json_response(serde_json::to_value(&resp).unwrap_or(json!({}))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            json_response(json!({
                "error": "invalid_request",
                "error_description": e.to_string(),
            })),
        )
            .into_response(),
    }
}

/// token 端点按 IP 的限流阈值（次/分钟）
const TOKEN_IP_LIMIT_PER_MIN: u32 = 30;
/// token 端点按 client_id 的限流阈值（次/分钟）
const TOKEN_CLIENT_LIMIT_PER_MIN: u32 = 10;
/// 注册端点按 IP 的限流阈值（次/分钟）
pub(super) const REGISTER_IP_LIMIT_PER_MIN: u32 = 5;

pub(super) fn too_many_requests(msg: &'static str) -> Response {
    (StatusCode::TOO_MANY_REQUESTS, msg).into_response()
}

/// 还原调用方客户端凭据：优先 HTTP Basic（RFC 7662 §2.1 推荐），其次表单字段。
fn client_credentials(
    headers: &HeaderMap,
    form_id: Option<&str>,
    form_secret: Option<&str>,
) -> Option<(String, String)> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(encoded) = value.strip_prefix("Basic ") {
            let decoded = STANDARD.decode(encoded.trim()).ok()?;
            let text = String::from_utf8(decoded).ok()?;
            let (id, secret) = text.split_once(':')?;
            return Some((id.to_string(), secret.to_string()));
        }
    }
    match (form_id, form_secret) {
        (Some(id), Some(secret)) => Some((id.to_string(), secret.to_string())),
        _ => None,
    }
}

/// 客户端认证失败：按 OAuth 错误格式回 401
fn unauthenticated_client() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, "Basic realm=\"oauth\""),
            (header::CONTENT_TYPE, "application/json"),
        ],
        serde_json::to_string(&json!({ "error": "invalid_client" })).unwrap_or_default(),
    )
        .into_response()
}

/// POST /oauth/introspect（RFC 7662）
///
/// 需客户端认证：否则任何持有令牌串的人都能确认其有效性并读出该 Agent
/// 的整份命名空间授权清单。
pub(super) async fn handle_introspect(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<IntrospectForm>,
) -> Response {
    let Some((client_id, secret)) = client_credentials(
        &headers,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    ) else {
        return unauthenticated_client();
    };
    if s.oauth
        .authenticate_client(&client_id, &secret)
        .await
        .is_err()
    {
        return unauthenticated_client();
    }

    let resp = s.oauth.introspect(&form.token).await.unwrap_or({
        crate::oauth_flow::IntrospectResponse {
            active: false,
            agent_id: None,
            client_id: None,
            scope: None,
            exp: None,
            grants: None,
        }
    });
    json_response(serde_json::to_value(&resp).unwrap_or(json!({ "active": false })))
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntrospectForm {
    pub token: String,
    /// RFC 7662 允许客户端附带提示；本服务按 token 自身解析，不做分支——
    /// 保留字段以维持线协议形状（未读取是刻意的）。
    #[serde(default)]
    #[allow(dead_code)]
    pub token_type_hint: Option<String>,
    /// 未使用 HTTP Basic 时以表单提交客户端凭据
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// POST /oauth/revoke（RFC 7009）
///
/// 需客户端认证，且只能撤销属于本客户端的令牌族；
/// 不属于调用方时按规范静默返回 200（不得用于探测他人令牌）。
pub(super) async fn handle_revoke(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<RevokeForm>,
) -> Response {
    let Some((client_id, secret)) = client_credentials(
        &headers,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    ) else {
        return unauthenticated_client();
    };
    let authenticated = match s.oauth.authenticate_client(&client_id, &secret).await {
        Ok(agent_id) => agent_id,
        Err(_) => return unauthenticated_client(),
    };
    s.oauth.revoke_for_client(&form.token, &authenticated);
    (StatusCode::OK, "").into_response()
}

#[derive(Debug, Clone, Deserialize)]
pub struct RevokeForm {
    pub token: String,
    /// RFC 7009 允许客户端附带提示；本服务按令牌族直接定位，忽略提示——
    /// 保留字段以维持线协议形状（未读取是刻意的）。
    #[serde(default)]
    #[allow(dead_code)]
    pub token_type_hint: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
}

// ===== 账户：注册 / 登录 / 登出 / 注册令牌 =====
