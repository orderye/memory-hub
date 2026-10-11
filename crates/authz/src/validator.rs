//! 访问令牌校验与 RFC 8707 资源指示符强制绑定
//!
//! 令牌由授权服务器（auth-server）签发，资源服务器（mcp-server）用同一
//! HS256 密钥**离线验签**：不需要每次调用都内省授权服务器。
//! `aud` 必须严格绑定为本服务资源 URI，拒绝发给其他服务的令牌。

use chrono::Utc;
use common::{Acl, AgentId, AuthCtx, HubError, NamespaceGrant, Result, Scope};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 访问令牌载荷 Claims
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    /// 主体：调用方 Agent 标识
    pub sub: AgentId,
    /// 签发方
    pub iss: String,
    /// 受众目标（必须严格绑定为本服务资源 URI，RFC 8707）
    pub aud: String,
    /// OAuth 客户端标识（与 sub 一致；保留以兼容 RFC 7662 内省响应）
    pub client_id: AgentId,
    /// 所属账户（沙箱边界）；缺失视为非法令牌
    pub account: Option<Uuid>,
    /// 授权 Scopes
    pub scopes: Vec<Scope>,
    /// 命名空间授权条目（读/写分开）
    #[serde(default)]
    pub grants: Vec<NamespaceGrant>,
    /// 过期时间戳（秒）
    pub exp: i64,
    /// 签发时间戳（秒）
    pub iat: i64,
    /// 令牌唯一标识（jti），用于审计与失效排查
    #[serde(default)]
    pub jti: String,
}

/// 令牌校验器
pub struct TokenValidator {
    expected_resource_uri: String,
    expected_issuer: String,
}

impl TokenValidator {
    pub fn new(
        expected_resource_uri: impl Into<String>,
        expected_issuer: impl Into<String>,
    ) -> Self {
        Self {
            expected_resource_uri: expected_resource_uri.into(),
            expected_issuer: expected_issuer.into(),
        }
    }

    /// 校验 TokenClaims 并构造 AuthCtx
    pub fn validate_claims(&self, claims: &TokenClaims) -> Result<AuthCtx> {
        let now = Utc::now().timestamp();

        // 1. 检查时效
        if claims.exp <= now {
            return Err(HubError::AuthRequired);
        }

        // 2. 检查发行方 iss
        if claims.iss != self.expected_issuer {
            return Err(HubError::AuthRequired);
        }

        // 3. 检查受众 aud（RFC 8707 资源指示符校验，防跨服务透传）
        if claims.aud != self.expected_resource_uri {
            tracing::warn!(
                provided_aud = %claims.aud,
                expected_aud = %self.expected_resource_uri,
                "Audience mismatch: token was not issued for this resource"
            );
            return Err(HubError::InsufficientScope);
        }

        // 4. 主体非空
        if claims.sub.trim().is_empty() {
            return Err(HubError::AuthRequired);
        }

        // 5. 账户声明必须存在（沙箱边界；升级前的旧令牌一律拒收）
        let Some(account_id) = claims.account else {
            tracing::warn!(agent = %claims.sub, "Token missing account claim: rejected");
            return Err(HubError::AuthRequired);
        };

        Ok(AuthCtx {
            agent_id: claims.sub.clone(),
            account_id,
            scopes: claims.scopes.clone(),
            acl: Acl::from_grants(&claims.grants),
        })
    }

    /// 检查特定工具调用所需权限
    pub fn enforce_tool_scope(&self, auth: &AuthCtx, required_scope: &Scope) -> Result<()> {
        if !auth.has_scope(required_scope) {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims_with(account: Option<Uuid>) -> TokenClaims {
        let now = Utc::now().timestamp();
        TokenClaims {
            sub: "agent-alpha".to_string(),
            iss: "https://auth.example.com".to_string(),
            aud: "https://hub.example.com".to_string(),
            client_id: "agent-alpha".to_string(),
            account,
            scopes: vec![Scope::Read, Scope::Write],
            grants: vec![NamespaceGrant::read_write("team-x")],
            exp: now + 900,
            iat: now,
            jti: "test-jti".to_string(),
        }
    }

    #[test]
    fn test_valid_token_claims() {
        let validator = TokenValidator::new("https://hub.example.com", "https://auth.example.com");
        let account = Uuid::from_u128(9);

        let auth_ctx = validator
            .validate_claims(&claims_with(Some(account)))
            .expect("Claims should be valid");
        assert_eq!(auth_ctx.agent_id, "agent-alpha");
        assert_eq!(auth_ctx.account_id, account, "沙箱边界随令牌下发");
        assert!(auth_ctx.has_scope(&Scope::Read));
        assert!(auth_ctx.has_scope(&Scope::Write));
        assert!(!auth_ctx.has_scope(&Scope::Admin));
        assert!(auth_ctx.can_read_namespace("team-x"));
        assert!(auth_ctx.can_write_namespace("team-x"));
        assert!(!auth_ctx.can_read_namespace("team-y"));
        assert!(validator
            .enforce_tool_scope(&auth_ctx, &Scope::Read)
            .is_ok());
        assert!(validator
            .enforce_tool_scope(&auth_ctx, &Scope::Admin)
            .is_err());
    }

    #[test]
    fn test_missing_account_rejected() {
        let validator = TokenValidator::new("https://hub.example.com", "https://auth.example.com");
        // 升级前签发的旧令牌没有 account 声明：一律拒收
        let err = validator
            .validate_claims(&claims_with(None))
            .unwrap_err();
        assert!(matches!(err, HubError::AuthRequired));
    }

    #[test]
    fn test_audience_mismatch_rejected() {
        let validator = TokenValidator::new("https://hub.example.com", "https://auth.example.com");
        let mut claims = claims_with(Some(Uuid::from_u128(9)));
        claims.aud = "https://other-service.example.com".to_string(); // 错配受众
        assert!(validator.validate_claims(&claims).is_err());
    }

    #[test]
    fn test_expired_token_rejected() {
        let validator = TokenValidator::new("https://hub.example.com", "https://auth.example.com");
        let now = Utc::now().timestamp();
        let mut claims = claims_with(Some(Uuid::from_u128(9)));
        claims.exp = now - 10; // 已过期
        claims.iat = now - 1000;
        assert!(validator.validate_claims(&claims).is_err());
    }

    #[test]
    fn test_empty_subject_rejected() {
        let validator = TokenValidator::new("https://hub.example.com", "https://auth.example.com");
        let mut claims = claims_with(Some(Uuid::from_u128(9)));
        claims.sub = "  ".to_string();
        claims.client_id = "  ".to_string();
        assert!(validator.validate_claims(&claims).is_err());
    }
}
