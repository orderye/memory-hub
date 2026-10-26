use super::oauth::{too_many_requests, REGISTER_IP_LIMIT_PER_MIN};
use super::*;

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
pub(super) async fn handle_register_page(State(s): State<AsWebState>) -> Response {
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
///
/// 开放注册与邀请码两种模式都按 IP 限流（5/min）：注册会跑 Argon2 哈希
/// 并写库，未认证可达；不限制时既是 DoS 入口也是批量注册垃圾账户的入口。
pub(super) async fn handle_register_submit(
    State(s): State<AsWebState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<RegisterAccountForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }
    let client_ip = extract_client_ip(&headers, remote, &s.trusted_proxies);
    // 键加端点前缀（与 token 端点同理：避免与 login 的 IP 桶互相钳制容量）
    if !s
        .rate_limiter
        .try_acquire_in_window(
            common::RateDimension::Ip,
            &format!("register:{client_ip}"),
            REGISTER_IP_LIMIT_PER_MIN,
            60,
        )
        .await
        .unwrap_or(true)
    {
        return too_many_requests("注册请求过于频繁，请 1 分钟后重试");
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
pub(super) async fn handle_login_page(State(s): State<AsWebState>) -> Response {
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

/// 解析客户端 IP（用于 IP 维度限流）。
///
/// **默认只信任 TCP 对端地址，完全忽略 `X-Forwarded-For` / `X-Real-IP`**：
/// 这两个头是任意客户端可写的，直接采信等于让攻击者每请求换一个随机 XFF
/// 就能绕过 IP 限流。
///
/// 仅当对端地址命中 `TRUSTED_PROXIES`（自家反向代理）时才解析 XFF，且取
/// **从右往左第一个不属于可信代理的 IP**——XFF 由链路上的代理逐跳追加，
/// 右侧是离本服务最近的代理写入的可信部分，左侧才是客户端可伪造的部分。
pub(super) fn extract_client_ip(
    headers: &HeaderMap,
    remote: SocketAddr,
    trusted_proxies: &[std::net::IpAddr],
) -> String {
    if !trusted_proxies.contains(&remote.ip()) {
        return remote.ip().to_string();
    }

    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()));
    let mut hops: Vec<&str> = match forwarded {
        Some(h) => h.collect(),
        None => return remote.ip().to_string(),
    };

    // 从右往左跳过可信代理；第一个非可信 IP 即客户端真实地址。
    // 条目无法解析时视为不可信：采信它会污染限流键，直接回落到对端地址。
    // XFF 全部为可信代理时（客户端即代理本身），同样回落到对端地址。
    while let Some(last) = hops.last().copied() {
        match last.parse::<std::net::IpAddr>() {
            Ok(ip) if trusted_proxies.contains(&ip) => {
                hops.pop();
            }
            Ok(ip) => return ip.to_string(),
            Err(_) => return remote.ip().to_string(),
        }
    }
    remote.ip().to_string()
}

/// POST /account/login
pub(super) async fn handle_login_submit(
    State(s): State<AsWebState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !origin_allowed(&s, &headers) {
        return (StatusCode::FORBIDDEN, "Origin 不被允许").into_response();
    }

    let client_ip = extract_client_ip(&headers, remote, &s.trusted_proxies);
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
pub(super) async fn handle_logout(State(s): State<AsWebState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_token(&headers) {
        let _ = s.accounts.logout(&token).await;
    }
    with_cookie(redirect("/account/login"), clear_session_cookie(&s))
}

/// POST /account/token：签发注册令牌（供 MCP 客户端 DCR 使用）
pub(super) async fn handle_account_token(
    State(s): State<AsWebState>,
    headers: HeaderMap,
) -> Response {
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

// ===== 入口页 =====
