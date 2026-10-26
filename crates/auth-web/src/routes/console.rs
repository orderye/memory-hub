use super::*;

/// GET /：根路径直接送到账户控制台（未登录时控制台再跳登录页）
pub(super) async fn handle_root() -> Response {
    redirect("/console")
}

// ===== 控制台 =====

/// GET /console：账户控制台（需登录；只展示本账户数据）
pub(super) async fn handle_console(State(s): State<AsWebState>, headers: HeaderMap) -> Response {
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
pub(super) async fn handle_console_register_agent(
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
pub(super) async fn handle_console_agent_status(
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
pub(super) async fn handle_console_grant_revoke(
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
pub(super) async fn handle_console_account_status(
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
