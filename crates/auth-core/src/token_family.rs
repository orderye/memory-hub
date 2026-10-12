//! 刷新令牌轮换与重放检测（OAuth 2.1 Token Family & Rotation）

use chrono::{DateTime, Duration, Utc};
use common::{AgentId, HubError, NamespaceGrant, Result, Scope};
use std::collections::HashMap;
use std::sync::RwLock;
use uuid::Uuid;

/// 刷新令牌记录
#[derive(Debug, Clone)]
pub struct RefreshTokenRecord {
    pub token_string: String,
    pub family_id: Uuid,
    pub agent_id: AgentId,
    pub scopes: Vec<Scope>,
    /// 命名空间授权（刷新时原样延续，不得降级）
    pub grants: Vec<NamespaceGrant>,
    pub is_rotated: bool,
    pub is_revoked: bool,
    pub expires_at: DateTime<Utc>,
}

/// 轮换结果：新刷新令牌及其必须原样延续的 ACL
#[derive(Debug, Clone)]
pub struct RotatedTokens {
    pub refresh_token: String,
    pub agent_id: AgentId,
    pub scopes: Vec<Scope>,
    pub grants: Vec<NamespaceGrant>,
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
        agent_id: &AgentId,
        scopes: Vec<Scope>,
        grants: Vec<NamespaceGrant>,
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
                agent_id: agent_id.clone(),
                scopes,
                grants,
                is_rotated: false,
                is_revoked: false,
                expires_at,
            },
        );

        token_string
    }

    /// 轮换刷新令牌（严格执行重放攻击检测）
    ///
    /// 返回值中的命名空间授权为授权时的原始裁剪结果，
    /// 刷新流程必须原样延续，不得降级或扩张。
    pub fn rotate(&self, old_token: &str) -> Result<RotatedTokens> {
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
                agent_id = %record.agent_id,
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
        let agent_id = record.agent_id.clone();
        let scopes = record.scopes.clone();
        let grants = record.grants.clone();

        // 签发同族的新刷新令牌
        let new_token_string = Uuid::new_v4().to_string();
        let expires_at = now + Duration::days(self.ttl_days);

        write.insert(
            new_token_string.clone(),
            RefreshTokenRecord {
                token_string: new_token_string.clone(),
                family_id,
                agent_id: agent_id.clone(),
                scopes: scopes.clone(),
                grants: grants.clone(),
                is_rotated: false,
                is_revoked: false,
                expires_at,
            },
        );

        Ok(RotatedTokens {
            refresh_token: new_token_string,
            agent_id,
            scopes,
            grants,
        })
    }

    /// 在轮换前预查令牌所属 Agent 主体（不触发状态变更与重放检测，供预鉴权使用）
    pub fn peek_agent_id(&self, token: &str) -> Option<AgentId> {
        let read = self.tokens.read().unwrap();
        read.get(token).map(|r| r.agent_id.clone())
    }

    /// 撤销给定刷新令牌所在的整个令牌族（RFC 7009 语义）
    pub fn revoke_family(&self, token: &str) {
        if let Ok(mut write) = self.tokens.write() {
            if let Some(record) = write.get(token) {
                let family_id = record.family_id;
                for item in write.values_mut() {
                    if item.family_id == family_id {
                        item.is_revoked = true;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_rotation_and_replay_detection() {
        let manager = TokenFamilyManager::new(30);
        let agent_id: AgentId = "agent-alpha".to_string();
        let scopes = vec![Scope::Read, Scope::Write];
        let grants = vec![NamespaceGrant::read_write("team-x")];

        // 1. 初始签发
        let t1 = manager.issue_initial(&agent_id, scopes.clone(), grants.clone());

        // 2. 第一次正常刷新 -> 获得 t2
        let r2 = manager.rotate(&t1).expect("First rotation should succeed");
        assert_eq!(r2.agent_id, agent_id);
        assert_eq!(r2.grants.len(), 1);
        let t2 = r2.refresh_token.clone();
        assert_ne!(t1, t2);

        // 3. 正常再次刷新 t2 -> 获得 t3
        let r3 = manager.rotate(&t2).expect("Second rotation should succeed");
        assert_ne!(t2, r3.refresh_token);

        // 4. 模拟黑客窃取了已废弃的 t1，并发起重放刷新
        let replay_res = manager.rotate(&t1);
        assert!(replay_res.is_err(), "Replaying t1 must fail");

        // 5. 验证整个令牌族已被联级吊销：合法的 t3 也应被拒绝！
        let t3_res = manager.rotate(&r3.refresh_token);
        assert!(
            t3_res.is_err(),
            "Token family must be completely revoked after replay attempt"
        );
    }

    #[test]
    fn test_revoke_family_invalidates_future_rotations() {
        let manager = TokenFamilyManager::new(30);
        let t1 = manager.issue_initial(&"agent-alpha".to_string(), vec![Scope::Read], vec![]);

        manager.revoke_family(&t1);
        assert!(manager.rotate(&t1).is_err(), "撤销后不得再轮换");
    }
}
