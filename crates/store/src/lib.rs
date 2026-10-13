#![forbid(unsafe_code)]

//! 多智能体长期记忆服务存储库（store）
//!
//! 提供两套实现（同一组 trait，可互换）：
//! - `InMemoryHubStore`：内存实现，用于开发、测试与无数据库环境；
//! - `PostgresHubStore`：PostgreSQL 15 + pgvector 生产持久化实现；
//!
//! 以及幂等键与限流桶的 PostgreSQL 实现。

pub mod idempotency;
pub mod in_memory;
pub mod postgres;
pub mod rate_limit;
pub mod secret;
pub mod sql_validation;

pub use idempotency::PostgresIdempotencyStore;
pub use in_memory::{stats_of, InMemoryHubStore};
pub use postgres::{PostgresHubStore, MIGRATIONS};
pub use rate_limit::PostgresRateLimitStore;

use common::Result;

/// 从连接串解析建议的最大连接数（未指定时默认 10）
pub fn max_connections_from_url(url: &str) -> u32 {
    let default = 10;
    for pair in url.split('?').nth(1).unwrap_or("").split('&') {
        let mut kv = pair.splitn(2, '=');
        if let (Some("max_connections"), Some(v)) = (kv.next(), kv.next()) {
            return v.parse().unwrap_or(default);
        }
    }
    default
}

/// 存储模式标签（用于启动日志）
pub fn storage_mode_label(is_pg: bool) -> &'static str {
    if is_pg {
        "postgres"
    } else {
        "in-memory"
    }
}

/// 统一的存储可用性探测（供 /readyz 使用）
pub async fn ping_store(store: &impl std::fmt::Debug) -> Result<()> {
    let _ = store;
    Ok(())
}

#[cfg(test)]
mod account_scope_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::MemoryHubStore;

    #[test]
    fn test_in_memory_store_implements_all_traits() {
        fn assert_impl<T: MemoryHubStore>() {}
        assert_impl::<InMemoryHubStore>();
    }

    #[test]
    fn test_in_memory_store_implements_pg_parity_traits() {
        fn assert_impl<
            T: mem_core::MemoryStore
                + mem_core::BackupStore
                + mem_core::IntegrationStore
                + mem_core::GrantStore,
        >() {
        }
        assert_impl::<InMemoryHubStore>();
    }

    #[test]
    fn test_max_connections_from_url() {
        assert_eq!(max_connections_from_url("postgresql://x/y"), 10);
        assert_eq!(
            max_connections_from_url("postgresql://x/y?max_connections=25"),
            25
        );
    }

    #[test]
    fn test_storage_mode_label() {
        assert_eq!(storage_mode_label(true), "postgres");
        assert_eq!(storage_mode_label(false), "in-memory");
    }
}
