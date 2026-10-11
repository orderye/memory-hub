//! 授权服务器与 Agent 控制台 HTTP 路由（auth-web）
//!
//! 端点清单：
//! | 方法 | 路径 | 说明 |
//! | :--- | :--- | :--- |
//! | GET | `/.well-known/oauth-authorization-server` | RFC 8414 授权服务器元数据 |
//! | GET | `/.well-known/oauth-protected-resource` | RFC 9728 受保护资源元数据 |
//! | POST | `/oauth/register` | 动态客户端注册（RFC 7591） |
//! | GET | `/oauth/authorize` | 授权码 + PKCE 同意页 |
//! | POST | `/oauth/consent/submit` | 记录同意换发授权码，302 回 client |
//! | POST | `/oauth/token` | client_credentials / 授权码 / 刷新令牌 |
//! | POST | `/oauth/introspect` | 令牌内省（RFC 7662） |
//! | POST | `/oauth/revoke` | 撤销刷新令牌（RFC 7009） |
//! | GET | `/console` | Agent 控制台（注册 Agent 与命名空间授权一览） |
//! | GET | `/healthz` `/readyz` | 探针 |

use crate::oauth_flow::{AgentOAuthServer, AuthorizeParams};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Form;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 路由共享状态
#[derive(Clone)]
pub struct AsWebState {
    pub oauth: Arc<AgentOAuthServer>,
    /// 本服务对外基准地址（用于元数据与重定向拼装）
    pub external_base_url: String,
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
        .route("/console", get(handle_console))
        .route("/healthz", axum::routing::get(handle_healthz))
        .route("/readyz", axum::routing::get(handle_healthz))
        .with_state(state)
}

async fn handle_healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

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

/// POST /oauth/register（DCR）
async fn handle_register(
    State(s): State<AsWebState>,
    body: axum::extract::Json<crate::oauth_flow::RegisterAgentRequest>,
) -> Response {
    match s.oauth.register(body.0).await {
        Ok(record) => (
            StatusCode::CREATED,
            json_response(json!({
                "agent_id": record.agent_id,
                "name": record.name,
                "kind": record.kind,
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

/// GET /oauth/authorize：渲染同意页
async fn handle_authorize(
    State(s): State<AsWebState>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
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
  <p>以下 Agent 正在申请访问长期记忆服务：</p>
  <div class="agent"><strong>{agent_id}</strong>（{name} · {kind}）</div>
  <form method="post" action="/oauth/consent/submit">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="agent_id" value="{agent_id}">
    <input type="hidden" name="scopes" value="{requested}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="code_challenge" value="{code_challenge}">
    <input type="hidden" name="state" value="{state}">
    <label><input type="checkbox" name="allow" value="true" checked> 允许访问我的记忆命名空间（范围：{requested}）</label>
    <button type="submit">同意并继续</button>
  </form>
  <p class="muted">仅授权请求的范围；可随时在控制台撤销。</p>
</div>
</body>
</html>"#,
        agent_id = agent.agent_id,
        name = agent.name,
        kind = agent.kind,
        client_id = params.client_id,
        redirect_uri = params.redirect_uri,
        code_challenge = params.code_challenge,
        state = state_hidden,
        requested = requested,
    );

    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// POST /oauth/consent/submit：记录同意换发授权码
async fn handle_consent(State(s): State<AsWebState>, Form(form): Form<ConsentForm>) -> Response {
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
        .issue_authorization_code(&params, &agent, form.scope_list(), None)
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

/// POST /oauth/introspect（RFC 7662）
async fn handle_introspect(
    State(s): State<AsWebState>,
    Form(form): Form<IntrospectForm>,
) -> Response {
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
}

/// POST /oauth/revoke（RFC 7009）
async fn handle_revoke(State(s): State<AsWebState>, Form(form): Form<RevokeForm>) -> Response {
    s.oauth.revoke(&form.token);
    (StatusCode::OK, "").into_response()
}

#[derive(Debug, Clone, Deserialize)]
pub struct RevokeForm {
    pub token: String,
    #[serde(default)]
    pub token_type_hint: Option<String>,
}

/// GET /console：Agent 控制台（只读展示注册 Agent 与授权）
async fn handle_console(State(s): State<AsWebState>) -> Response {
    let agents = s.oauth.list_agents().await.unwrap_or_default();
    let mut rows = String::new();
    for a in &agents {
        rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            a.agent_id,
            a.name,
            a.kind,
            if a.confidential { "机密" } else { "公共" },
            a.status,
            a.max_scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "),
        ));
    }
    if rows.is_empty() {
        rows.push_str(r#"<tr><td colspan="6" style="color:#888">尚未注册任何 Agent（POST /oauth/register）</td></tr>"#);
    }

    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Agent 控制台 · 记忆中枢</title>
<style>
  body {{ font-family: -apple-system, "PingFang SC", sans-serif; background: #f6f7f9; margin: 0; padding: 32px 16px; }}
  .wrap {{ max-width: 880px; margin: 0 auto; }}
  h1 {{ font-size: 22px; }}
  table {{ width: 100%; border-collapse: collapse; background: #fff; border-radius: 12px; overflow: hidden; box-shadow: 0 2px 12px rgba(0,0,0,.06); }}
  th, td {{ padding: 12px 16px; text-align: left; font-size: 14px; border-bottom: 1px solid #eee; }}
  th {{ background: #fafafa; color: #555; font-weight: 600; }}
  code {{ background: #f0f0f0; padding: 2px 6px; border-radius: 4px; font-size: 13px; }}
  .hint {{ color: #666; font-size: 14px; line-height: 1.7; margin: 16px 0 24px; }}
</style>
</head>
<body>
<div class="wrap">
  <h1>Agent 控制台</h1>
  <p class="hint">多智能体长期记忆服务。每个 Agent 以自己的身份访问记忆命名空间；命名空间首次写入者成为 owner，可通过 <code>memory_grant</code> 分享给其他 Agent。</p>
  <table>
    <thead><tr><th>Agent ID</th><th>名称</th><th>类型</th><th>客户端</th><th>状态</th><th>权限上限</th></tr></thead>
    <tbody>{rows}</tbody>
  </table>
</div>
</body>
</html>"#,
    );

    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
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
        AsWebState {
            oauth: Arc::new(AgentOAuthServer::new(
                store,
                "test-secret",
                "https://auth.example.com",
                "https://hub.example.com",
                3600,
                120,
                30,
            )),
            external_base_url: "https://auth.example.com".to_string(),
        }
    }

    #[tokio::test]
    async fn test_register_endpoint_accepts_and_returns_record() {
        let s = state();
        let req = RegisterAgentRequest {
            agent_id: "agent-a".to_string(),
            name: "Agent A".to_string(),
            kind: "service".to_string(),
            client_secret: Some("secret".to_string()),
            redirect_uris: vec!["https://app.example.com/cb".to_string()],
            max_scopes: None,
        };
        let resp = handle_register(State(s), axum::extract::Json(req)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[test]
    fn test_urlencoded() {
        assert_eq!(urlencoded("abc-_.~"), "abc-_.~");
        assert_eq!(urlencoded("a b/c"), "a%20b%2Fc");
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
    async fn test_console_renders_agents() {
        let s = state();
        let resp = handle_console(State(s)).await;
        assert_eq!(resp.status(), StatusCode::OK);
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
}
