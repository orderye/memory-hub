//! 刷新令牌轮换与重放检测（OAuth 2.1 Token Family & Rotation）

use chrono::{DateTime, Duration, Utc};
use common::{ClientId, HubError, Result, Scope, UserId};
use std::collections::HashMap;
use std::sync::RwLock;
use uuid::Uuid;

/// 刷新令牌记录
#[derive(Debug, Clone)]
pub struct RefreshTokenRecord {
    pub token_string: String,
    pub family_id: Uuid,
    pub user_id: UserId,
    pub client_id: ClientId,
    pub scopes: Vec<Scope>,
    pub is_rotated: bool,
    pub is_revoked: bool,
    pub expires_at: DateTime<Utc>,
}

/// 刷新令牌管理器
pub struct TokenFamilyManager {
    tokens: RwLock<HashMap<String, RefreshTokenRecord>>,
    ttl_days: i64,
}

impl TokenFamilyManager {
    pub fn new(ttl_days: i64) -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
            ttl_days,
        }
    }

    /// 首次签发刷新令牌（创建新的令牌族）
    pub fn issue_initial(
        &self,
        user_id: UserId,
        client_id: ClientId,
        scopes: Vec<Scope>,
    ) -> String {
        let mut write = self.tokens.write().unwrap();
        let family_id = Uuid::new_v4();
        let token_string = Uuid::new_v4().to_string();
        let expires_at = Utc::now() + Duration::days(self.ttl_days);

        write.insert(
            token_string.clone(),
            RefreshTokenRecord {
                token_string: token_string.clone(),
                family_id,
                user_id,
                client_id,
                scopes,
                is_rotated: false,
                is_revoked: false,
                expires_at,
            },
        );

        token_string
    }

    /// 轮换刷新令牌（严格执行重放攻击检测）
    pub fn rotate(&self, old_token: &str) -> Result<(String, UserId, ClientId, Vec<Scope>)> {
        let mut write = self.tokens.write().unwrap();
        let now = Utc::now();

        let record = write.get_mut(old_token).ok_or(HubError::AuthRequired)?;

        // 1. 检查是否过期
        if record.expires_at <= now {
            return Err(HubError::AuthRequired);
        }

        // 2. 检查是否已被撤销
        if record.is_revoked {
            return Err(HubError::AuthRequired);
        }

        // 3. 核心安全防御：检测重放攻击（Replay Attack）
        if record.is_rotated {
            let attacked_family = record.family_id;
            tracing::error!(
                family_id = %attacked_family,
                user_id = %record.user_id,
                client_id = %record.client_id,
                "SECURITY ALERT: Refresh token replay detected! Revoking entire token family."
            );

            // 立即吊销同族所有令牌！
            for item in write.values_mut() {
                if item.family_id == attacked_family {
                    item.is_revoked = true;
                }
            }

            return Err(HubError::AuthRequired);
        }

        // 4. 正常轮换：标记旧令牌为已轮换
        record.is_rotated = true;
        let family_id = record.family_id;
        let user_id = record.user_id;
        let client_id = record.client_id.clone();
        let scopes = record.scopes.clone();

        // 签发同族的新刷新令牌
        let new_token_string = Uuid::new_v4().to_string();
        let expires_at = now + Duration::days(self.ttl_days);

        write.insert(
            new_token_string.clone(),
            RefreshTokenRecord {
                token_string: new_token_string.clone(),
                family_id,
                user_id,
                client_id: client_id.clone(),
                scopes: scopes.clone(),
                is_rotated: false,
                is_revoked: false,
                expires_at,
            },
        );

        Ok((new_token_string, user_id, client_id, scopes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_rotation_and_replay_detection() {
        let manager = TokenFamilyManager::new(30);
        let user_id = Uuid::new_v4();
        let client_id = "assistant_1".to_string();
        let scopes = vec![Scope::Context, Scope::Write];

        // 1. 初始签发
        let t1 = manager.issue_initial(user_id, client_id.clone(), scopes.clone());

        // 2. 第一次正常刷新 -> 获得 t2
        let (t2, u_id, c_id, _) = manager.rotate(&t1).expect("First rotation should succeed");
        assert_eq!(u_id, user_id);
        assert_eq!(c_id, client_id);
        assert_ne!(t1, t2);

        // 3. 正常再次刷新 t2 -> 获得 t3
        let (t3, _, _, _) = manager.rotate(&t2).expect("Second rotation should succeed");
        assert_ne!(t2, t3);

        // 4. 模拟黑客窃取了已废弃的 t1，并发起重放刷新
        let replay_res = manager.rotate(&t1);
        assert!(replay_res.is_err(), "Replaying t1 must fail");

        // 5. 验证整个令牌族已被联级吊销：合法的 t3 也应被拒绝！
        let t3_res = manager.rotate(&t3);
        assert!(
            t3_res.is_err(),
            "Token family must be completely revoked after replay attempt"
        );
    }
}
