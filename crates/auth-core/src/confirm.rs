//! 敏感操作二次确认令牌管理器（120s 有效，一次性使用）

use chrono::{DateTime, Duration, Utc};
use common::{AgentId, HubError, Result};
use std::collections::HashMap;
use std::sync::RwLock;
use uuid::Uuid;

struct StoredConfirmToken {
    agent_id: AgentId,
    op: String,
    expires_at: DateTime<Utc>,
}

/// 确认令牌管理器
pub struct ConfirmTokenManager {
    tokens: RwLock<HashMap<String, StoredConfirmToken>>,
    ttl_seconds: i64,
}

impl ConfirmTokenManager {
    pub fn new(ttl_seconds: i64) -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
            ttl_seconds,
        }
    }

    /// 生成一次性确认令牌
    pub fn generate(&self, agent_id: &AgentId, op: &str) -> String {
        let mut write = self.tokens.write().unwrap();
        let token = format!("cnf_{}", Uuid::new_v4().simple());
        let expires_at = Utc::now() + Duration::seconds(self.ttl_seconds);

        write.insert(
            token.clone(),
            StoredConfirmToken {
                agent_id: agent_id.clone(),
                op: op.to_string(),
                expires_at,
            },
        );

        token
    }

    /// 校验并立即销毁令牌（单次使用即焚）
    pub fn verify_and_consume(&self, token: &str, agent_id: &AgentId, op: &str) -> Result<()> {
        let mut write = self.tokens.write().unwrap();
        let stored = write.remove(token).ok_or(HubError::ConfirmExpired)?;

        if &stored.agent_id != agent_id || stored.op != op {
            return Err(HubError::ConfirmExpired);
        }

        if stored.expires_at <= Utc::now() {
            return Err(HubError::ConfirmExpired);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_confirm_token_lifecycle() {
        let manager = ConfirmTokenManager::new(120);
        let agent: AgentId = "agent-a".to_string();

        let token = manager.generate(&agent, "restore");

        // 首次校验成功
        assert!(manager
            .verify_and_consume(&token, &agent, "restore")
            .is_ok());

        // 二次使用应当失败（一次性）
        assert!(manager
            .verify_and_consume(&token, &agent, "restore")
            .is_err());
    }

    #[test]
    fn test_wrong_operation_or_agent_rejected() {
        let manager = ConfirmTokenManager::new(120);
        let agent_a: AgentId = "agent-a".to_string();
        let agent_b: AgentId = "agent-b".to_string();

        let token = manager.generate(&agent_a, "restore");

        // 冒用者校验失败
        assert!(manager
            .verify_and_consume(&token, &agent_b, "restore")
            .is_err());
    }
}
