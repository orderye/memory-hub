use super::account::{extract_client_ip, LoginForm, RegisterAccountForm};
use super::console::{ConsoleAgentForm, ConsoleAgentStatusForm};
use super::oauth::{IntrospectForm, RevokeForm};
use super::*;

use crate::oauth_flow::RegisterAgentRequest;
use store::InMemoryHubStore;

/// 从 HeaderMap 取第一个值（测试辅助）
fn header_value(headers: &HeaderMap, key: &str) -> Option<String> {
    headers.get(key)?.to_str().ok().map(str::to_string)
}

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
        trusted_proxies: vec![],
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

/// 测试用的对端地址（无代理部署下的"真实客户端 IP"）
fn remote() -> SocketAddr {
    "203.0.113.10:4444".parse().unwrap()
}

/// 测试用的可信代理对端地址
fn proxy_remote() -> SocketAddr {
    "10.0.0.2:55555".parse().unwrap()
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
async fn test_root_redirects_to_console() {
    let resp = handle_root().await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(resp.headers()[header::LOCATION], "/console");
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
        ConnectInfo(remote()),
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
        ConnectInfo(remote()),
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
        ConnectInfo(remote()),
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
        ConnectInfo(remote()),
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
        ConnectInfo(remote()),
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
    let client_ip = extract_client_ip(&HeaderMap::new(), remote(), &[]);
    for _ in 0..15 {
        assert!(s
            .rate_limiter
            .try_acquire_in_window(common::RateDimension::Ip, &client_ip, 15, 60)
            .await
            .unwrap());
    }
    let rate_limited = handle_login_submit(
        State(s.clone()),
        ConnectInfo(remote()),
        HeaderMap::new(),
        Form(LoginForm {
            username: "bob".to_string(),
            password: "wrong".to_string(),
        }),
    )
    .await;
    assert_eq!(rate_limited.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// XFF 三路径：无可信代理忽略 XFF / 可信代理解析 XFF / 不可信来源忽略 XFF
#[test]
fn test_extract_client_ip_trust_paths() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-forwarded-for",
        "1.2.3.4, 10.0.0.2, 10.0.0.3".parse().unwrap(),
    );
    let proxies: Vec<std::net::IpAddr> =
        vec!["10.0.0.2".parse().unwrap(), "10.0.0.3".parse().unwrap()];

    // 1) 无可信代理：XFF 完全忽略，一律用 socket 对端地址
    assert_eq!(
        extract_client_ip(&headers, remote(), &[]),
        "203.0.113.10",
        "未配置可信代理时 XFF 必须被忽略"
    );

    // 2) 可信代理来源：从右往左跳过可信代理，取第一个非可信 IP（真实客户端）
    assert_eq!(
        extract_client_ip(&headers, proxy_remote(), &proxies),
        "1.2.3.4",
        "可信代理的 XFF 应取从右往左第一个非可信 IP"
    );

    // 3) 不可信来源：即使 XFF 存在也忽略，用对端地址
    //    （对端是 10.0.0.9，不在可信列表）
    let other: SocketAddr = "10.0.0.9:55555".parse().unwrap();
    assert_eq!(
        extract_client_ip(&headers, other, &proxies),
        "10.0.0.9",
        "不可信来源的 XFF 必须被忽略"
    );
}

/// 可信代理但 XFF 全是可信代理/条目不可解析时的兜底行为
#[test]
fn test_extract_client_ip_edge_cases() {
    let proxies: Vec<std::net::IpAddr> = vec!["10.0.0.2".parse().unwrap()];

    // XFF 全部为可信代理：回落到对端地址（此处对端即代理 10.0.0.2）
    let mut h1 = HeaderMap::new();
    h1.insert("x-forwarded-for", "10.0.0.2, 10.0.0.2".parse().unwrap());
    assert_eq!(extract_client_ip(&h1, proxy_remote(), &proxies), "10.0.0.2");

    // XFF 含无法解析的条目：不采信该条目，回落到对端地址
    let mut h2 = HeaderMap::new();
    h2.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
    assert_eq!(
        extract_client_ip(&h2, proxy_remote(), &proxies),
        "10.0.0.2",
        "不可解析的 XFF 条目不得进入限流键"
    );

    // 无 XFF 头：用对端地址
    assert_eq!(
        extract_client_ip(&HeaderMap::new(), remote(), &proxies),
        "203.0.113.10"
    );

    // X-Real-IP 不再被采信（旧实现的问题：单头同样可伪造）
    let mut h3 = HeaderMap::new();
    h3.insert("x-real-ip", "1.2.3.4".parse().unwrap());
    assert_eq!(extract_client_ip(&h3, remote(), &proxies), "203.0.113.10");
}

/// token 端点双层限流：IP 维度与 client_id 维度各自独立生效
#[tokio::test]
async fn test_token_endpoint_rate_limited_by_ip_and_client() {
    let mk_form = |cid: &str| {
        Form(crate::oauth_flow::TokenRequest {
            grant_type: "client_credentials".to_string(),
            code: None,
            code_verifier: None,
            redirect_uri: None,
            client_id: Some(cid.to_string()),
            client_secret: Some("x".to_string()),
            refresh_token: None,
            scope: None,
            resource: None,
        })
    };

    // 按 client_id 维度（10/min）：打空桶后同 client 的请求应 429，
    // 且此时 IP 桶余额充足——证明 429 来自 client 维度
    let s = state();
    let client_key = "token-endpoint:client-a";
    let mut drained = 0;
    while s
        .rate_limiter
        .try_acquire_in_window(common::RateDimension::User, client_key, 10, 60)
        .await
        .unwrap()
    {
        drained += 1;
        assert!(drained <= 10, "client 维度桶未能在 10 次内打空");
    }
    let resp = handle_token(
        State(s.clone()),
        ConnectInfo(remote()),
        HeaderMap::new(),
        mk_form("client-a"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // 换一个 client_id（IP 桶独立）立即可用——不会互相挤占
    let s2 = state();
    let ok_other = handle_token(
        State(s2),
        ConnectInfo(remote()),
        HeaderMap::new(),
        mk_form("client-b"),
    )
    .await;
    assert_ne!(ok_other.status(), StatusCode::TOO_MANY_REQUESTS);

    // 按 IP 维度（容量 30/分钟）：打空后任何 client 的请求都 429
    let s = state();
    let mut drained = 0;
    while s
        .rate_limiter
        .try_acquire_in_window(common::RateDimension::Ip, "token:203.0.113.10", 30, 60)
        .await
        .unwrap()
    {
        drained += 1;
        assert!(drained <= 30, "IP 维度桶未能在 30 次内打空");
    }
    let resp = handle_token(
        State(s),
        ConnectInfo(remote()),
        HeaderMap::new(),
        mk_form("client-c"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// register 端点按 IP 限流（5/min），开放注册与邀请码两种模式都限
#[tokio::test]
async fn test_register_endpoint_rate_limited_by_ip() {
    let mk_form = |invite: &str| {
        Form(RegisterAccountForm {
            username: "newbie".to_string(),
            password: "supersecret".to_string(),
            display_name: String::new(),
            invite_code: invite.to_string(),
        })
    };

    // 邀请码模式（注册被拒也照样扣令牌：判定在业务逻辑之前）
    let store = Arc::new(InMemoryHubStore::new());
    let s = build_state(store, Some("letmein"));
    let mut drained = 0;
    while s
        .rate_limiter
        .try_acquire_in_window(common::RateDimension::Ip, "register:203.0.113.10", 5, 60)
        .await
        .unwrap()
    {
        drained += 1;
        assert!(drained <= 5, "IP 桶未能在 5 次内打空");
    }
    let resp = handle_register_submit(
        State(s),
        ConnectInfo(remote()),
        HeaderMap::new(),
        mk_form("wrong"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // 开放注册模式同样被限
    let s = state();
    let mut drained = 0;
    while s
        .rate_limiter
        .try_acquire_in_window(common::RateDimension::Ip, "register:203.0.113.10", 5, 60)
        .await
        .unwrap()
    {
        drained += 1;
        assert!(drained <= 5);
    }
    let resp = handle_register_submit(
        State(s),
        ConnectInfo(remote()),
        HeaderMap::new(),
        mk_form(""),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}
