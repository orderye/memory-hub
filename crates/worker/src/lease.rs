//! 后台作业跨进程租约
//!
//! 多副本同时运行 `backup-worker` 时，每个副本都会把同一命名空间扫一遍：
//! 提案本身是幂等的（不会重复落库），但 CPU、数据库往返与嵌入调用会成倍放大。
//! 生产侧完全可以把副本数限制为 1，但"靠部署纪律保证"不是工程答案。
//!
//! 这里复用现成的 `RateLimitStore`（生产为 PostgreSQL `rate_limit_buckets` 表）：
//! 以 `capacity=1`、`window=租约窗口` 的令牌桶做"窗口内至多放行一次"，
//! 一次原子 SQL 就完成跨副本互斥——**不需要新增表结构，也不需要新迁移**。
//!
//! 语义取舍：
//! - 租约按**作业 + 命名空间**（或全局）划分，粒度足够细，不会让一个慢命名空间拖住其它；
//! - 作业失败时租约不会提前释放（窗口继续计时），因此失败只会在下个窗口重试，
//!   不会在两个副本之间来回抖动；这比"抢锁失败就整轮跳过"更容易推理；
//! - `window_hours = 0` 表示显式关闭租约（单机/Lite 手动执行场景）。

use common::{RateDimension, RateLimitStore, Result};
use std::sync::Arc;

/// 作业租约
#[derive(Clone)]
pub struct JobLease {
    store: Arc<dyn RateLimitStore>,
    window_seconds: u32,
}

impl JobLease {
    /// 按小时窗口构造；0 小时表示关闭租约（返回 None，调用方不加租约）
    pub fn from_hours(store: Arc<dyn RateLimitStore>, window_hours: u32) -> Option<Self> {
        if window_hours == 0 {
            return None;
        }
        // u32 秒上限约 136 年，24h 窗口不会溢出
        let seconds = window_hours.saturating_mul(3600);
        Some(Self {
            store,
            window_seconds: seconds.max(1),
        })
    }

    /// 按秒窗口构造（测试用）
    pub fn from_seconds(store: Arc<dyn RateLimitStore>, window_seconds: u32) -> Self {
        Self {
            store,
            window_seconds: window_seconds.max(1),
        }
    }

    /// 租约窗口（秒）
    pub fn window_seconds(&self) -> u32 {
        self.window_seconds
    }

    /// 尝试取得租约：`true` = 本窗口内首次，可以执行；`false` = 已被别的副本取走
    pub async fn try_acquire(&self, key: &str) -> Result<bool> {
        self.store
            .try_acquire_in_window(RateDimension::Job, key, 1, self.window_seconds)
            .await
    }

    /// 全局作业键（备份 / 过期清理 / 证据清理）
    pub fn global_key(job: &str) -> String {
        format!("job:{job}")
    }

    /// 命名空间级作业键（整合扫描）
    pub fn namespace_key(job: &str, namespace: &str) -> String {
        format!("job:{job}:{namespace}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::InMemoryRateLimitStore;

    fn lease(seconds: u32) -> JobLease {
        JobLease::from_seconds(Arc::new(InMemoryRateLimitStore::new()), seconds)
    }

    #[tokio::test]
    async fn test_lease_allows_only_once_per_window() {
        let l = lease(3600);
        let key = JobLease::global_key("backup");
        assert!(l.try_acquire(&key).await.unwrap(), "首次必须放行");
        assert!(!l.try_acquire(&key).await.unwrap(), "窗口内第二次必须拒绝");
    }

    #[tokio::test]
    async fn test_lease_is_scoped_per_key() {
        let l = lease(3600);
        assert!(l
            .try_acquire(&JobLease::namespace_key("integration-scan", "ns-a"))
            .await
            .unwrap());
        assert!(l
            .try_acquire(&JobLease::namespace_key("integration-scan", "ns-b"))
            .await
            .unwrap());
        assert!(!l
            .try_acquire(&JobLease::namespace_key("integration-scan", "ns-a"))
            .await
            .unwrap());
    }

    #[test]
    fn test_zero_hours_disables_lease() {
        let store: Arc<dyn RateLimitStore> = Arc::new(InMemoryRateLimitStore::new());
        assert!(JobLease::from_hours(store, 0).is_none());
    }

    #[test]
    fn test_window_conversion() {
        let store: Arc<dyn RateLimitStore> = Arc::new(InMemoryRateLimitStore::new());
        let l = JobLease::from_hours(store, 24).unwrap();
        assert_eq!(l.window_seconds(), 86_400);
    }
}
