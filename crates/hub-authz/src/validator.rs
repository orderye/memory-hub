//! 访问令牌校验与 RFC 8707 资源指示符强制绑定（基于设计规范 5.9 节）

use chrono::Utc;
use common::{Acl, AuthCtx, ClientId, HubError, Result, Scope, UserId};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// 访问令牌载荷 Claims
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    /// 用户主体 ID
    pub sub: UserId,
    /// 签发方
    pub iss: String,
    /// 受众目标（必须严格绑定为中枢资源 URI，RFC 8707）
    pub aud: String,
    /// 客户端 ClientId
    pub client_id: ClientId,
    /// 授权 Scopes
    pub scopes: Vec<Scope>,
    /// 授权可读层
    pub layers_read: HashSet<String>,
    /// 授权可写层
    pub layers_write: HashSet<String>,
    /// 是否允许涉及敏感层
    pub allow_sensitive: bool,
    /// 过期时间戳（秒）
    pub exp: i64,
    /// 签发时间戳（秒）
    pub iat: i64,
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

        Ok(AuthCtx {
            user_id: claims.sub,
            client_id: claims.client_id.clone(),
            scopes: claims.scopes.clone(),
            acl: Acl {
                read_layers: claims.layers_read.clone(),
                write_layers: claims.layers_write.clone(),
                allow_sensitive: claims.allow_sensitive,
            },
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
    use uuid::Uuid;

    #[test]
    fn test_valid_token_claims() {
        let validator = TokenValidator::new("https://hub.example.com", "https://auth.example.com");
        let now = Utc::now().timestamp();
        let user_id = Uuid::new_v4();

        let claims = TokenClaims {
            sub: user_id,
            iss: "https://auth.example.com".to_string(),
            aud: "https://hub.example.com".to_string(),
            client_id: "assistant_a".to_string(),
            scopes: vec![Scope::Context, Scope::Write],
            layers_read: HashSet::from(["preference".to_string()]),
            layers_write: HashSet::from(["preference".to_string()]),
            allow_sensitive: false,
            exp: now + 900,
            iat: now,
        };

        let auth_ctx = validator
            .validate_claims(&claims)
            .expect("Claims should be valid");
        assert_eq!(auth_ctx.user_id, user_id);
        assert!(auth_ctx.has_scope(&Scope::Context));
        assert!(auth_ctx.has_scope(&Scope::Write));
        assert!(!auth_ctx.has_scope(&Scope::Manage));
        assert!(validator
            .enforce_tool_scope(&auth_ctx, &Scope::Context)
            .is_ok());
        assert!(validator
            .enforce_tool_scope(&auth_ctx, &Scope::Manage)
            .is_err());
    }

    #[test]
    fn test_audience_mismatch_rejected() {
        let validator = TokenValidator::new("https://hub.example.com", "https://auth.example.com");
        let now = Utc::now().timestamp();

        let claims = TokenClaims {
            sub: Uuid::new_v4(),
            iss: "https://auth.example.com".to_string(),
            aud: "https://other-service.example.com".to_string(), // 错配受众
            client_id: "asst".to_string(),
            scopes: vec![Scope::Context],
            layers_read: HashSet::new(),
            layers_write: HashSet::new(),
            allow_sensitive: false,
            exp: now + 900,
            iat: now,
        };

        assert!(validator.validate_claims(&claims).is_err());
    }

    #[test]
    fn test_expired_token_rejected() {
        let validator = TokenValidator::new("https://hub.example.com", "https://auth.example.com");
        let now = Utc::now().timestamp();

        let claims = TokenClaims {
            sub: Uuid::new_v4(),
            iss: "https://auth.example.com".to_string(),
            aud: "https://hub.example.com".to_string(),
            client_id: "asst".to_string(),
            scopes: vec![Scope::Context],
            layers_read: HashSet::new(),
            layers_write: HashSet::new(),
            allow_sensitive: false,
            exp: now - 10, // 已过期
            iat: now - 1000,
        };

        assert!(validator.validate_claims(&claims).is_err());
    }
}
