//! 访问令牌签发与校验（as-core）
//!
//! 采用自包含的 HS256 JWT：授权服务器用共享密钥签发，资源服务器
//! （MCP 服务）用同一密钥离线验签，无需每次内省授权服务器。
//! 载荷直接复用 `authz::TokenClaims`，保证 `aud`（RFC 8707 资源指示符）
//! 与 `iss` 在资源服务器侧得到同等强校验。

use authz::TokenClaims;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{Duration, Utc};
use common::{AccountId, AgentId, AuthCtx, HubError, NamespaceGrant, Result, Scope};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// JWT 头部
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JwtHeader {
    alg: String,
    typ: String,
}

/// 访问令牌管理器
#[derive(Clone)]
pub struct AccessTokenManager {
    /// 共享签名密钥（HS256）
    secret: SecretString,
    /// 签发方标识（ISS）
    pub issuer: String,
    /// 受众：中枢资源 URI（RFC 8707）
    pub audience: String,
    /// 有效期（秒）
    pub ttl_seconds: i64,
}

impl AccessTokenManager {
    pub fn new(
        secret: impl Into<String>,
        issuer: impl Into<String>,
        audience: impl Into<String>,
        ttl_seconds: i64,
    ) -> Self {
        Self {
            secret: SecretString::new(secret.into().into()),
            issuer: issuer.into(),
            audience: audience.into(),
            ttl_seconds,
        }
    }

    fn signing_input(header: &JwtHeader, claims: &TokenClaims) -> Result<String> {
        let h = serde_json_to_b64(&header)?;
        let p = serde_json_to_b64(claims)?;
        Ok(format!("{}.{}", h, p))
    }

    fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
        // 标准 HMAC 构造：block size 64 字节
        let mut block = [0u8; 64];
        let key_len = key.len().min(64);
        block[..key_len].copy_from_slice(&key[..key_len]);

        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for i in 0..64 {
            ipad[i] ^= block[i];
            opad[i] ^= block[i];
        }

        let mut inner = Sha256::new();
        inner.update(ipad);
        inner.update(msg);
        let inner_digest = inner.finalize();

        let mut outer = Sha256::new();
        outer.update(opad);
        outer.update(inner_digest);
        outer.finalize().to_vec()
    }

    /// 签发访问令牌
    ///
    /// `agent_id` 为调用方 Agent 主体；`account_id` 为其所属账户（沙箱边界，
    /// 由授权服务器从 agents.owner_account_id 注入）；`grants` 为该 Agent
    /// 当前生效的命名空间授权条目（用户可见名，不含账户前缀）。
    pub fn issue(
        &self,
        agent_id: &AgentId,
        account_id: AccountId,
        scopes: Vec<Scope>,
        grants: Vec<NamespaceGrant>,
    ) -> Result<String> {
        let now = Utc::now();
        let claims = TokenClaims {
            sub: agent_id.clone(),
            iss: self.issuer.clone(),
            aud: self.audience.clone(),
            client_id: agent_id.clone(),
            account: Some(account_id),
            scopes,
            grants,
            iat: now.timestamp(),
            exp: (now + Duration::seconds(self.ttl_seconds)).timestamp(),
            jti: Uuid::new_v4().to_string(),
        };

        let signing_input = Self::signing_input(
            &JwtHeader {
                alg: "HS256".to_string(),
                typ: "JWT".to_string(),
            },
            &claims,
        )?;
        let sig = Self::hmac_sha256(
            self.secret.expose_secret().as_bytes(),
            signing_input.as_bytes(),
        );

        Ok(format!("{}.{}", signing_input, URL_SAFE_NO_PAD.encode(sig)))
    }

    /// 验签并解出载荷；签名不符或结构非法一律拒绝
    pub fn verify(&self, token: &str) -> Result<TokenClaims> {
        let mut parts = token.split('.');
        let header_b64 = parts.next().ok_or(HubError::AuthRequired)?;
        let payload_b64 = parts.next().ok_or(HubError::AuthRequired)?;
        let sig_b64 = parts.next().ok_or(HubError::AuthRequired)?;
        if parts.next().is_some() {
            return Err(HubError::AuthRequired);
        }

        let signing_input = format!("{}.{}", header_b64, payload_b64);
        let expected = Self::hmac_sha256(
            self.secret.expose_secret().as_bytes(),
            signing_input.as_bytes(),
        );

        use subtle::ConstantTimeEq;
        let provided = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| HubError::AuthRequired)?;
        if expected.len() != provided.len() || expected.ct_eq(&provided).unwrap_u8() == 0 {
            return Err(HubError::AuthRequired);
        }

        // 头部算法必须是 HS256，拒绝 alg=none 与算法混淆
        let header_bytes = URL_SAFE_NO_PAD
            .decode(header_b64)
            .map_err(|_| HubError::AuthRequired)?;
        let header: JwtHeader =
            serde_json::from_slice(&header_bytes).map_err(|_| HubError::AuthRequired)?;
        if header.alg != "HS256" || header.typ != "JWT" {
            return Err(HubError::AuthRequired);
        }

        let payload_bytes = URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| HubError::AuthRequired)?;
        let claims: TokenClaims =
            serde_json::from_slice(&payload_bytes).map_err(|_| HubError::AuthRequired)?;

        // 本地时效校验（资源服务器侧的兜底检查）
        if claims.exp <= Utc::now().timestamp() {
            return Err(HubError::AuthRequired);
        }
        // 受众与签发方必须在本地重新校验，绝不信任令牌自述
        if claims.aud != self.audience {
            return Err(HubError::InsufficientScope);
        }
        if claims.iss != self.issuer {
            return Err(HubError::AuthRequired);
        }

        Ok(claims)
    }

    /// 便捷方法：直接换取 AuthCtx（复用 hub-authz 的受众/时效强校验）
    pub fn introspect_to_ctx(
        &self,
        token: &str,
        validator: &authz::TokenValidator,
    ) -> Result<AuthCtx> {
        let claims = self.verify(token)?;
        validator.validate_claims(&claims)
    }
}

/// 授权服务器即令牌内省方：资源服务器凭同一密钥离线验签。
#[async_trait::async_trait]
impl mem_core::TokenIntrospector for AccessTokenManager {
    async fn introspect(&self, token: &str) -> Result<AuthCtx> {
        let validator = authz::TokenValidator::new(self.audience.clone(), self.issuer.clone());
        self.introspect_to_ctx(token, &validator)
    }
}

/// JSON → base64url 序列化辅助
fn serde_json_to_b64<T: Serialize>(v: &T) -> Result<String> {
    let bytes = serde_json::to_vec(v)
        .map_err(|e| HubError::Internal(anyhow::anyhow!("令牌载荷序列化失败: {}", e)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr() -> AccessTokenManager {
        AccessTokenManager::new(
            "test-secret-key",
            "https://auth.example.com",
            "https://hub.example.com",
            900,
        )
    }

    fn account() -> AccountId {
        Uuid::from_u128(7)
    }

    #[test]
    fn test_issue_and_verify_roundtrip() {
        let m = mgr();
        let token = m
            .issue(
                &"agent-alpha".to_string(),
                account(),
                vec![Scope::Read, Scope::Write],
                vec![NamespaceGrant::read_write("team-x")],
            )
            .unwrap();

        assert_eq!(token.split('.').count(), 3);
        let claims = m.verify(&token).unwrap();
        assert_eq!(claims.sub, "agent-alpha");
        assert_eq!(claims.client_id, "agent-alpha");
        assert_eq!(claims.account, Some(account()));
        assert_eq!(claims.iss, "https://auth.example.com");
        assert_eq!(claims.aud, "https://hub.example.com");
        assert!(claims.exp > claims.iat);
        assert_eq!(claims.grants.len(), 1);
        assert_eq!(claims.grants[0].namespace, "team-x");
        assert!(claims.grants[0].write);
    }

    #[test]
    fn test_tampered_payload_rejected() {
        let m = mgr();
        let token = m
            .issue(
                &"agent-alpha".to_string(),
                account(),
                vec![Scope::Read],
                vec![],
            )
            .unwrap();

        let mut parts: Vec<&str> = token.split('.').collect();
        // 篡改载荷：把 scope 提权（内容无关紧要，签名必然失配）
        let forged =
            URL_SAFE_NO_PAD.encode(b"{\"sub\":\"attacker\",\"scopes\":[\"memory:admin\"]}");
        parts[1] = &forged;
        let tampered = parts.join(".");

        assert!(m.verify(&tampered).is_err());
    }

    #[test]
    fn test_wrong_secret_rejected() {
        let m = mgr();
        let token = m
            .issue(
                &"agent-alpha".to_string(),
                account(),
                vec![Scope::Read],
                vec![],
            )
            .unwrap();

        let other = AccessTokenManager::new(
            "different-secret",
            "https://auth.example.com",
            "https://hub.example.com",
            900,
        );
        assert!(other.verify(&token).is_err());
    }

    #[test]
    fn test_alg_none_rejected() {
        let m = mgr();
        let header = JwtHeader {
            alg: "none".to_string(),
            typ: "JWT".to_string(),
        };
        let now = Utc::now().timestamp();
        let claims = TokenClaims {
            sub: "agent-alpha".to_string(),
            iss: m.issuer.clone(),
            aud: m.audience.clone(),
            client_id: "agent-alpha".to_string(),
            account: Some(account()),
            scopes: vec![Scope::Read],
            grants: vec![],
            iat: now,
            exp: now + 900,
            jti: "t".to_string(),
        };
        let token = format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        assert!(m.verify(&token).is_err());
    }

    #[test]
    fn test_malformed_token_rejected() {
        let m = mgr();
        assert!(m.verify("not-a-token").is_err());
        assert!(m.verify("").is_err());
        assert!(m.verify("a.b").is_err());
        assert!(m.verify("a.b.c.d").is_err());
    }
}
