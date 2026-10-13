//! 账户服务：注册、登录、会话与"注册令牌"（DCR 的 initial access token）
//!
//! 账户是沙箱边界：一个账户拥有自己的 Agent 集合与独立的命名空间空间，
//! 跨账户无任何访问通路。本模块负责控制台侧的身份能力：
//!
//! - 用户名/口令注册与登录（Argon2id 口令散列；可选邀请码）；
//! - 登录会话：随机令牌 → sha256 落库（`account_sessions`），Cookie 只持有原文；
//! - 注册令牌：登录后签发的短期 HS256 令牌，供 MCP 客户端在
//!   `POST /oauth/register`（DCR）中作为 initial access token 使用，
//!   使程序化注册也能绑定到具体账户。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use auth_core::AccessTokenManager;
use chrono::{Duration, Utc};
use common::{AccountId, HubError, Result};
use mem_core::{AccountRecord, AccountStore};
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// 注册令牌有效期（秒）
pub const REGISTRATION_TOKEN_TTL_S: i64 = 3600;

/// 用户名长度范围
pub const USERNAME_MIN: usize = 3;
pub const USERNAME_MAX: usize = 32;
/// 口令长度范围
pub const PASSWORD_MIN: usize = 8;
pub const PASSWORD_MAX: usize = 128;
/// 展示名长度上限
pub const DISPLAY_NAME_MAX: usize = 64;

/// 会话 Cookie 名
pub const SESSION_COOKIE: &str = "hub_session";

/// 账户服务
pub struct AccountService {
    store: Arc<dyn AccountStore>,
    /// 邀请码（None / 空 = 开放注册）
    invite_code: Option<SecretString>,
    session_ttl_hours: i64,
    register_tokens: AccessTokenManager,
}

impl AccountService {
    pub fn new(
        store: Arc<dyn AccountStore>,
        invite_code: Option<String>,
        session_ttl_hours: i64,
        token_secret: impl Into<String>,
        issuer: impl Into<String>,
        register_audience: impl Into<String>,
    ) -> Self {
        Self {
            store,
            invite_code: invite_code
                .filter(|c| !c.trim().is_empty())
                .map(|c| SecretString::new(c.into())),
            session_ttl_hours: session_ttl_hours.max(1),
            register_tokens: AccessTokenManager::new(
                token_secret,
                issuer,
                register_audience,
                REGISTRATION_TOKEN_TTL_S,
            ),
        }
    }

    /// 是否开放注册（未配置邀请码）
    pub fn registration_open(&self) -> bool {
        self.invite_code.is_none()
    }

    /// 会话有效期（秒）
    pub fn session_ttl_secs(&self) -> i64 {
        self.session_ttl_hours * 3600
    }

    /// 注册：校验邀请码与口令强度，创建账户并直接建立登录会话
    pub async fn register(
        &self,
        username: &str,
        password: &str,
        display_name: &str,
        invite_code: Option<&str>,
    ) -> Result<(AccountRecord, String)> {
        let normalized = validate_username(username)?;
        validate_password(password)?;
        if display_name.chars().count() > DISPLAY_NAME_MAX {
            return Err(HubError::InvalidArgument(format!(
                "展示名不得超过 {} 字符",
                DISPLAY_NAME_MAX
            )));
        }

        if let Some(expected) = &self.invite_code {
            let provided = invite_code.unwrap_or_default();
            if !constant_time_eq(provided.as_bytes(), expected.expose_secret().as_bytes()) {
                return Err(HubError::InvalidArgument("邀请码不正确".to_string()));
            }
        }

        let hash = hash_password(password)?;
        let account = self
            .store
            .create_account(&normalized, Some(&hash), display_name.trim())
            .await?;
        let session = self.open_session(&account.id).await?;
        Ok((account, session))
    }

    /// 登录：口令校验通过后建立会话
    pub async fn login(&self, username: &str, password: &str) -> Result<(AccountRecord, String)> {
        let account = match self.store.get_account_by_username(username.trim()).await? {
            Some(a) => a,
            None => return Err(invalid_credentials()),
        };
        let stored = self
            .store
            .password_hash_of(&account.username)
            .await?
            .ok_or_else(|| HubError::AuthRequired)?;
        if !verify_password(password, &stored) {
            return Err(invalid_credentials());
        }
        if account.status != "active" {
            return Err(HubError::AuthRequired);
        }
        let session = self.open_session(&account.id).await?;
        Ok((account, session))
    }

    /// 登出：吊销会话（幂等）
    pub async fn logout(&self, session_token: &str) -> Result<()> {
        if !session_token.is_empty() {
            self.store
                .delete_session(&session_token_hash(session_token))
                .await?;
        }
        Ok(())
    }

    /// 会话 → 账户（过期、被吊销或账户停用均返回 None）
    pub async fn session_account(&self, session_token: &str) -> Result<Option<AccountRecord>> {
        if session_token.is_empty() {
            return Ok(None);
        }
        let hash = session_token_hash(session_token);
        let Some(session) = self.store.get_session(&hash).await? else {
            return Ok(None);
        };
        if session.expires_at <= Utc::now() {
            self.store.delete_session(&hash).await?;
            return Ok(None);
        }
        let Some(account) = self.store.get_account(&session.account_id).await? else {
            return Ok(None);
        };
        if account.status != "active" {
            return Ok(None);
        }
        Ok(Some(account))
    }

    /// 清理过期会话（启动/定期维护）
    pub async fn prune_sessions(&self) -> Result<usize> {
        self.store.prune_sessions(Utc::now()).await
    }

    /// 签发注册令牌（DCR 用 initial access token）
    pub async fn issue_registration_token(&self, account: &AccountRecord) -> Result<String> {
        if account.status != "active" {
            return Err(HubError::AuthRequired);
        }
        self.register_tokens.issue(
            &format!("account:{}", account.username),
            account.id,
            vec![],
            vec![],
        )
    }

    /// 校验注册令牌 → 账户（签名、aud、时效、账户状态全查）
    pub async fn verify_registration_token(&self, token: &str) -> Result<AccountRecord> {
        let claims = self.register_tokens.verify(token)?;
        let account_id = claims.account.ok_or(HubError::AuthRequired)?;
        let account = self
            .store
            .get_account(&account_id)
            .await?
            .ok_or(HubError::AuthRequired)?;
        if account.status != "active" {
            return Err(HubError::AuthRequired);
        }
        Ok(account)
    }

    /// 账户列表（引导账户在控制台可见全部账户）
    pub async fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        self.store.list_accounts().await
    }

    /// 停用/恢复账户（仅引导账户调用）
    pub async fn set_account_status(&self, account_id: &AccountId, status: &str) -> Result<()> {
        if !matches!(status, "active" | "suspended") {
            return Err(HubError::InvalidArgument(
                "status 只支持 active | suspended".to_string(),
            ));
        }
        self.store.set_account_status(account_id, status).await
    }

    /// 建立会话：返回 Cookie 持有的原文令牌
    async fn open_session(&self, account_id: &AccountId) -> Result<String> {
        let token = new_session_token();
        let expires_at = Utc::now() + Duration::hours(self.session_ttl_hours);
        self.store
            .create_session(&session_token_hash(&token), account_id, expires_at)
            .await?;
        Ok(token)
    }
}

/// 引导账户：按固定 ID 确保存在；提供口令时写入/更新口令散列
pub async fn ensure_bootstrap_account(
    store: &dyn AccountStore,
    username: &str,
    password: Option<&str>,
) -> Result<AccountRecord> {
    let normalized = validate_username(username).unwrap_or_else(|_| "bootstrap".to_string());
    let account = store
        .ensure_account(&common::BOOTSTRAP_ACCOUNT_ID, &normalized, "引导账户")
        .await?;
    if let Some(password) = password.filter(|p| !p.trim().is_empty()) {
        validate_password(password)?;
        let hash = hash_password(password)?;
        store.set_account_password(&account.id, &hash).await?;
        return store
            .get_account(&account.id)
            .await?
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("引导账户口令写入后读取失败")));
    }
    Ok(account)
}

/// 用户名校验并归一化（小写；3..=32；仅 [a-z0-9_-]）
pub fn validate_username(username: &str) -> Result<String> {
    let normalized = username.trim().to_ascii_lowercase();
    let len = normalized.chars().count();
    if !(USERNAME_MIN..=USERNAME_MAX).contains(&len) {
        return Err(HubError::InvalidArgument(format!(
            "用户名长度必须在 {}..={} 之间",
            USERNAME_MIN, USERNAME_MAX
        )));
    }
    if !normalized
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
    {
        return Err(HubError::InvalidArgument(
            "用户名只允许小写字母、数字、-、_".to_string(),
        ));
    }
    Ok(normalized)
}

/// 口令强度校验（长度；内容不做复杂度强制）
pub fn validate_password(password: &str) -> Result<()> {
    let len = password.chars().count();
    if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&len) {
        return Err(HubError::InvalidArgument(format!(
            "口令长度必须在 {}..={} 之间",
            PASSWORD_MIN, PASSWORD_MAX
        )));
    }
    Ok(())
}

/// OWASP 基准参数的 Argon2id 实例：64MiB 内存硬度、3 轮迭代、1 并行度
pub fn owasp_argon2() -> Argon2<'static> {
    let params = Params::new(65536, 3, 1, None).expect("Valid OWASP Argon2 parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Argon2id 口令散列（返回 PHC 字符串，符合 OWASP 基线）
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::encode_b64(&random_bytes())
        .map_err(|e| HubError::Internal(anyhow::anyhow!("口令盐生成失败: {}", e)))?;
    owasp_argon2()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| HubError::Internal(anyhow::anyhow!("口令散列失败: {}", e)))
}

/// 口令校验（散列结构非法一律判否）
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// 新会话令牌：32 字节随机 → hex（Cookie 持有原文，库中只存 sha256）
pub fn new_session_token() -> String {
    hex::encode(random_bytes())
}

/// 会话令牌散列
pub fn session_token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// 32 字节密码学安全随机字节（CSPRNG，无 UUID 固定版本位偏差）
fn random_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    getrandom::getrandom(&mut out).expect("CSPRNG failure");
    out
}

fn invalid_credentials() -> HubError {
    // 不区分"用户不存在"与"口令错误"，避免账户枚举
    HubError::InvalidArgument("用户名或口令不正确".to_string())
}

/// 常量时间比较（邀请码等短秘密）
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && a.ct_eq(b).unwrap_u8() == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::InMemoryHubStore;

    fn service(store: Arc<InMemoryHubStore>, invite: Option<&str>) -> AccountService {
        AccountService::new(
            store,
            invite.map(str::to_string),
            // 会话 TTL 12 小时
            12,
            "test-secret",
            "https://auth.example.com",
            "https://auth.example.com/oauth/register",
        )
    }

    #[test]
    fn test_username_and_password_rules() {
        assert_eq!(validate_username(" Alice_01 ").unwrap(), "alice_01");
        assert!(validate_username("ab").is_err());
        assert!(validate_username("has space").is_err());
        assert!(validate_username("中文名").is_err());

        assert!(validate_password("12345678").is_ok());
        assert!(validate_password("short").is_err());
        assert!(validate_password(&"x".repeat(129)).is_err());
    }

    #[test]
    fn test_password_hash_roundtrip() {
        let phc = hash_password("correct horse").unwrap();
        assert!(phc.starts_with("$argon2"));
        assert!(
            phc.contains("m=65536,t=3,p=1"),
            "应符合 OWASP 基准参数: {}",
            phc
        );
        assert!(verify_password("correct horse", &phc));
        assert!(!verify_password("wrong horse", &phc));
        assert!(!verify_password("correct horse", "not-a-phc"));
    }

    #[tokio::test]
    async fn test_register_login_session_flow() {
        let store = Arc::new(InMemoryHubStore::new());
        let svc = service(store.clone(), None);

        let (account, session) = svc
            .register("Alice", "supersecret", "爱丽丝", None)
            .await
            .unwrap();
        assert_eq!(account.username, "alice");
        assert!(account.has_password);

        // 会话可用
        let who = svc.session_account(&session).await.unwrap().unwrap();
        assert_eq!(who.id, account.id);

        // 重复注册 → 拒绝
        assert!(svc
            .register("alice", "supersecret", "", None)
            .await
            .is_err());

        // 错误口令 → 拒绝；正确口令 → 新会话
        assert!(svc.login("alice", "wrong").await.is_err());
        let (again, session2) = svc.login("alice", "supersecret").await.unwrap();
        assert_eq!(again.id, account.id);
        assert_ne!(session, session2, "每次登录签发新会话");

        // 登出后会话失效
        svc.logout(&session2).await.unwrap();
        assert!(svc.session_account(&session2).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_invite_code_enforced() {
        let store = Arc::new(InMemoryHubStore::new());
        let svc = service(store, Some("open-sesame"));
        assert!(!svc.registration_open());

        assert!(svc
            .register("bob", "supersecret", "", Some("wrong"))
            .await
            .is_err());
        assert!(svc.register("bob", "supersecret", "", None).await.is_err());
        assert!(svc
            .register("bob", "supersecret", "", Some("open-sesame"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_registration_token_roundtrip_and_audience() {
        let store = Arc::new(InMemoryHubStore::new());
        let svc = service(store, None);
        let (account, _) = svc
            .register("carol", "supersecret", "", None)
            .await
            .unwrap();

        let token = svc.issue_registration_token(&account).await.unwrap();
        let verified = svc.verify_registration_token(&token).await.unwrap();
        assert_eq!(verified.id, account.id);

        // 资源服务器的令牌管理器（aud=资源 URI）不能接受注册令牌
        let resource_tokens = AccessTokenManager::new(
            "test-secret",
            "https://auth.example.com",
            "https://hub.example.com",
            900,
        );
        assert!(
            resource_tokens.verify(&token).is_err(),
            "aud 严格绑定必须生效"
        );
    }

    #[tokio::test]
    async fn test_suspended_account_session_rejected() {
        let store = Arc::new(InMemoryHubStore::new());
        let svc = service(store.clone(), None);
        let (account, session) = svc.register("dave", "supersecret", "", None).await.unwrap();

        store
            .set_account_status(&account.id, "suspended")
            .await
            .unwrap();
        assert!(svc.session_account(&session).await.unwrap().is_none());
        assert!(svc.login("dave", "supersecret").await.is_err());
    }

    #[tokio::test]
    async fn test_bootstrap_account_ensure_and_password() {
        let store = InMemoryHubStore::new();
        let a1 = ensure_bootstrap_account(&store, "boss", Some("bootstrap-pass"))
            .await
            .unwrap();
        assert_eq!(a1.id, common::BOOTSTRAP_ACCOUNT_ID);
        assert!(a1.has_password);

        // 幂等：再次 ensure 不改变账户
        let a2 = ensure_bootstrap_account(&store, "boss", None)
            .await
            .unwrap();
        assert_eq!(a2.id, a1.id);
        assert!(a2.has_password);
    }
}
