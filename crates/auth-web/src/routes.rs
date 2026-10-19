//! 授权服务器、账户页面与账户控制台 HTTP 路由（auth-web）
//!
//! 端点清单：
//! | 方法 | 路径 | 说明 |
//! | :--- | :--- | :--- |
//! | GET | `/.well-known/oauth-authorization-server` | RFC 8414 授权服务器元数据 |
//! | GET | `/.well-known/oauth-protected-resource` | RFC 9728 受保护资源元数据 |
//! | POST | `/oauth/register` | 动态客户端注册（RFC 7591，需会话或注册令牌） |
//! | GET | `/oauth/authorize` | 授权码 + PKCE 同意页（需登录；仅限本账户 Agent） |
//! | POST | `/oauth/consent/submit` | 记录同意换发授权码，302 回 client |
//! | POST | `/oauth/token` | client_credentials / 授权码 / 刷新令牌 |
//! | POST | `/oauth/introspect` | 令牌内省（RFC 7662，需客户端认证） |
//! | POST | `/oauth/revoke` | 撤销刷新令牌（RFC 7009，需客户端认证） |
//! | GET | `/account/register` `POST` | 账户注册（可选邀请码） |
//! | GET | `/account/login` `POST` | 账户登录（建立会话 Cookie） |
//! | POST | `/account/logout` | 退出登录 |
//! | POST | `/account/token` | 签发注册令牌（DCR 的 initial access token） |
//! | GET | `/console` | 账户控制台（本账户 Agent / 命名空间 / 授权） |
//! | POST | `/console/agents` | 注册 Agent 到本账户 |
//! | POST | `/console/agents/status` | 停用/恢复 Agent |
//! | POST | `/console/grants/revoke` | 撤销命名空间授权 |
//! | POST | `/console/accounts/status` | 停用/恢复账户（仅引导账户） |
//! | GET | `/healthz` `/readyz` | 探针 |

use crate::account::{AccountService, SESSION_COOKIE};
use crate::oauth_flow::{AgentOAuthServer, AuthorizeParams};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Form;
use axum::Router;
use mem_core::{AccountRecord, GrantStore};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 路由共享状态
#[derive(Clone)]
pub struct AsWebState {
    pub oauth: Arc<AgentOAuthServer>,
    pub accounts: Arc<AccountService>,
    /// 本服务对外基准地址（用于元数据与重定向拼装）
    pub external_base_url: String,
    /// Origin 白名单（非空时校验浏览器侧状态变更请求，防跨站请求伪造）
    pub allowed_origins: Vec<String>,
    pub rate_limiter: Arc<dyn common::RateLimitStore>,
}

/// 同意页提交参数（表单）
#[derive(Debug, Clone, Deserialize)]
pub struct ConsentForm {
    pub client_id: String,
    pub agent_id: String,
    /// 空格分隔 scope 串
    #[serde(default)]
    pub scopes: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    #[serde(default)]
    pub state: Option<String>,
    /// 复选框语义：字段存在且不为 false 即放行
    #[serde(default)]
    pub allow: Option<String>,
}

impl ConsentForm {
    pub fn allowed(&self) -> bool {
        match self.allow.as_deref() {
            Some(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "off" | "no" | ""
            ),
            None => false,
        }
    }

    pub fn scope_list(&self) -> Vec<common::Scope> {
        crate::oauth_flow::parse_scopes(&self.scopes)
    }
}

/// Token 端点表单（axum Form 直接反序列化）
pub use crate::oauth_flow::TokenRequest as TokenForm;

/// 构造授权服务器 axum 路由
pub fn oauth_router(state: AsWebState) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(handle_authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(handle_protected_resource_metadata),
        )
        .route("/oauth/register", post(handle_register))
        .route("/oauth/authorize", get(handle_authorize))
        .route("/oauth/consent/submit", post(handle_consent))
        .route("/oauth/token", post(handle_token))
        .route("/oauth/introspect", post(handle_introspect))
        .route("/oauth/revoke", post(handle_revoke))
        .route(
            "/account/register",
            get(handle_register_page).post(handle_register_submit),
        )
        .route(
            "/account/login",
            get(handle_login_page).post(handle_login_submit),
        )
        .route("/account/logout", post(handle_logout))
        .route("/account/token", post(handle_account_token))
        .route("/console", get(handle_console))
        .route("/console/agents", post(handle_console_register_agent))
        .route("/console/agents/status", post(handle_console_agent_status))
        .route("/console/grants/revoke", post(handle_console_grant_revoke))
        .route(
            "/console/accounts/status",
            post(handle_console_account_status),
        )
        .route("/healthz", axum::routing::get(handle_healthz))
        .route("/readyz", axum::routing::get(handle_readyz))
        // 与 MCP 服务同一套传输层卫生：体积上限在缓冲前生效、单请求硬超时、访问追踪。
        // 授权服务器是公网可达端点，此前这三项都没有。
        .layer(axum::extract::DefaultBodyLimit::max(MAX_AUTH_BODY_BYTES))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(AUTH_REQUEST_TIMEOUT_SECS),
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// 授权服务器请求体上限（表单与 JSON 都很小，64KB 已宽）
const MAX_AUTH_BODY_BYTES: usize = 64 * 1024;
/// 单个请求的硬超时（秒）
const AUTH_REQUEST_TIMEOUT_SECS: u64 = 30;

async fn handle_healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

async fn handle_readyz(State(s): State<AsWebState>) -> Response {
    match s.oauth.store().ping().await {
        Ok(()) => (StatusCode::OK, "ready").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Auth 就绪探针失败：存储不可用");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable").into_response()
        }
    }
}

// ===== 元数据 =====

/// RFC 8414 授权服务器元数据
async fn handle_authorization_server_metadata(State(s): State<AsWebState>) -> Response {
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
async fn handle_protected_resource_metadata(State(s): State<AsWebState>) -> Response {
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
async fn handle_register(
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
async fn handle_authorize(
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
async fn handle_consent(
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
async fn handle_token(
    State(s): State<AsWebState>,
    Form(form): Form<crate::oauth_flow::TokenRequest>,
) -> Response {
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
async fn handle_introspect(
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
    #[serde(default)]
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
async fn handle_revoke(
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
    #[serde(default)]
    pub token_type_hint: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
}

// ===== 账户：注册 / 登录 / 登出 / 注册令牌 =====

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterAccountForm {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub invite_code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

/// GET /account/register
async fn handle_register_page(State(s): State<AsWebState>) -> Response {
    let invite_field = if s.accounts.registration_open() {
        String::new()
    } else {
        r#"<label>邀请码<input name="invite_code" type="text" autocomplete="off" required></label>"#
            .to_string()
    };
    page(
        "注册账户 · 记忆中枢",
        &format!(
            r#"<div class="card">
  <h1>注册账户</h1>
  <p class="muted">账户是记忆沙箱：你的 Agent 与命名空间仅在本账户内可见。</p>
  <form method="post" action="/account/register">
    <label>用户名<input name="username" type="text" minlength="3" maxlength="32" pattern="[A-Za-z0-9_-]+" required autofocus></label>
    <label>口令<input name="password" type="password" minlength="8" maxlength="128" required></label>
    <label>展示名（可选）<input name="display_name" type="text" maxlength="64"></label>
    {invite_field}
    <button type="submit">注册并登录</button>
  </form>
  <p class="muted">已有账户？<a href="/account/login">去登录</a></p>
</div>"#
        ),
    )
}

/// POST /account/register
async fn handle_register_submit(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<RegisterAccountForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }
    let invite = (!form.invite_code.trim().is_empty()).then_some(form.invite_code.as_str());
    match s
        .accounts
        .register(&form.username, &form.password, &form.display_name, invite)
        .await
    {
        Ok((_account, session)) => with_session_cookie(&s, redirect("/console"), &session),
        Err(e) => page(
            "注册失败 · 记忆中枢",
            &format!(
                r#"<div class="card">
  <h1>注册失败</h1>
  <p class="err">{}</p>
  <p class="muted"><a href="/account/register">返回重试</a></p>
</div>"#,
                esc(&e.to_string())
            ),
        ),
    }
}

/// GET /account/login
async fn handle_login_page(State(s): State<AsWebState>) -> Response {
    let register_link = if s.accounts.registration_open() {
        r#"<p class="muted">还没有账户？<a href="/account/register">注册</a></p>"#
    } else {
        ""
    };
    page(
        "登录 · 记忆中枢",
        &format!(
            r#"<div class="card">
  <h1>登录</h1>
  <p class="muted">登录后管理本账户的 Agent、命名空间与授权。</p>
  <form method="post" action="/account/login">
    <label>用户名<input name="username" type="text" required autofocus></label>
    <label>口令<input name="password" type="password" required></label>
    <button type="submit">登录</button>
  </form>
  {register_link}
</div>"#
        ),
    )
}

fn extract_client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// POST /account/login
async fn handle_login_submit(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }

    let client_ip = extract_client_ip(&headers);
    // 按 IP 限制每分钟 15 次
    if !s
        .rate_limiter
        .try_acquire_in_window(common::RateDimension::Ip, &client_ip, 15, 60)
        .await
        .unwrap_or(true)
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "请求过于频繁，请 1 分钟后重试",
        )
            .into_response();
    }

    // 按用户名限制每分钟 5 次
    if !s
        .rate_limiter
        .try_acquire_in_window(common::RateDimension::User, form.username.trim(), 5, 60)
        .await
        .unwrap_or(true)
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "该账户登录尝试过于频繁，请 1 分钟后重试",
        )
            .into_response();
    }

    match s.accounts.login(&form.username, &form.password).await {
        Ok((_account, session)) => with_session_cookie(&s, redirect("/console"), &session),
        Err(e) => page(
            "登录失败 · 记忆中枢",
            &format!(
                r#"<div class="card">
  <h1>登录失败</h1>
  <p class="err">{}</p>
  <p class="muted"><a href="/account/login">返回重试</a></p>
</div>"#,
                esc(&e.to_string())
            ),
        ),
    }
}

/// POST /account/logout
async fn handle_logout(State(s): State<AsWebState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_token(&headers) {
        let _ = s.accounts.logout(&token).await;
    }
    with_cookie(redirect("/account/login"), clear_session_cookie(&s))
}

/// POST /account/token：签发注册令牌（供 MCP 客户端 DCR 使用）
async fn handle_account_token(State(s): State<AsWebState>, headers: HeaderMap) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }
    let Some(account) = session_required(&s, &headers, "/account/token").await else {
        return login_redirect(&s);
    };
    match s.accounts.issue_registration_token(&account).await {
        Ok(token) => {
            if wants_json(&headers) {
                return json_response(json!({
                    "access_token": token,
                    "token_type": "Bearer",
                    "expires_in": crate::account::REGISTRATION_TOKEN_TTL_S,
                    "usage": "POST /oauth/register 时作为 Authorization: Bearer 使用",
                }));
            }
            page(
                "注册令牌 · 记忆中枢",
                &format!(
                    r#"<div class="card wide">
  <h1>注册令牌</h1>
  <p class="muted">有效期 {} 秒。供 MCP 客户端调用 <code>POST /oauth/register</code> 时作为 initial access token：</p>
  <pre class="token">{token}</pre>
  <p class="muted">用法：<code>Authorization: Bearer &lt;令牌&gt;</code>。令牌仅绑定本账户，注册出的 Agent 自动归属本账户。</p>
  <p class="muted"><a href="/console">返回控制台</a></p>
</div>"#,
                    crate::account::REGISTRATION_TOKEN_TTL_S,
                    token = esc(&token)
                ),
            )
        }
        Err(e) => page(
            "签发失败 · 记忆中枢",
            &format!(
                r#"<div class="card"><h1>签发失败</h1><p class="err">{}</p><p class="muted"><a href="/console">返回控制台</a></p></div>"#,
                esc(&e.to_string())
            ),
        ),
    }
}

// ===== 控制台 =====

/// GET /console：账户控制台（需登录；只展示本账户数据）
async fn handle_console(State(s): State<AsWebState>, headers: HeaderMap) -> Response {
    let Some(account) = session_required(&s, &headers, "/console").await else {
        return login_redirect(&s);
    };
    let store = s.oauth.store();
    let scoped = mem_core::AccountScopedStore::new(store.clone(), account.id);

    // 本账户 Agent
    let agents: Vec<_> = store
        .list_agents()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.owner_account_id == account.id)
        .collect();
    let mut agent_rows = String::new();
    for a in &agents {
        let next_status = if a.status == "active" {
            "suspended"
        } else {
            "active"
        };
        let action = if a.status == "active" {
            "停用"
        } else {
            "恢复"
        };
        agent_rows.push_str(&format!(
            r#"<tr><td><code>{id}</code></td><td>{name}</td><td>{kind}</td><td>{client}</td><td>{status}</td><td>{scopes}</td>
<td><form method="post" action="/console/agents/status"><input type="hidden" name="agent_id" value="{id}"><input type="hidden" name="status" value="{next_status}"><button class="mini">{action}</button></form></td></tr>"#,
            id = esc(&a.agent_id),
            name = esc(&a.name),
            kind = esc(&a.kind),
            client = if a.confidential { "机密" } else { "公共" },
            status = esc(&a.status),
            scopes = esc(&a.max_scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")),
            next_status = next_status,
            action = action,
        ));
    }
    if agent_rows.is_empty() {
        agent_rows.push_str(
            r#"<tr><td colspan="7" style="color:#888">尚未注册 Agent：可用下方表单注册，或让 MCP 客户端携带注册令牌走 POST /oauth/register</td></tr>"#,
        );
    }

    // 命名空间与授权（账户作用域）
    let namespaces = scoped.list_namespace_infos().await.unwrap_or_default();
    let mut ns_rows = String::new();
    let mut grant_rows = String::new();
    for ns in &namespaces {
        ns_rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            esc(&ns.namespace),
            esc(&ns.owner_agent),
            ns.created_at.format("%Y-%m-%d %H:%M")
        ));
    }
    if ns_rows.is_empty() {
        ns_rows.push_str(r#"<tr><td colspan="3" style="color:#888">本账户还没有命名空间（首次写入记忆时自动创建）</td></tr>"#);
    }
    // 授权明细：逐个本账户 Agent 拉取（作用域存储只返回本账户前缀的行）
    for a in &agents {
        for g in scoped.grants_for(&a.agent_id).await.unwrap_or_default() {
            grant_rows.push_str(&format!(
                r#"<tr><td><code>{ns}</code></td><td><code>{agent}</code></td><td>{rw}</td><td><code>{by}</code></td>
<td><form method="post" action="/console/grants/revoke"><input type="hidden" name="namespace" value="{ns}"><input type="hidden" name="agent_id" value="{agent}"><button class="mini">撤销</button></form></td></tr>"#,
                ns = esc(&g.namespace),
                agent = esc(&g.agent_id),
                rw = match (g.can_read, g.can_write) {
                    (true, true) => "读写",
                    (true, false) => "只读",
                    (false, true) => "只写",
                    (false, false) => "-",
                },
                by = esc(&g.granted_by),
            ));
        }
    }
    if grant_rows.is_empty() {
        grant_rows.push_str(r#"<tr><td colspan="5" style="color:#888">暂无授权记录</td></tr>"#);
    }

    // 引导账户：额外展示账户管理
    let admin_section = if account.id == common::BOOTSTRAP_ACCOUNT_ID {
        let accounts = s.accounts.list_accounts().await.unwrap_or_default();
        let mut rows = String::new();
        for a in &accounts {
            let next = if a.status == "active" {
                "suspended"
            } else {
                "active"
            };
            let action = if a.status == "active" {
                "停用"
            } else {
                "恢复"
            };
            rows.push_str(&format!(
                r#"<tr><td><code>{id}</code></td><td>{username}</td><td>{name}</td><td>{status}</td><td>{pwd}</td>
<td><form method="post" action="/console/accounts/status"><input type="hidden" name="account_id" value="{id}"><input type="hidden" name="status" value="{next}"><button class="mini">{action}</button></form></td></tr>"#,
                id = a.id,
                username = esc(&a.username),
                name = esc(&a.display_name),
                status = esc(&a.status),
                pwd = if a.has_password { "已设" } else { "未设" },
                next = next,
                action = action,
            ));
        }
        format!(
            r#"<h2>账户管理（引导账户）</h2>
  <table><thead><tr><th>ID</th><th>用户名</th><th>展示名</th><th>状态</th><th>口令</th><th>操作</th></tr></thead><tbody>{rows}</tbody></table>"#
        )
    } else {
        String::new()
    };

    page(
        "控制台 · 记忆中枢",
        &format!(
            r#"<h1>控制台</h1>
  <p class="muted">账户 <strong>{username}</strong>（<code>{account_id}</code>）· 沙箱边界：命名空间与 Agent 仅在本账户内可见
    <form method="post" action="/account/logout" class="inline"><button class="mini">退出登录</button></form>
  </p>

  <h2>注册 Agent</h2>
  <form method="post" action="/console/agents" class="grid">
    <label>Agent ID<input name="agent_id" type="text" maxlength="128" required placeholder="my-agent"></label>
    <label>名称<input name="name" type="text" maxlength="128" required placeholder="我的助手"></label>
    <label>类型<select name="kind"><option value="service">service</option><option value="assistant">assistant</option><option value="worker">worker</option></select></label>
    <label>机密客户端<input type="checkbox" name="confidential" value="true" checked> <span class="muted">勾选后签发 client_secret（仅显示一次）</span></label>
    <label>重定向 URI（可选，逗号分隔）<input name="redirect_uris" type="text" placeholder="https://app.example.com/cb"></label>
    <button type="submit">注册 Agent</button>
  </form>
  <p class="muted">程序化注册（MCP 客户端自动 DCR）：<form method="post" action="/account/token" class="inline"><button class="mini">生成注册令牌</button></form></p>

  <h2>我的 Agent</h2>
  <table><thead><tr><th>Agent ID</th><th>名称</th><th>类型</th><th>客户端</th><th>状态</th><th>权限上限</th><th>操作</th></tr></thead><tbody>{agent_rows}</tbody></table>

  <h2>命名空间（本账户沙箱）</h2>
  <table><thead><tr><th>命名空间</th><th>Owner</th><th>创建时间</th></tr></thead><tbody>{ns_rows}</tbody></table>

  <h2>授权</h2>
  <table><thead><tr><th>命名空间</th><th>Agent</th><th>权限</th><th>授予人</th><th>操作</th></tr></thead><tbody>{grant_rows}</tbody></table>

  {admin_section}"#,
            username = esc(&account.username),
            account_id = account.id,
        ),
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConsoleAgentForm {
    pub agent_id: String,
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub confidential: Option<String>,
    #[serde(default)]
    pub redirect_uris: String,
}

/// POST /console/agents：注册 Agent 到本账户
async fn handle_console_register_agent(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<ConsoleAgentForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }
    let Some(account) = session_required(&s, &headers, "/console").await else {
        return login_redirect(&s);
    };

    let confidential = form.confidential.as_deref().is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "false" | "0" | "off" | ""
        )
    });
    // 控制台注册默认生成密钥（控件实现"显示一次"）
    let secret = confidential.then(|| format!("hub_{}", crate::account::new_session_token()));
    let redirect_uris: Vec<String> = form
        .redirect_uris
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let req = crate::oauth_flow::RegisterAgentRequest {
        agent_id: form.agent_id.clone(),
        name: form.name.clone(),
        kind: if form.kind.trim().is_empty() {
            "service".to_string()
        } else {
            form.kind.clone()
        },
        client_secret: secret.clone(),
        redirect_uris,
        max_scopes: None,
    };
    match s.oauth.register(req, account.id).await {
        Ok(record) => {
            let secret_block = match &secret {
                Some(sec) => format!(
                    r#"<p class="muted">client_secret（仅此一次显示，请立即保存）：</p><pre class="token">{}</pre>"#,
                    esc(sec)
                ),
                None => "<p class=\"muted\">公共客户端：只能走 PKCE 授权码流程。</p>".to_string(),
            };
            page(
                "Agent 已注册 · 记忆中枢",
                &format!(
                    r#"<div class="card wide">
  <h1>Agent 已注册</h1>
  <p><code>{id}</code>（{name} · {kind}）已归属账户 <strong>{owner}</strong>。</p>
  {secret_block}
  <p class="muted"><a href="/console">返回控制台</a></p>
</div>"#,
                    id = esc(&record.agent_id),
                    name = esc(&record.name),
                    kind = esc(&record.kind),
                    owner = esc(&account.username),
                ),
            )
        }
        Err(e) => page(
            "注册失败 · 记忆中枢",
            &format!(
                r#"<div class="card"><h1>注册失败</h1><p class="err">{}</p><p class="muted"><a href="/console">返回控制台</a></p></div>"#,
                esc(&e.to_string())
            ),
        ),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConsoleAgentStatusForm {
    pub agent_id: String,
    pub status: String,
}

/// POST /console/agents/status：停用/恢复本账户 Agent
async fn handle_console_agent_status(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<ConsoleAgentStatusForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }
    let Some(account) = session_required(&s, &headers, "/console").await else {
        return login_redirect(&s);
    };
    let store = s.oauth.store();
    match store.get_agent(&form.agent_id).await {
        Ok(Some(agent)) if agent.owner_account_id == account.id => {
            if !matches!(form.status.as_str(), "active" | "suspended") {
                return (StatusCode::BAD_REQUEST, "status 非法").into_response();
            }
            if let Err(e) = store.set_agent_status(&form.agent_id, &form.status).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
            }
            redirect("/console")
        }
        _ => (StatusCode::FORBIDDEN, "Agent 不属于当前账户").into_response(),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConsoleRevokeForm {
    pub namespace: String,
    pub agent_id: String,
}

/// POST /console/grants/revoke：撤销本账户内的命名空间授权
async fn handle_console_grant_revoke(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<ConsoleRevokeForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }
    let Some(account) = session_required(&s, &headers, "/console").await else {
        return login_redirect(&s);
    };
    let store = s.oauth.store();
    // 目标 Agent 必须属于本账户（作用域内的授权行不会命中其他账户）
    match store.get_agent(&form.agent_id).await {
        Ok(Some(agent)) if agent.owner_account_id == account.id => {}
        _ => return (StatusCode::FORBIDDEN, "Agent 不属于当前账户").into_response(),
    }
    let scoped = mem_core::AccountScopedStore::new(store, account.id);
    match scoped.revoke(&form.namespace, &form.agent_id).await {
        Ok(()) => redirect("/console"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConsoleAccountStatusForm {
    pub account_id: uuid::Uuid,
    pub status: String,
}

/// POST /console/accounts/status：停用/恢复账户（仅引导账户）
async fn handle_console_account_status(
    State(s): State<AsWebState>,
    headers: HeaderMap,
    Form(form): Form<ConsoleAccountStatusForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }
    let Some(account) = session_required(&s, &headers, "/console").await else {
        return login_redirect(&s);
    };
    if account.id != common::BOOTSTRAP_ACCOUNT_ID {
        return (StatusCode::FORBIDDEN, "仅引导账户可管理账户状态").into_response();
    }
    if form.account_id == common::BOOTSTRAP_ACCOUNT_ID {
        return (StatusCode::BAD_REQUEST, "不能停用引导账户").into_response();
    }
    match s
        .accounts
        .set_account_status(&form.account_id, &form.status)
        .await
    {
        Ok(()) => redirect("/console"),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ===== 会话与响应辅助 =====

/// 取会话账户；未登录返回 None（调用方决定重定向或 401）
async fn session_required(
    s: &AsWebState,
    headers: &HeaderMap,
    _endpoint: &str,
) -> Option<AccountRecord> {
    let token = cookie_token(headers)?;
    s.accounts.session_account(&token).await.ok().flatten()
}

/// 未登录 → 302 到登录页
fn login_redirect(_s: &AsWebState) -> Response {
    redirect("/account/login")
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k.trim() == SESSION_COOKIE).then(|| v.trim().to_string())
    })
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Origin 白名单校验（对齐 mcp-server：仅在配置了白名单且请求携带 Origin 时生效）
fn origin_allowed(s: &AsWebState, headers: &HeaderMap) -> bool {
    if s.allowed_origins.is_empty() {
        return true;
    }
    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        Some(origin) => s.allowed_origins.iter().any(|o| o == origin),
        None => true,
    }
}

/// Cookie 是否应带 Secure（外部地址为 https 时）
fn cookie_secure(s: &AsWebState) -> bool {
    s.external_base_url.starts_with("https://")
}

fn with_session_cookie(s: &AsWebState, resp: Response, token: &str) -> Response {
    with_cookie(resp, session_cookie(s, token))
}

fn session_cookie(s: &AsWebState, token: &str) -> String {
    format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        SESSION_COOKIE,
        token,
        s.accounts.session_ttl_secs(),
        if cookie_secure(s) { "; Secure" } else { "" }
    )
}

fn clear_session_cookie(s: &AsWebState) -> String {
    format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        SESSION_COOKIE,
        if cookie_secure(s) { "; Secure" } else { "" }
    )
}

fn with_cookie(resp: Response, cookie: String) -> Response {
    let (mut parts, body) = resp.into_parts();
    if let Ok(value) = cookie.parse() {
        parts.headers.append(header::SET_COOKIE, value);
    }
    Response::from_parts(parts, body)
}

fn redirect(to: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}

fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"))
}

/// 统一页面外壳（内置样式；所有动态内容调用方必须 `esc` 转义）
fn page(title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  body {{ font-family: -apple-system, "PingFang SC", sans-serif; background: #f6f7f9; margin: 0; padding: 32px 16px; color: #1a1a1a; }}
  .wrap, .card {{ max-width: 420px; margin: 0 auto; }}
  .wrap {{ max-width: 980px; }}
  .card {{ background: #fff; border-radius: 12px; box-shadow: 0 2px 12px rgba(0,0,0,.08); padding: 32px; }}
  .card.wide {{ max-width: 980px; }}
  h1 {{ font-size: 22px; margin: 0 0 8px; }}
  h2 {{ font-size: 16px; margin: 28px 0 10px; color: #333; }}
  p {{ color: #555; font-size: 14px; line-height: 1.7; }}
  label {{ display: block; font-size: 14px; margin: 12px 0; color: #333; }}
  input[type=text], input[type=password], select {{ width: 100%; padding: 10px; margin-top: 6px; border: 1px solid #ddd; border-radius: 8px; font-size: 14px; box-sizing: border-box; }}
  button {{ padding: 10px 16px; border: 0; border-radius: 8px; background: #2563eb; color: #fff; font-size: 14px; cursor: pointer; }}
  button:hover {{ background: #1d4ed8; }}
  button.mini {{ padding: 4px 10px; font-size: 12px; }}
  table {{ width: 100%; border-collapse: collapse; background: #fff; border-radius: 12px; overflow: hidden; box-shadow: 0 2px 12px rgba(0,0,0,.06); }}
  th, td {{ padding: 10px 14px; text-align: left; font-size: 13px; border-bottom: 1px solid #eee; }}
  th {{ background: #fafafa; color: #555; font-weight: 600; }}
  code {{ background: #f0f0f0; padding: 2px 6px; border-radius: 4px; font-size: 13px; }}
  pre.token {{ background: #0f172a; color: #e2e8f0; padding: 14px; border-radius: 8px; overflow-x: auto; font-size: 13px; }}
  .muted {{ color: #888; font-size: 13px; }}
  .err {{ color: #b91c1c; font-size: 14px; }}
  form.inline {{ display: inline; }}
  form.grid {{ background: #fff; border-radius: 12px; box-shadow: 0 2px 12px rgba(0,0,0,.06); padding: 16px 20px; max-width: 640px; }}
  .wrap {{ max-width: 980px; margin: 0 auto; }}
</style>
</head>
<body>
<div class="wrap">{body}</div>
</body>
</html>"#,
        title = esc(title),
        body = body
    );
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        html,
    )
        .into_response()
}

/// HTML 转义（所有用户可控内容进入页面前必须经过）
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// JSON 响应辅助
fn json_response(v: serde_json::Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&v).unwrap_or_default(),
    )
        .into_response()
}

/// 查询参数值的简易 percent-encoding（code/state 回传安全）
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 从 HeaderMap 取第一个值（测试辅助）
#[allow(dead_code)]
pub fn header_value(headers: &HeaderMap, key: &str) -> Option<String> {
    headers.get(key)?.to_str().ok().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth_flow::RegisterAgentRequest;
    use store::InMemoryHubStore;

    fn state() -> AsWebState {
        let store = Arc::new(InMemoryHubStore::new());
        build_state(store, None)
    }

    fn build_state(store: Arc<InMemoryHubStore>, invite: Option<&str>) -> AsWebState {
        AsWebState {
            oauth: Arc::new(AgentOAuthServer::new(
                store.clone(),
                "test-secret",
                "https://auth.example.com",
                "https://hub.example.com",
                3600,
                120,
                30,
            )),
            accounts: Arc::new(AccountService::new(
                store,
                invite.map(str::to_string),
                12,
                "test-secret",
                "https://auth.example.com",
                "https://auth.example.com/oauth/register",
            )),
            external_base_url: "https://auth.example.com".to_string(),
            allowed_origins: vec![],
            rate_limiter: Arc::new(common::InMemoryRateLimitStore::new()),
        }
    }

    fn headers_with_cookie(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            format!("{}={}", SESSION_COOKIE, token).parse().unwrap(),
        );
        h
    }

    async fn register_and_login(s: &AsWebState, username: &str) -> String {
        let (_, session) = s
            .accounts
            .register(username, "supersecret", "", None)
            .await
            .unwrap();
        session
    }

    #[tokio::test]
    async fn test_introspect_and_revoke_require_client_auth() {
        let s = state();
        let introspect = handle_introspect(
            State(s.clone()),
            HeaderMap::new(),
            Form(IntrospectForm {
                token: "whatever".to_string(),
                token_type_hint: None,
                client_id: None,
                client_secret: None,
            }),
        )
        .await;
        assert_eq!(
            introspect.status(),
            StatusCode::UNAUTHORIZED,
            "无客户端凭据的内省请求必须被拒（RFC 7662 §2.1）"
        );

        let revoke = handle_revoke(
            State(s),
            HeaderMap::new(),
            Form(RevokeForm {
                token: "whatever".to_string(),
                token_type_hint: None,
                client_id: None,
                client_secret: None,
            }),
        )
        .await;
        assert_eq!(
            revoke.status(),
            StatusCode::UNAUTHORIZED,
            "无客户端凭据的撤销请求必须被拒（RFC 7009 §2.1）"
        );
    }

    #[tokio::test]
    async fn test_introspect_accepts_basic_credentials() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;

        let store = Arc::new(InMemoryHubStore::new());
        let s = build_state(store, None);
        s.oauth
            .register(
                RegisterAgentRequest {
                    agent_id: "agent-a".to_string(),
                    name: "Agent A".to_string(),
                    kind: "service".to_string(),
                    client_secret: Some("s3cret".to_string()),
                    redirect_uris: vec!["https://app.example.com/cb".to_string()],
                    max_scopes: None,
                },
                common::BOOTSTRAP_ACCOUNT_ID,
            )
            .await
            .unwrap();

        let mut headers = HeaderMap::new();
        let encoded = STANDARD.encode("agent-a:s3cret".as_bytes());
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", encoded).parse().unwrap(),
        );

        let resp = handle_introspect(
            State(s),
            headers,
            Form(IntrospectForm {
                token: "not-a-real-token".to_string(),
                token_type_hint: None,
                client_id: None,
                client_secret: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "Basic 凭据应通过客户端认证");
    }

    #[tokio::test]
    async fn test_register_endpoint_requires_account() {
        let s = state();
        let req = RegisterAgentRequest {
            agent_id: "agent-a".to_string(),
            name: "Agent A".to_string(),
            kind: "service".to_string(),
            client_secret: Some("secret".to_string()),
            redirect_uris: vec!["https://app.example.com/cb".to_string()],
            max_scopes: None,
        };
        // 匿名 DCR：拒绝
        let resp = handle_register(
            State(s.clone()),
            HeaderMap::new(),
            axum::extract::Json(req.clone()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_register_endpoint_with_session_and_token() {
        let s = state();
        let session = register_and_login(&s, "alice").await;
        let req = RegisterAgentRequest {
            agent_id: "agent-a".to_string(),
            name: "Agent A".to_string(),
            kind: "service".to_string(),
            client_secret: Some("secret".to_string()),
            redirect_uris: vec!["https://app.example.com/cb".to_string()],
            max_scopes: None,
        };
        let resp = handle_register(
            State(s.clone()),
            headers_with_cookie(&session),
            axum::extract::Json(req.clone()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        // 归属账户写入 Agent 记录
        let agent = s.oauth.get_agent("agent-a").await.unwrap().unwrap();
        let account = s.accounts.session_account(&session).await.unwrap().unwrap();
        assert_eq!(agent.owner_account_id, account.id);

        // 注册令牌路径（无会话）
        let token = s.accounts.issue_registration_token(&account).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", token).parse().unwrap(),
        );
        let mut req2 = req.clone();
        req2.agent_id = "agent-b".to_string();
        let resp2 = handle_register(State(s), headers, axum::extract::Json(req2)).await;
        assert_eq!(resp2.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_console_requires_login_and_scopes_data() {
        let s = state();
        // 未登录 → 302 登录页
        let anon = handle_console(State(s.clone()), HeaderMap::new()).await;
        assert_eq!(anon.status(), StatusCode::SEE_OTHER);

        // 两个账户各自注册 Agent
        let s1 = register_and_login(&s, "user-one").await;
        let s2 = register_and_login(&s, "user-two").await;
        for (session, id) in [(&s1, "a-one"), (&s2, "a-two")] {
            let resp = handle_console_register_agent(
                State(s.clone()),
                headers_with_cookie(session),
                Form(ConsoleAgentForm {
                    agent_id: id.to_string(),
                    name: id.to_string(),
                    kind: "service".to_string(),
                    confidential: None,
                    redirect_uris: String::new(),
                }),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // u1 的控制台只显示 a-one
        let page = handle_console(State(s.clone()), headers_with_cookie(&s1)).await;
        assert_eq!(page.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(page.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("a-one"), "本账户 Agent 必须可见");
        assert!(!html.contains("a-two"), "跨账户 Agent 不得出现");
    }

    #[tokio::test]
    async fn test_agent_status_cross_account_forbidden() {
        let s = state();
        let s1 = register_and_login(&s, "user-one").await;
        let s2 = register_and_login(&s, "user-two").await;
        handle_console_register_agent(
            State(s.clone()),
            headers_with_cookie(&s2),
            Form(ConsoleAgentForm {
                agent_id: "owned-by-u2".to_string(),
                name: "x".to_string(),
                kind: "service".to_string(),
                confidential: None,
                redirect_uris: String::new(),
            }),
        )
        .await;

        // u1 尝试停用 u2 的 Agent → 403
        let resp = handle_console_agent_status(
            State(s),
            headers_with_cookie(&s1),
            Form(ConsoleAgentStatusForm {
                agent_id: "owned-by-u2".to_string(),
                status: "suspended".to_string(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_account_pages_render() {
        let s = state();
        assert_eq!(
            handle_login_page(State(s.clone())).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            handle_register_page(State(s.clone())).await.status(),
            StatusCode::OK
        );

        // 注册提交 → 303 + Set-Cookie
        let resp = handle_register_submit(
            State(s.clone()),
            HeaderMap::new(),
            Form(RegisterAccountForm {
                username: "newbie".to_string(),
                password: "supersecret".to_string(),
                display_name: "新人".to_string(),
                invite_code: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(resp.headers().contains_key(header::SET_COOKIE));

        // 登录失败 → 200 + 错误页
        let bad = handle_login_submit(
            State(s),
            HeaderMap::new(),
            Form(LoginForm {
                username: "newbie".to_string(),
                password: "wrong-pass".to_string(),
            }),
        )
        .await;
        assert_eq!(bad.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_invite_code_required_when_configured() {
        let store = Arc::new(InMemoryHubStore::new());
        let s = build_state(store, Some("letmein"));
        let resp = handle_register_submit(
            State(s),
            HeaderMap::new(),
            Form(RegisterAccountForm {
                username: "newbie".to_string(),
                password: "supersecret".to_string(),
                display_name: String::new(),
                invite_code: "wrong".to_string(),
            }),
        )
        .await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("邀请码不正确"));
    }

    #[tokio::test]
    async fn test_account_token_requires_login() {
        let s = state();
        let anon = handle_account_token(State(s.clone()), HeaderMap::new()).await;
        assert_eq!(anon.status(), StatusCode::SEE_OTHER);

        let session = register_and_login(&s, "alice").await;
        let resp = handle_account_token(State(s), headers_with_cookie(&session)).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_origin_check() {
        let mut s = state();
        assert!(origin_allowed(&s, &HeaderMap::new()), "未配置白名单：放行");
        s.allowed_origins = vec!["https://console.example.com".to_string()];
        assert!(origin_allowed(&s, &HeaderMap::new()), "无 Origin：放行");
        let mut h = HeaderMap::new();
        h.insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
        assert!(!origin_allowed(&s, &h));
        h.insert(
            header::ORIGIN,
            "https://console.example.com".parse().unwrap(),
        );
        assert!(origin_allowed(&s, &h));
    }

    #[test]
    fn test_urlencoded() {
        assert_eq!(urlencoded("abc-_.~"), "abc-_.~");
        assert_eq!(urlencoded("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn test_esc_escapes_html() {
        assert_eq!(
            esc(r#"<script>"x"&'y'"#),
            "&lt;script&gt;&quot;x&quot;&amp;&#39;y&#39;"
        );
    }

    #[test]
    fn test_consent_form_parsing() {
        let allow = ConsentForm {
            client_id: "c".to_string(),
            agent_id: "a".to_string(),
            scopes: "memory:read".to_string(),
            redirect_uri: "https://x.example.com/cb".to_string(),
            code_challenge: "ch".to_string(),
            state: None,
            allow: Some("true".to_string()),
        };
        assert!(allow.allowed());
        assert_eq!(allow.scope_list(), vec![common::Scope::Read]);

        let deny = ConsentForm {
            allow: Some("off".to_string()),
            ..allow.clone()
        };
        assert!(!deny.allowed());
    }

    #[tokio::test]
    async fn test_metadata_endpoints() {
        let s = state();
        let resp = handle_authorization_server_metadata(State(s.clone())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp2 = handle_protected_resource_metadata(State(s)).await;
        assert_eq!(resp2.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_healthz() {
        let resp = handle_healthz().await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_readyz() {
        let s = state();
        let resp = handle_readyz(State(s)).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_header_helper() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        assert_eq!(
            header_value(&headers, "content-type").as_deref(),
            Some("application/json")
        );
        assert!(header_value(&headers, "x-missing").is_none());
    }

    #[tokio::test]
    async fn test_login_rate_limiting_username_and_ip() {
        // 限流是令牌桶，按 capacity/window 连续回补（此处用户维度为每 12 秒补 1 个令牌）。
        // 因此不能用「连续 5 次真实错密码登录把桶打空、第 6 次必然限流」来构造用例：
        // debug 构建下每次 Argon2 校验都很慢，5 次尝试的耗时可超过回补一个令牌所需的时间，
        // 第 6 次就会被放行。改为直接打空对应维度的桶，再断言路由确实返回 429。

        // 单次失败登录仍返回 200（保留原有的行为断言）
        let s = state();
        let failed = handle_login_submit(
            State(s.clone()),
            HeaderMap::new(),
            Form(LoginForm {
                username: "bob".to_string(),
                password: "wrong".to_string(),
            }),
        )
        .await;
        assert_eq!(failed.status(), StatusCode::OK);

        // 按用户名维度（容量 5/60s）：打空剩余令牌后，同用户名的登录应被限流
        let mut drained = 0;
        while s
            .rate_limiter
            .try_acquire_in_window(common::RateDimension::User, "bob", 5, 60)
            .await
            .unwrap()
        {
            drained += 1;
            assert!(drained <= 5, "按用户名的令牌桶未能在 5 次内打空");
        }
        let rate_limited = handle_login_submit(
            State(s.clone()),
            HeaderMap::new(),
            Form(LoginForm {
                username: "bob".to_string(),
                password: "wrong".to_string(),
            }),
        )
        .await;
        assert_eq!(rate_limited.status(), StatusCode::TOO_MANY_REQUESTS);

        // 按客户端 IP 维度（容量 15/60s）：此时用户名桶仍有余额，被限流即说明 IP 维度生效
        let s = state();
        let client_ip = extract_client_ip(&HeaderMap::new());
        for _ in 0..15 {
            assert!(
                s.rate_limiter
                    .try_acquire_in_window(common::RateDimension::Ip, &client_ip, 15, 60)
                    .await
                    .unwrap()
            );
        }
        let rate_limited = handle_login_submit(
            State(s.clone()),
            HeaderMap::new(),
            Form(LoginForm {
                username: "bob".to_string(),
                password: "wrong".to_string(),
            }),
        )
        .await;
        assert_eq!(rate_limited.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
