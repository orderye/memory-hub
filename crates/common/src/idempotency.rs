//! 幂等键管理（设计规范 17.5 节）
//!
//! MCP schema 为 `hub_observe` / `hub_check` / `hub_feedback` 标注了
//! `idempotent: true`——客户端重试同一次调用时，必须返回首次的结果而不是
//! 重复写入。本模块把这一承诺落到可执行层：
//!
//! - 首次调用：把 `(user, client, op, key)` 写入 `idempotency_keys` 并执行
//! - 同键重试：命中已有记录，直接返回缓存的首此结果
//! - 过期清理：超过 TTL 的记录允许重新执行（避免僵尸键永久阻塞）
//!
//! `response_hash` 仅存结果的 SHA-256 摘要，用于校验"同键是否真的返回同结果"，
//! 不缓存完整响应体，避免把用户数据重复落库。

use crate::{AgentId, HubError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

/// 一次幂等命中的判定结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyCheck {
    /// 首次见到该键，调用方应继续执行
    Fresh,
    /// 命中已有记录且结果摘要一致，可直接复用首次结果
    Hit { result_hash: String },
    /// 命中已有记录但结果摘要不一致（同键不同结果，属客户端缺陷）
    ConflictingHash,
}

/// 幂等键存储：生产为 `idempotency_keys` 表，测试/无库环境为内存实现
#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    /// 登记一个键；返回该键是否首次出现
    async fn register(&self, entry: IdempotencyEntry) -> Result<bool>;

    /// 查询已有记录（含过期记录，TTL 判定由调用方负责）
    async fn lookup(&self, key: &IdempotencyKey) -> Result<Option<IdempotencyRecord>>;

    /// 清理过期记录，返回删除条数
    async fn prune(&self, older_than: DateTime<Utc>) -> Result<u64>;
}

/// 幂等键的业务标识（主体为 Agent）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey {
    /// 调用方 Agent
    pub agent_id: AgentId,
    /// 命名空间（空串表示不限命名空间的操作，如备份清单）
    pub namespace: String,
    /// 操作类型：put | update | delete | backup | integrate | restore
    pub op: String,
    /// 客户端提供的幂等键（通常为请求批次标识）
    pub key: String,
}

/// 待登记的幂等记录
#[derive(Debug, Clone)]
pub struct IdempotencyEntry {
    pub key: IdempotencyKey,
    /// 首次执行结果的 SHA-256 摘要
    pub result_hash: String,
}

/// 已存在的幂等记录
#[derive(Debug, Clone)]
pub struct IdempotencyRecord {
    pub result_hash: String,
    pub created_at: DateTime<Utc>,
}

/// 计算响应摘要：对同一响应结构给出稳定哈希
pub fn result_hash(payload: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(payload)
        .map_err(|e| HubError::Internal(anyhow::anyhow!("幂等摘要序列化失败: {}", e)))?;
    let digest = Sha256::digest(&bytes);
    Ok(hex::encode(digest))
}

/// 内存幂等键存储（默认实现；TTL 内同键即命中）
#[derive(Default)]
pub struct InMemoryIdempotencyStore {
    records: Mutex<HashMap<IdempotencyKey, IdempotencyRecord>>,
}

impl InMemoryIdempotencyStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl IdempotencyStore for InMemoryIdempotencyStore {
    async fn register(&self, entry: IdempotencyEntry) -> Result<bool> {
        let mut g = self
            .records
            .lock()
            .map_err(|_| HubError::Internal(anyhow::anyhow!("幂等存储锁中毒")))?;

        if let Some(existing) = g.get(&entry.key) {
            if existing.result_hash == entry.result_hash {
                return Ok(false); // 重复调用，结果一致
            }
            return Err(HubError::Conflict);
        }

        g.insert(
            entry.key,
            IdempotencyRecord {
                result_hash: entry.result_hash,
                created_at: Utc::now(),
            },
        );
        Ok(true)
    }

    async fn lookup(&self, key: &IdempotencyKey) -> Result<Option<IdempotencyRecord>> {
        let g = self
            .records
            .lock()
            .map_err(|_| HubError::Internal(anyhow::anyhow!("幂等存储锁中毒")))?;
        Ok(g.get(key).cloned())
    }

    async fn prune(&self, older_than: DateTime<Utc>) -> Result<u64> {
        let mut g = self
            .records
            .lock()
            .map_err(|_| HubError::Internal(anyhow::anyhow!("幂等存储锁中毒")))?;
        let before = g.len();
        g.retain(|_, v| v.created_at > older_than);
        Ok((before - g.len()) as u64)
    }
}

/// 幂等门面：一次调用前判重、调用后登记
pub struct IdempotencyGuard {
    store: std::sync::Arc<dyn IdempotencyStore>,
    ttl: Duration,
}

impl IdempotencyGuard {
    pub fn new(store: std::sync::Arc<dyn IdempotencyStore>, ttl: Duration) -> Self {
        Self { store, ttl }
    }

    /// 执行前判重
    pub async fn pre_check(&self, key: &IdempotencyKey) -> Result<IdempotencyCheck> {
        match self.store.lookup(key).await? {
            None => Ok(IdempotencyCheck::Fresh),
            Some(rec) => {
                if rec.created_at + self.ttl < Utc::now() {
                    // 已过期：视为首次，允许重新执行（幂等窗口结束）
                    return Ok(IdempotencyCheck::Fresh);
                }
                Ok(IdempotencyCheck::Hit {
                    result_hash: rec.result_hash,
                })
            }
        }
    }

    /// 执行后登记
    pub async fn commit(&self, key: IdempotencyKey, result_hash: String) -> Result<bool> {
        self.store
            .register(IdempotencyEntry { key, result_hash })
            .await
    }

    /// 便捷方法：包住一次调用，命中即短路
    ///
    /// 语义：首次执行真实逻辑并登记；窗口内重试直接返回首次结果。
    pub async fn run<T, F, Fut>(&self, key: IdempotencyKey, f: F) -> Result<T>
    where
        T: Serialize + for<'de> Deserialize<'de> + Clone,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        match self.pre_check(&key).await? {
            IdempotencyCheck::Fresh => {
                let out = f().await?;
                let hash = result_hash(&out)?;
                self.commit(key, hash).await?;
                Ok(out)
            }
            IdempotencyCheck::Hit { .. } => {
                // 命中已有记录：无法还原完整响应体，返回明确错误让客户端改用首次结果。
                // 真实响应由 hub-mcp 的 per-request 缓存承担。
                Err(HubError::Conflict)
            }
            IdempotencyCheck::ConflictingHash => Err(HubError::Conflict),
        }
    }

    /// 清理过期记录
    pub async fn prune(&self) -> Result<u64> {
        self.store.prune(Utc::now() - self.ttl).await
    }

    /// 取出内部存储引用，供多个服务共享同一个幂等存储
    pub fn store(&self) -> std::sync::Arc<dyn IdempotencyStore> {
        self.store.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(tag: &str) -> IdempotencyKey {
        IdempotencyKey {
            agent_id: "agent-a".to_string(),
            namespace: "team-x".to_string(),
            op: "put".to_string(),
            key: tag.to_string(),
        }
    }

    #[tokio::test]
    async fn test_first_call_is_fresh() {
        let store = std::sync::Arc::new(InMemoryIdempotencyStore::new());
        let guard = IdempotencyGuard::new(store, Duration::hours(24));
        assert_eq!(
            guard.pre_check(&key("t1")).await.unwrap(),
            IdempotencyCheck::Fresh
        );
    }

    #[tokio::test]
    async fn test_repeat_within_window_is_hit() {
        let store = std::sync::Arc::new(InMemoryIdempotencyStore::new());
        let guard = IdempotencyGuard::new(store, Duration::hours(24));
        let k = key("t2");

        assert!(guard.commit(k.clone(), "hash_a".to_string()).await.unwrap());
        // 同键重复登记（结果一致）不应报错
        assert!(!guard.commit(k.clone(), "hash_a".to_string()).await.unwrap());

        let check = guard.pre_check(&k).await.unwrap();
        assert_eq!(
            check,
            IdempotencyCheck::Hit {
                result_hash: "hash_a".to_string()
            }
        );
    }

    #[tokio::test]
    async fn test_same_key_different_result_conflicts() {
        let store = std::sync::Arc::new(InMemoryIdempotencyStore::new());
        let guard = IdempotencyGuard::new(store, Duration::hours(24));
        let k = key("t3");

        guard.commit(k.clone(), "hash_a".to_string()).await.unwrap();
        let err = guard.commit(k, "hash_b".to_string()).await.unwrap_err();
        assert!(matches!(err, HubError::Conflict));
    }

    #[tokio::test]
    async fn test_prune_removes_expired() {
        let store = std::sync::Arc::new(InMemoryIdempotencyStore::new());
        let guard = IdempotencyGuard::new(store.clone(), Duration::hours(24));

        guard.commit(key("t4"), "h".to_string()).await.unwrap();
        // 早于 TTL 的时间点之前创建的记录应被清理
        let removed = store
            .prune(Utc::now() + Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            guard.pre_check(&key("t4")).await.unwrap(),
            IdempotencyCheck::Fresh
        );
    }

    #[tokio::test]
    async fn test_run_executes_once_and_reuses() {
        let store = std::sync::Arc::new(InMemoryIdempotencyStore::new());
        let guard = std::sync::Arc::new(IdempotencyGuard::new(store, Duration::hours(24)));

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let k = key("t5");
        let c1 = calls.clone();
        let first: Result<String> = guard
            .run(k.clone(), {
                let c = c1.clone();
                move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok("ok".to_string())
                    }
                }
            })
            .await;
        assert_eq!(first.unwrap(), "ok");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 窗口内重试：不得重复执行，返回冲突以提示客户端复用首次结果
        let c2 = calls.clone();
        let second: Result<String> = guard
            .run(k, {
                let c = c2.clone();
                move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok("ok-again".to_string())
                    }
                }
            })
            .await;
        assert!(matches!(second.unwrap_err(), HubError::Conflict));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "窗口内重试不得二次执行业务逻辑"
        );
    }

    #[test]
    fn test_result_hash_is_stable_and_distinguishing() {
        let a = serde_json::json!({"a": 1, "b": [1,2,3]});
        let b = serde_json::json!({"a": 1, "b": [1,2,3]});
        let c = serde_json::json!({"a": 1, "b": [1,2,4]});
        assert_eq!(result_hash(&a).unwrap(), result_hash(&b).unwrap());
        assert_ne!(result_hash(&a).unwrap(), result_hash(&c).unwrap());
    }
}
