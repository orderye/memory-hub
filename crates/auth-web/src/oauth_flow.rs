//! 多智能体长期记忆服务授权服务器编排层（auth-web）
//!
//! 把 `auth-core` 的安全原语（PKCE S256、Token Family 轮换、HS256 访问令牌）
//! 与 `AgentStore` / `GrantStore` 编排成完整的 OAuth 2.1 授权服务器：
//!
//! 1. `POST /oauth/register`    动态客户端注册（RFC 7591）：Agent 自助注册
//! 2. `GET  /oauth/authorize`   授权码 + PKCE（Agent 控制台 / 代为行动场景）
//! 3. `POST /oauth/token`      三种授予：`client_credentials`（机器 Agent）、
//!
//!    `authorization_code`（带 PKCE）、`refresh_token`（轮换）
//! 4. `POST /oauth/introspect`  RFC 7662 令牌内省
//! 5. `POST /oauth/revoke`      RFC 7009 令牌撤销
//!
//! 授权码刷新令牌默认存内存（单副本授权服务器）；生产多副本可替换存储实现。

use auth_core::{
    compute_code_challenge_s256, AccessTokenManager, ConfirmTokenManager, TokenFamilyManager,
};
use chrono::{DateTime, Duration, Utc};
use common::{AgentId, HubError, NamespaceGrant, Result, Scope, UserId};
use mem_core::{AgentRecord, MemoryHubStore, NamespaceGrantRecord};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// 授权码有效期（OAuth 2.1 建议 ≤ 10 分钟）
const AUTH_CODE_TTL_SECONDS: i64 = 300;

/// 动态注册请求（RFC 7591 风格）
#[derive(Debug, Clone, Deserialize)]
pub struct RegisterAgentRequest {
    pub agent_id: String,
    pub name: String,
    /// service | assistant | worker
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 机密客户端密钥（不提供则为公共客户端，只能走 PKCE 授权码）
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    /// 申请的权限上限（默认 read+write）
    #[serde(default)]
    pub max_scopes: Option<Vec<String>>,
}

fn default_kind() -> String {
    "service".to_string()
}

/// 令牌响应（RFC 6749 §5.1）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub scope: String,
}

/// 令牌端点请求（form-urlencoded）
#[derive(Debug, Clone, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub code_verifier: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    /// RFC 8707 资源指示符
    #[serde(default)]
    pub resource: Option<String>,
}

/// 授权码记录
#[derive(Debug, Clone)]
struct AuthorizationCode {
    agent_id: AgentId,
    redirect_uri: String,
    code_challenge: String,
    scopes: Vec<Scope>,
    grants: Vec<NamespaceGrant>,
    expires_at: DateTime<Utc>,
    used: bool,
}

/// 授权请求参数（`GET /oauth/authorize` 查询串）
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizeParams {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// 内省响应（RFC 7662）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntrospectResponse {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grants: Option<Vec<NamespaceGrant>>,
}

/// 授权服务器（编排层）
pub struct AgentOAuthServer {
    store: Arc<dyn MemoryHubStore>,
    tokens: AccessTokenManager,
    families: TokenFamilyManager,
    confirm: ConfirmTokenManager,
    auth_codes: RwLock<HashMap<String, AuthorizationCode>>,
    /// 授权码换取结果缓存（refresh 令牌由 families 管理）
    audience: String,
    issuer: String,
}

impl AgentOAuthServer {
    pub fn new(
        store: Arc<dyn MemoryHubStore>,
        secret: impl Into<String>,
        issuer: impl Into<String>,
        audience: impl Into<String>,
        access_token_ttl_s: i64,
        confirm_token_ttl_s: u64,
        refresh_token_ttl_days: i64,
    ) -> Self {
        let issuer: String = issuer.into();
        let audience: String = audience.into();
        Self {
            store,
            tokens: AccessTokenManager::new(
                secret,
                issuer.clone(),
                audience.clone(),
                access_token_ttl_s,
            ),
            families: TokenFamilyManager::new(refresh_token_ttl_days),
            confirm: ConfirmTokenManager::new(confirm_token_ttl_s as i64),
            auth_codes: RwLock::new(HashMap::new()),
            audience,
            issuer,
        }
    }

    /// 令牌管理器引用（供内省接口复用）
    pub fn token_manager(&self) -> &AccessTokenManager {
        &self.tokens
    }

    /// 确认令牌管理器引用
    pub fn confirm_manager(&self) -> &ConfirmTokenManager {
        &self.confirm
    }

    /// 签发方标识
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// 受众（资源 URI）
    pub fn audience(&self) -> &str {
        &self.audience
    }

    // ===== 动态注册（RFC 7591）=====

    pub async fn register(&self, req: RegisterAgentRequest) -> Result<AgentRecord> {
        if req.agent_id.trim().is_empty() || req.agent_id.len() > 128 {
            return Err(HubError::InvalidArgument(
                "agent_id 长度必须在 1..=128".to_string(),
            ));
        }
        if req.name.trim().is_empty() {
            return Err(HubError::InvalidArgument("name 不得为空".to_string()));
        }
        if !matches!(req.kind.as_str(), "service" | "assistant" | "worker") {
            return Err(HubError::InvalidArgument(
                "kind 只支持 service | assistant | worker".to_string(),
            ));
        }
        if req.client_secret.as_deref().is_some_and(str::is_empty) {
            return Err(HubError::InvalidArgument(
                "client_secret 不得为空字符串".to_string(),
            ));
        }
        for uri in &req.redirect_uris {
            if !(uri.starts_with("https://") || uri.starts_with("http://localhost")) {
                return Err(HubError::InvalidArgument(format!(
                    "redirect_uri 必须为 https（或 localhost 开发地址）: {}",
                    uri
                )));
            }
        }

        let max_scopes = match &req.max_scopes {
            Some(list) => {
                let mut out = Vec::new();
                for s in list {
                    let scope = Scope::parse(s)
                        .ok_or_else(|| HubError::InvalidArgument(format!("未知 scope: {}", s)))?;
                    out.push(scope);
                }
                out
            }
            None => vec![Scope::Read, Scope::Write],
        };
        if max_scopes.is_empty() {
            return Err(HubError::InvalidArgument(
                "max_scopes 不得为空集合".to_string(),
            ));
        }

        self.store
            .register_agent(&mem_core::AgentRegistration {
                agent_id: req.agent_id.clone(),
                name: req.name.clone(),
                kind: req.kind.clone(),
                client_secret: req.client_secret.clone(),
                redirect_uris: req.redirect_uris.clone(),
                max_scopes,
            })
            .await
    }

    pub async fn get_agent(&self, agent_id: &str) -> Result<Option<AgentRecord>> {
        self.store.get_agent(&agent_id.to_string()).await
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        self.store.list_agents().await
    }

    // ===== 令牌端点 =====

    /// 令牌端点（三种授予）
    pub async fn token(&self, req: TokenRequest) -> Result<TokenResponse> {
        match req.grant_type.as_str() {
            "client_credentials" => self.client_credentials(req).await,
            "authorization_code" => self.authorization_code(req).await,
            "refresh_token" => self.refresh_token(req).await,
            other => Err(HubError::InvalidArgument(format!(
                "不支持的 grant_type: {}",
                other
            ))),
        }
    }

    /// client_credentials：机器 Agent 直接换取访问令牌
    async fn client_credentials(&self, req: TokenRequest) -> Result<TokenResponse> {
        let client_id = req
            .client_id
            .clone()
            .ok_or_else(|| HubError::InvalidArgument("缺少 client_id".to_string()))?;
        let secret = req
            .client_secret
            .clone()
            .ok_or_else(|| HubError::InvalidArgument("缺少 client_secret".to_string()))?;

        let agent = self
            .store
            .get_agent(&client_id)
            .await?
            .ok_or(HubError::AuthRequired)?;
        if agent.status != "active" {
            return Err(HubError::AuthRequired);
        }
        // 机密客户端才允许此授予；公共客户端无密钥可校验
        if !self.store.verify_agent_secret(&client_id, &secret).await? {
            return Err(HubError::AuthRequired);
        }

        let scopes = self.resolve_scopes(req.scope.as_deref(), &agent)?;
        let grants = self.resolve_grants(&client_id).await?;

        let access = self
            .tokens
            .issue(&client_id.clone(), scopes.clone(), grants)?;
        Ok(TokenResponse {
            access_token: access,
            token_type: "Bearer",
            expires_in: self.tokens.ttl_seconds,
            refresh_token: None, // client_credentials 不应当有刷新令牌
            scope: join_scopes(&scopes),
        })
    }

    /// authorization_code：授权码 + PKCE 换令牌
    async fn authorization_code(&self, req: TokenRequest) -> Result<TokenResponse> {
        let code = req
            .code
            .clone()
            .ok_or_else(|| HubError::InvalidArgument("缺少 code".to_string()))?;
        let verifier = req
            .code_verifier
            .clone()
            .ok_or_else(|| HubError::InvalidArgument("缺少 code_verifier".to_string()))?;
        let redirect_uri = req
            .redirect_uri
            .clone()
            .ok_or_else(|| HubError::InvalidArgument("缺少 redirect_uri".to_string()))?;

        // 取出并立即标记已用（一次性）
        let record = {
            let mut w = self.auth_codes.write().unwrap();
            let rec = w.get_mut(&code).ok_or(HubError::AuthRequired)?;
            if rec.used || rec.expires_at <= Utc::now() {
                return Err(HubError::AuthRequired);
            }
            if rec.redirect_uri != redirect_uri {
                return Err(HubError::InvalidArgument(
                    "redirect_uri 与授权时不一致".to_string(),
                ));
            }
            rec.used = true;
            rec.clone()
        };

        // PKCE S256 强校验
        let computed = compute_code_challenge_s256(&verifier);
        if computed != record.code_challenge {
            return Err(HubError::AuthRequired);
        }

        let access = self.tokens.issue(
            &record.agent_id,
            record.scopes.clone(),
            record.grants.clone(),
        )?;
        let refresh = self.families.issue_initial(
            &record.agent_id,
            record.scopes.clone(),
            record.grants.clone(),
        );

        Ok(TokenResponse {
            access_token: access,
            token_type: "Bearer",
            expires_in: self.tokens.ttl_seconds,
            refresh_token: Some(refresh),
            scope: join_scopes(&record.scopes),
        })
    }

    /// refresh_token：强制轮换 + 重放检测
    async fn refresh_token(&self, req: TokenRequest) -> Result<TokenResponse> {
        let token = req
            .refresh_token
            .clone()
            .ok_or_else(|| HubError::InvalidArgument("缺少 refresh_token".to_string()))?;

        let rotated = self.families.rotate(&token)?;
        let access = self.tokens.issue(
            &rotated.agent_id,
            rotated.scopes.clone(),
            rotated.grants.clone(),
        )?;
        Ok(TokenResponse {
            access_token: access,
            token_type: "Bearer",
            expires_in: self.tokens.ttl_seconds,
            refresh_token: Some(rotated.refresh_token),
            scope: join_scopes(&rotated.scopes),
        })
    }

    // ===== 授权码流程（同意页）=====

    /// 校验授权请求参数；返回客户端信息供渲染同意页
    pub async fn validate_authorize(&self, params: &AuthorizeParams) -> Result<AgentRecord> {
        if params.response_type != "code" {
            return Err(HubError::InvalidArgument(
                "仅支持 response_type=code".to_string(),
            ));
        }
        if params.code_challenge_method != "S256" {
            return Err(HubError::InvalidArgument("PKCE 必须使用 S256".to_string()));
        }
        let agent = self
            .store
            .get_agent(&params.client_id)
            .await?
            .ok_or(HubError::InvalidArgument("client_id 未注册".to_string()))?;
        if agent.status != "active" {
            return Err(HubError::InvalidArgument("客户端已被停用".to_string()));
        }
        if !agent.redirect_uris.contains(&params.redirect_uri) {
            return Err(HubError::InvalidArgument(
                "redirect_uri 未在客户端注册列表中".to_string(),
            ));
        }
        Ok(agent)
    }

    /// 记录同意并换发授权码
    ///
    /// `user_id` 由调用方从会话显式传入（Agent 代终端用户行动时携带）。
    pub async fn issue_authorization_code(
        &self,
        params: &AuthorizeParams,
        agent: &AgentRecord,
        scopes: Vec<Scope>,
        user_id: Option<UserId>,
    ) -> Result<String> {
        let _ = user_id; // 主体为 Agent；用户信息记住在 consent 侧
                         // scope 不得超出客户端注册上限
        for s in &scopes {
            if !agent.max_scopes.contains(s) {
                return Err(HubError::InsufficientScope);
            }
        }
        let code = format!("ac_{}", Uuid::new_v4().simple());
        let grants = self.resolve_grants(&agent.agent_id).await?;
        self.auth_codes.write().unwrap().insert(
            code.clone(),
            AuthorizationCode {
                agent_id: agent.agent_id.clone(),
                redirect_uri: params.redirect_uri.clone(),
                code_challenge: params.code_challenge.clone(),
                scopes,
                grants,
                expires_at: Utc::now() + Duration::seconds(AUTH_CODE_TTL_SECONDS),
                used: false,
            },
        );
        Ok(code)
    }

    // ===== 内省与撤销 =====

    pub async fn introspect(&self, token: &str) -> Result<IntrospectResponse> {
        match self.tokens.verify(token) {
            Ok(claims) => Ok(IntrospectResponse {
                active: true,
                agent_id: Some(claims.sub.clone()),
                client_id: Some(claims.client_id),
                scope: Some(join_scopes(&claims.scopes)),
                exp: Some(claims.exp),
                grants: Some(claims.grants),
            }),
            Err(_) => Ok(IntrospectResponse {
                active: false,
                agent_id: None,
                client_id: None,
                scope: None,
                exp: None,
                grants: None,
            }),
        }
    }

    /// 撤销（refresh 令牌整族吊销；access 令牌由 TTL 自然过期）
    pub fn revoke(&self, token: &str) {
        self.families.revoke_family(token);
    }

    // ===== 辅助 =====

    /// 解析请求 scope 字符串并裁剪到客户端上限
    fn resolve_scopes(&self, requested: Option<&str>, agent: &AgentRecord) -> Result<Vec<Scope>> {
        let list: Vec<Scope> = match requested {
            Some(s) => {
                let mut out = Vec::new();
                for part in s.split_whitespace() {
                    let scope = Scope::parse(part).ok_or_else(|| {
                        HubError::InvalidArgument(format!("未知 scope: {}", part))
                    })?;
                    out.push(scope);
                }
                out
            }
            None => vec![Scope::Read, Scope::Write],
        };
        if list.is_empty() {
            return Err(HubError::InvalidArgument("scope 不得为空".to_string()));
        }
        for s in &list {
            if !agent.max_scopes.contains(s) {
                return Err(HubError::InsufficientScope);
            }
        }
        Ok(list)
    }

    /// 从 GrantStore 解析 Agent 当前生效的命名空间授权
    async fn resolve_grants(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrant>> {
        let records: Vec<NamespaceGrantRecord> = self.store.grants_for(agent_id).await?;
        Ok(records
            .into_iter()
            .map(|r| NamespaceGrant {
                namespace: r.namespace,
                read: r.can_read,
                write: r.can_write,
            })
            .collect())
    }
}

/// scope 列表 → 空格分隔串
pub fn join_scopes(scopes: &[Scope]) -> String {
    scopes
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 解析 scope 字符串为列表（公开辅助）
pub fn parse_scopes(s: &str) -> Vec<Scope> {
    s.split_whitespace().filter_map(Scope::parse).collect()
}

/// UUID 解析辅助（保持 FromStr 导入有效）
pub fn parse_uuid(s: &str) -> Option<Uuid> {
    Uuid::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::{AgentStore, GrantStore, MemoryHubStore};
    use store::InMemoryHubStore;

    async fn server() -> (AgentOAuthServer, Arc<InMemoryHubStore>) {
        let store = Arc::new(InMemoryHubStore::new());
        (
            AgentOAuthServer::new(
                store.clone(),
                "test-secret",
                "https://auth.example.com",
                "https://hub.example.com",
                3600,
                120,
                30,
            ),
            store,
        )
    }

    fn register_req(id: &str, with_secret: bool) -> RegisterAgentRequest {
        RegisterAgentRequest {
            agent_id: id.to_string(),
            name: format!("Agent {}", id),
            kind: "service".to_string(),
            client_secret: if with_secret {
                Some("s3cret".to_string())
            } else {
                None
            },
            redirect_uris: vec!["https://app.example.com/cb".to_string()],
            max_scopes: None,
        }
    }

    #[tokio::test]
    async fn test_register_and_client_credentials() {
        let (srv, _store) = server().await;
        srv.register(register_req("agent-a", true)).await.unwrap();

        let resp = srv
            .token(TokenRequest {
                grant_type: "client_credentials".to_string(),
                code: None,
                code_verifier: None,
                redirect_uri: None,
                client_id: Some("agent-a".to_string()),
                client_secret: Some("s3cret".to_string()),
                refresh_token: None,
                scope: None,
                resource: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.token_type, "Bearer");
        assert!(
            resp.refresh_token.is_none(),
            "client_credentials 不应发刷新令牌"
        );

        // 令牌可解析且含 agent 主体
        let intro = srv.introspect(&resp.access_token).await.unwrap();
        assert!(intro.active);
        assert_eq!(intro.agent_id.as_deref(), Some("agent-a"));
    }

    #[tokio::test]
    async fn test_client_credentials_rejects_bad_secret() {
        let (srv, _store) = server().await;
        srv.register(register_req("agent-a", true)).await.unwrap();

        let err = srv
            .token(TokenRequest {
                grant_type: "client_credentials".to_string(),
                code: None,
                code_verifier: None,
                redirect_uri: None,
                client_id: Some("agent-a".to_string()),
                client_secret: Some("wrong".to_string()),
                refresh_token: None,
                scope: None,
                resource: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::AuthRequired));
    }

    #[tokio::test]
    async fn test_public_client_cannot_use_client_credentials() {
        let (srv, _store) = server().await;
        srv.register(register_req("pub", false)).await.unwrap();

        let err = srv
            .token(TokenRequest {
                grant_type: "client_credentials".to_string(),
                code: None,
                code_verifier: None,
                redirect_uri: None,
                client_id: Some("pub".to_string()),
                client_secret: Some("anything".to_string()),
                refresh_token: None,
                scope: None,
                resource: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::AuthRequired));
    }

    #[tokio::test]
    async fn test_scope_cannot_exceed_registered_max() {
        let (srv, _store) = server().await;
        srv.register(register_req("agent-a", true)).await.unwrap();

        let err = srv
            .token(TokenRequest {
                grant_type: "client_credentials".to_string(),
                code: None,
                code_verifier: None,
                redirect_uri: None,
                client_id: Some("agent-a".to_string()),
                client_secret: Some("s3cret".to_string()),
                refresh_token: None,
                scope: Some("memory:admin".to_string()),
                resource: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));
    }

    #[tokio::test]
    async fn test_authorization_code_with_pkce() {
        let (srv, store) = server().await;
        let agent = srv.register(register_req("agent-a", false)).await.unwrap();

        // 先给命名空间授权（模拟 owner 分享）
        store
            .grant(&NamespaceGrantRecord {
                namespace: "team-x".to_string(),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "owner".to_string(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = compute_code_challenge_s256(verifier);
        let params = AuthorizeParams {
            response_type: "code".to_string(),
            client_id: "agent-a".to_string(),
            redirect_uri: "https://app.example.com/cb".to_string(),
            code_challenge: challenge,
            code_challenge_method: "S256".to_string(),
            state: Some("xyz".to_string()),
            scope: None,
        };

        srv.validate_authorize(&params).await.unwrap();
        let code = srv
            .issue_authorization_code(&params, &agent, vec![Scope::Read, Scope::Write], None)
            .await
            .unwrap();

        let resp = srv
            .token(TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code.clone()),
                code_verifier: Some(verifier.to_string()),
                redirect_uri: Some("https://app.example.com/cb".to_string()),
                client_id: Some("agent-a".to_string()),
                client_secret: None,
                refresh_token: None,
                scope: None,
                resource: None,
            })
            .await
            .unwrap();
        assert!(resp.refresh_token.is_some(), "授权码流程应发刷新令牌");

        // 授权码含命名空间授权
        let intro = srv.introspect(&resp.access_token).await.unwrap();
        assert_eq!(intro.grants.unwrap().len(), 1);

        // 授权码一次性：重放失败
        let replay = srv
            .token(TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code),
                code_verifier: Some(verifier.to_string()),
                redirect_uri: Some("https://app.example.com/cb".to_string()),
                client_id: Some("agent-a".to_string()),
                client_secret: None,
                refresh_token: None,
                scope: None,
                resource: None,
            })
            .await;
        assert!(replay.is_err());
    }

    #[tokio::test]
    async fn test_authorization_code_rejects_wrong_verifier() {
        let (srv, _store) = server().await;
        let agent = srv.register(register_req("agent-a", false)).await.unwrap();

        let challenge = compute_code_challenge_s256("correct-verifier");
        let params = AuthorizeParams {
            response_type: "code".to_string(),
            client_id: "agent-a".to_string(),
            redirect_uri: "https://app.example.com/cb".to_string(),
            code_challenge: challenge,
            code_challenge_method: "S256".to_string(),
            state: None,
            scope: None,
        };
        let code = srv
            .issue_authorization_code(&params, &agent, vec![Scope::Read], None)
            .await
            .unwrap();

        let err = srv
            .token(TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code),
                code_verifier: Some("wrong-verifier".to_string()),
                redirect_uri: Some("https://app.example.com/cb".to_string()),
                client_id: Some("agent-a".to_string()),
                client_secret: None,
                refresh_token: None,
                scope: None,
                resource: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::AuthRequired));
    }

    #[tokio::test]
    async fn test_authorize_rejects_bad_redirect_and_method() {
        let (srv, _store) = server().await;
        srv.register(register_req("agent-a", false)).await.unwrap();

        let mut params = AuthorizeParams {
            response_type: "token".to_string(), // 隐式流不允许
            client_id: "agent-a".to_string(),
            redirect_uri: "https://app.example.com/cb".to_string(),
            code_challenge: "x".to_string(),
            code_challenge_method: "S256".to_string(),
            state: None,
            scope: None,
        };
        assert!(srv.validate_authorize(&params).await.is_err());

        params.response_type = "code".to_string();
        params.redirect_uri = "https://evil.example.com/cb".to_string();
        assert!(srv.validate_authorize(&params).await.is_err());

        params.redirect_uri = "https://app.example.com/cb".to_string();
        params.code_challenge_method = "plain".to_string();
        assert!(srv.validate_authorize(&params).await.is_err());
    }

    #[tokio::test]
    async fn test_refresh_rotation() {
        let (srv, _store) = server().await;
        let agent = srv.register(register_req("agent-a", false)).await.unwrap();

        let challenge = compute_code_challenge_s256("v1");
        let params = AuthorizeParams {
            response_type: "code".to_string(),
            client_id: "agent-a".to_string(),
            redirect_uri: "https://app.example.com/cb".to_string(),
            code_challenge: challenge,
            code_challenge_method: "S256".to_string(),
            state: None,
            scope: None,
        };
        let code = srv
            .issue_authorization_code(&params, &agent, vec![Scope::Read], None)
            .await
            .unwrap();
        let first = srv
            .token(TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code),
                code_verifier: Some("v1".to_string()),
                redirect_uri: Some("https://app.example.com/cb".to_string()),
                client_id: None,
                client_secret: None,
                refresh_token: None,
                scope: None,
                resource: None,
            })
            .await
            .unwrap();
        let refresh = first.refresh_token.unwrap();

        let second = srv
            .token(TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                code_verifier: None,
                redirect_uri: None,
                client_id: None,
                client_secret: None,
                refresh_token: Some(refresh.clone()),
                scope: None,
                resource: None,
            })
            .await
            .unwrap();
        assert_ne!(refresh, second.refresh_token.unwrap(), "刷新令牌必须轮换");

        // 旧刷新令牌重放 → 失败
        let replay = srv
            .token(TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                code_verifier: None,
                redirect_uri: None,
                client_id: None,
                client_secret: None,
                refresh_token: Some(refresh),
                scope: None,
                resource: None,
            })
            .await;
        assert!(replay.is_err());
    }

    #[tokio::test]
    async fn test_introspect_unknown_token_is_inactive() {
        let (srv, _store) = server().await;
        let resp = srv.introspect("garbage").await.unwrap();
        assert!(!resp.active);
        assert!(resp.agent_id.is_none());
    }

    #[tokio::test]
    async fn test_register_validates_inputs() {
        let (srv, _store) = server().await;
        assert!(srv
            .register(RegisterAgentRequest {
                agent_id: "".to_string(),
                name: "x".to_string(),
                kind: "service".to_string(),
                client_secret: None,
                redirect_uris: vec![],
                max_scopes: None,
            })
            .await
            .is_err());

        assert!(srv
            .register(RegisterAgentRequest {
                agent_id: "a".to_string(),
                name: "x".to_string(),
                kind: "unknown-kind".to_string(),
                client_secret: None,
                redirect_uris: vec![],
                max_scopes: None,
            })
            .await
            .is_err());

        assert!(srv
            .register(RegisterAgentRequest {
                agent_id: "a".to_string(),
                name: "x".to_string(),
                kind: "service".to_string(),
                client_secret: None,
                redirect_uris: vec!["http://insecure.example.com".to_string()],
                max_scopes: None,
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_suspended_agent_cannot_get_tokens() {
        let (srv, store) = server().await;
        srv.register(register_req("agent-a", true)).await.unwrap();
        store
            .set_agent_status(&"agent-a".to_string(), "suspended")
            .await
            .unwrap();

        let err = srv
            .token(TokenRequest {
                grant_type: "client_credentials".to_string(),
                code: None,
                code_verifier: None,
                redirect_uri: None,
                client_id: Some("agent-a".to_string()),
                client_secret: Some("s3cret".to_string()),
                refresh_token: None,
                scope: None,
                resource: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::AuthRequired));
    }

    #[tokio::test]
    async fn test_scope_helpers() {
        assert_eq!(
            join_scopes(&[Scope::Read, Scope::Write]),
            "memory:read memory:write"
        );
        assert_eq!(parse_scopes("memory:read memory:admin").len(), 2);
        assert!(parse_uuid("not-a-uuid").is_none());
        assert!(parse_uuid(&Uuid::new_v4().to_string()).is_some());
    }

    // 编译期检查：内存存储满足聚合契约
    #[test]
    fn test_store_contract() {
        fn assert_impl<T: MemoryHubStore>() {}
        assert_impl::<InMemoryHubStore>();
    }
}
