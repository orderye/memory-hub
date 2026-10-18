//! 限流存储抽象
//!
//! 限流必须跨副本一致：令牌桶若只存在进程内存，多副本部署下每个副本
//! 各自放行 `capacity` 个请求，实际总放行量是 `capacity × 副本数`，
//! 限流形同虚设。因此把存储抽象出来，生产实现落到 PostgreSQL，
//! 用一条原子 SQL 完成"读取 → 按时间补充 → 扣减"，杜绝读改写竞态。

use crate::{HubError, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// 限流维度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDimension {
    Agent,
    User,
    Ip,
}

impl RateDimension {
    pub fn as_str(&self) -> &'static str {
        match self {
            RateDimension::Agent => "agent",
            RateDimension::User => "user",
            RateDimension::Ip => "ip",
        }
    }
}

/// 限流存储：尝试消耗 1 个令牌
#[async_trait]
pub trait RateLimitStore: Send + Sync {
    /// 默认窗口为 1 秒（即 capacity 次每秒）
    async fn try_acquire(
        &self,
        dimension: RateDimension,
        key: &str,
        capacity: u32,
    ) -> Result<bool> {
        self.try_acquire_in_window(dimension, key, capacity, 1)
            .await
    }

    /// 在指定窗口（秒）内最多放行 capacity 个请求
    async fn try_acquire_in_window(
        &self,
        dimension: RateDimension,
        key: &str,
        capacity: u32,
        window_seconds: u32,
    ) -> Result<bool>;
}

/// 默认的补充窗口（秒）：capacity 个令牌 / 1 秒 = `capacity` RPS
pub const REFILL_WINDOW_SECONDS: f64 = 1.0;

/// 删减用的公共校验：capacity 为 0 视为禁用该维度
pub fn is_capacity_enabled(capacity: u32) -> bool {
    capacity > 0
}

/// 供错误映射复用的统一入口
pub fn rate_limit_error() -> HubError {
    HubError::RateLimited {
        retry_after_s: Some(1),
    }
}

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

/// 进程内令牌桶存储（开发与测试环境使用，或单实例运行）
pub struct InMemoryRateLimitStore {
    buckets: Mutex<HashMap<String, BucketState>>,
}

impl Default for InMemoryRateLimitStore {
    fn default() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }
}

impl InMemoryRateLimitStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RateLimitStore for InMemoryRateLimitStore {
    async fn try_acquire_in_window(
        &self,
        dimension: RateDimension,
        key: &str,
        capacity: u32,
        window_seconds: u32,
    ) -> Result<bool> {
        if capacity == 0 {
            return Ok(true);
        }

        let cap = f64::from(capacity);
        let win = f64::from(window_seconds.max(1));
        let refill_rate = cap / win;
        let now = Instant::now();
        let bucket_key = format!("{}:{}", dimension.as_str(), key);

        let mut buckets = self.buckets.lock().unwrap();

        if buckets.len() > 10000 {
            buckets.retain(|_, b| now.saturating_duration_since(b.last_refill).as_secs() < 3600);
        }

        let bucket = buckets.entry(bucket_key).or_insert_with(|| BucketState {
            tokens: cap,
            last_refill: now,
        });

        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + refill_rate * elapsed).min(cap);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dimension_label() {
        assert_eq!(RateDimension::Agent.as_str(), "agent");
        assert_eq!(RateDimension::User.as_str(), "user");
        assert_eq!(RateDimension::Ip.as_str(), "ip");
    }

    #[test]
    fn test_zero_capacity_means_disabled() {
        assert!(!is_capacity_enabled(0));
        assert!(is_capacity_enabled(1));
    }

    #[tokio::test]
    async fn test_in_memory_rate_limit_store_window() {
        let store = InMemoryRateLimitStore::new();
        // 允许 2 次
        assert!(store
            .try_acquire_in_window(RateDimension::User, "alice", 2, 60)
            .await
            .unwrap());
        assert!(store
            .try_acquire_in_window(RateDimension::User, "alice", 2, 60)
            .await
            .unwrap());
        // 第 3 次超限
        assert!(!store
            .try_acquire_in_window(RateDimension::User, "alice", 2, 60)
            .await
            .unwrap());

        // 另一个 key 不受影响
        assert!(store
            .try_acquire_in_window(RateDimension::User, "bob", 2, 60)
            .await
            .unwrap());
    }
}
