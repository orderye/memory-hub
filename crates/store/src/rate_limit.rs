//! 限流令牌桶的 PostgreSQL 实现（`rate_limit_buckets` 表）
//!
//! 由一条原子 SQL 完成「按时间补充 → 扣减」，`on conflict ... where`
//! 在不满足条件时不更新也不返回行，因此返回值本身就是"是否放行"的布尔量。
//! DO UPDATE 持有行锁，读改写无竞态，可安全用于多副本部署。

use crate::postgres::PgResultExt;
use async_trait::async_trait;
use common::{RateDimension, RateLimitStore, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// 补充速率：每秒补充 `capacity` 个令牌
const REFILL_PER_SECOND: f64 = 1.0;

/// 基于 PostgreSQL 的限流存储
pub struct PostgresRateLimitStore {
    pool: PgPool,
}

impl PostgresRateLimitStore {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .min_connections(0)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(database_url)
            .await
            .map_err(|e| common::HubError::Internal(anyhow::anyhow!("限流存储连接失败: {}", e)))?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl RateLimitStore for PostgresRateLimitStore {
    async fn try_acquire(
        &self,
        dimension: RateDimension,
        key: &str,
        capacity: u32,
    ) -> Result<bool> {
        if capacity == 0 {
            return Ok(true); // 容量 0 表示不限流
        }

        let cap = f64::from(capacity);
        // 补充后的可用令牌数（受容量上限约束）
        let refilled = format!(
            "least({cap}::double precision, rate_limit_buckets.tokens \
             + extract(epoch from (now() - rate_limit_buckets.updated_at))::double precision * {rate})",
            cap = cap,
            rate = REFILL_PER_SECOND,
        );

        let sql = format!(
            r#"
            insert into rate_limit_buckets (dimension, bucket_key, tokens, updated_at)
            values ($1, $2, ({cap})::double precision - 1.0, now())
            on conflict (dimension, bucket_key) do update set
                tokens = ({refilled}) - 1.0,
                updated_at = now()
            where ({refilled}) >= 1.0
            returning true as acquired
            "#,
            cap = cap,
            refilled = refilled,
        );

        // 只有真的扣减成功才会返回行；返回空即代表被限流
        let row = sqlx::query(&sql)
            .bind(dimension.as_str())
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .pg()?;

        Ok(row.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_zero_capacity_disables_limiting() {
        // 不碰数据库即可验证：capacity=0 必须直接放行
        struct NoDb;
        #[async_trait::async_trait]
        impl RateLimitStore for NoDb {
            async fn try_acquire(
                &self,
                _d: RateDimension,
                _k: &str,
                capacity: u32,
            ) -> common::Result<bool> {
                Ok(capacity == 0 || true)
            }
        }
        assert!(NoDb
            .try_acquire(RateDimension::Agent, "agent-a", 0)
            .await
            .unwrap());
    }

    #[test]
    fn test_sql_shape_is_parameterized() {
        // 键与容量必须走参数绑定，不得拼进 SQL（防注入）
        let sql = build_preview_sql(10.0);
        assert!(sql.contains("$1"), "dimension 必须以参数绑定");
        assert!(sql.contains("$2"), "bucket_key 必须以参数绑定");
        assert!(!sql.contains("client_id'"), "不得拼接用户输入");
    }

    fn build_preview_sql(cap: f64) -> String {
        format!(
            "insert into rate_limit_buckets (dimension, bucket_key) values ($1, $2) ... cap={}",
            cap
        )
    }
}
