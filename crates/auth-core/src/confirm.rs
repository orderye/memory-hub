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

/// 写入路径惰性淘汰阈值：条目数超过它时顺带清理已过期记录
/// （与 `common::rate_limit` 内存令牌桶的做法一致）
const LAZY_PRUNE_THRESHOLD: usize = 10_000;

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

        if write.len() > LAZY_PRUNE_THRESHOLD {
            let now = Utc::now();
            write.retain(|_, t| t.expires_at > now);
        }

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

    /// 清理已过期的确认令牌，返回清理条数
    ///
    /// 令牌是一次性的：正常路径在消费时即被移除，这里的对象是
    /// "签发后从未被消费"的滞留令牌（TTL 120s，量级天然有限）。
    pub fn prune_expired(&self, now: DateTime<Utc>) -> usize {
        let mut write = self.tokens.write().unwrap();
        let before = write.len();
        write.retain(|_, t| t.expires_at > now);
        before - write.len()
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

    #[test]
    fn test_prune_expired_removes_only_expired() {
        let manager = ConfirmTokenManager::new(120);
        let now = Utc::now();
        let agent: AgentId = "agent-a".to_string();

        // 直接种入过期与未过期令牌各一（TTL 120s，用内部结构构造混合状态）
        {
            let mut write = manager.tokens.write().unwrap();
            for (name, expires_at) in [
                ("cnf_expired", now - Duration::seconds(1)),
                ("cnf_alive", now + Duration::seconds(60)),
            ] {
                write.insert(
                    name.to_string(),
                    StoredConfirmToken {
                        agent_id: agent.clone(),
                        op: "restore".to_string(),
                        expires_at,
                    },
                );
            }
        }

        assert_eq!(manager.prune_expired(now), 1);
        // 过期的已被清掉（校验报过期错误），未过期的不受影响
        assert!(manager
            .verify_and_consume("cnf_expired", &agent, "restore")
            .is_err());
        assert!(manager
            .verify_and_consume("cnf_alive", &agent, "restore")
            .is_ok());
        assert_eq!(manager.prune_expired(now), 0, "二次清扫应无残留");
    }

    #[test]
    fn test_generate_prunes_lazily_above_threshold() {
        let manager = ConfirmTokenManager::new(120);
        let now = Utc::now();

        {
            let mut write = manager.tokens.write().unwrap();
            for i in 0..=LAZY_PRUNE_THRESHOLD {
                write.insert(
                    format!("cnf_stale_{i}"),
                    StoredConfirmToken {
                        agent_id: "agent-a".to_string(),
                        op: "restore".to_string(),
                        expires_at: now - Duration::seconds(1),
                    },
                );
            }
        }

        let agent: AgentId = "agent-fresh".to_string();
        let fresh = manager.generate(&agent, "restore");
        // 超阈值触发惰性清理：过期全清，只剩新签发的一条
        assert_eq!(manager.tokens.read().unwrap().len(), 1);
        assert!(manager
            .verify_and_consume(&fresh, &agent, "restore")
            .is_ok());
    }
}
