//! 限流存储抽象
//!
//! 限流必须跨副本一致：令牌桶若只存在进程内存，多副本部署下每个副本
//! 各自放行 `capacity` 个请求，实际总放行量是 `capacity × 副本数`，
//! 限流形同虚设。因此把存储抽象出来，生产实现落到 PostgreSQL，
//! 用一条原子 SQL 完成"读取 → 按时间补充 → 扣减"，杜绝读改写竞态。

use crate::{HubError, Result};
use async_trait::async_trait;

/// 限流维度：按调用方 Agent 计桶
///
/// 多智能体场景下每个 Agent（OAuth client）都是独立配额主体，
/// 单个 Agent 的突发流量不得影响其他 Agent。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDimension {
    Agent,
}

impl RateDimension {
    pub fn as_str(&self) -> &'static str {
        match self {
            RateDimension::Agent => "agent",
        }
    }
}

/// 限流存储：尝试消耗 1 个令牌
///
/// `capacity` 为桶容量（个），补充速率为每秒 `capacity` 个，
/// 即"每秒最多 capacity 次请求"的语义。
#[async_trait]
pub trait RateLimitStore: Send + Sync {
    async fn try_acquire(&self, dimension: RateDimension, key: &str, capacity: u32)
        -> Result<bool>;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dimension_label() {
        assert_eq!(RateDimension::Agent.as_str(), "agent");
    }

    #[test]
    fn test_zero_capacity_means_disabled() {
        assert!(!is_capacity_enabled(0));
        assert!(is_capacity_enabled(1));
    }
}
