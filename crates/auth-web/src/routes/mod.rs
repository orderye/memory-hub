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
//! | GET | `/` | 入口页：重定向到 `/console` |
//! | GET | `/console` | 账户控制台（本账户 Agent / 命名空间 / 授权） |
//! | POST | `/console/agents` | 注册 Agent 到本账户 |
//! | POST | `/console/agents/status` | 停用/恢复 Agent |
//! | POST | `/console/grants/revoke` | 撤销命名空间授权 |
//! | POST | `/console/accounts/status` | 停用/恢复账户（仅引导账户） |
//! | GET | `/healthz` `/readyz` | 探针 |

use crate::account::{AccountService, SESSION_COOKIE};
use crate::oauth_flow::{AgentOAuthServer, AuthorizeParams};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Form;
use axum::Router;
use mem_core::{AccountRecord, GrantStore};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
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
    /// 可信反向代理（`TRUSTED_PROXIES`）：仅当 TCP 对端命中时才解析 XFF。
    /// 空列表 = 无代理部署，一律用 socket 对端地址做 IP 限流。
    pub trusted_proxies: Vec<std::net::IpAddr>,
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
        // 入口页：预览页/直接访问根路径时送到控制台；未登录由控制台跳登录页
        .route("/", get(handle_root))
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
mod account;
mod console;
mod oauth;

use account::{
    handle_account_token, handle_login_page, handle_login_submit, handle_logout,
    handle_register_page, handle_register_submit,
};
use console::{
    handle_console, handle_console_account_status, handle_console_agent_status,
    handle_console_grant_revoke, handle_console_register_agent, handle_root,
};
use oauth::{
    handle_authorization_server_metadata, handle_authorize, handle_consent, handle_introspect,
    handle_protected_resource_metadata, handle_register, handle_revoke, handle_token,
};
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

#[cfg(test)]
mod tests;
