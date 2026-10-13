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

/// 基于 PostgreSQL 的限流存储
pub struct PostgresRateLimitStore {
    pool: PgPool,
}

impl PostgresRateLimitStore {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .min_connections(0)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(database_url)
            .await
            .map_err(|e| common::HubError::Internal(anyhow::anyhow!("限流存储连接失败: {}", e)))?;

        Ok(Self { pool })
    }

    /// 复用业务存储已有的连接池，避免每个进程再开一个独立池
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// 组装「按时间补充 → 扣减」的原子 SQL。
///
/// 补充速率必须是 `capacity` 令牌/秒——即 `RATE_LIMIT_PER_AGENT_RPS=50` 表示
/// 稳态 50 QPS。此前这里硬编码为 1.0 令牌/秒，而文档与内存实现都写作 capacity，
/// 于是配置 50 实际只放行 1 QPS，且与 `InMemoryRateLimitStore` 行为相差 capacity 倍。
///
/// `capacity` 是服务端配置解析出的 f64，直接内插；`dimension`/`key` 来自调用方，
/// 一律参数绑定。
pub(crate) fn acquire_sql(cap: f64, window_seconds: f64) -> String {
    let rate = cap / window_seconds.max(1.0);
    let refilled = format!(
        "least({cap}::double precision, rate_limit_buckets.tokens \
         + extract(epoch from (now() - rate_limit_buckets.updated_at))::double precision * {rate})",
    );

    format!(
        r#"
        insert into rate_limit_buckets (dimension, bucket_key, tokens, updated_at)
        values ($1, $2, ({cap})::double precision - 1.0, now())
        on conflict (dimension, bucket_key) do update set
            tokens = ({refilled}) - 1.0,
            updated_at = now()
        where ({refilled}) >= 1.0
        returning true as acquired
        "#,
    )
}

#[async_trait]
impl RateLimitStore for PostgresRateLimitStore {
    async fn try_acquire_in_window(
        &self,
        dimension: RateDimension,
        key: &str,
        capacity: u32,
        window_seconds: u32,
    ) -> Result<bool> {
        if capacity == 0 {
            return Ok(true); // 容量 0 表示不限流
        }

        let sql = acquire_sql(f64::from(capacity), f64::from(window_seconds.max(1)));

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

    #[test]
    fn test_refill_rate_follows_capacity() {
        // 回归护栏：补充速率必须等于 capacity，不得回退成固定 1.0
        let sql = acquire_sql(50.0, 1.0);
        let elapsed_term = sql
            .split(
                "extract(epoch from (now() - rate_limit_buckets.updated_at))::double precision *",
            )
            .nth(1)
            .expect("SQL 应包含按时间补充项")
            .trim()
            .split(')')
            .next()
            .expect("补充项系数应闭合")
            .trim()
            .to_string();
        assert_eq!(elapsed_term, "50", "补充速率必须为 capacity 令牌/秒");
    }

    #[test]
    fn test_caller_supplied_values_are_bound_not_interpolated() {
        // dimension / bucket_key 可能来自请求，必须以 $1/$2 绑定出现
        let sql = acquire_sql(10.0, 1.0);
        assert!(sql.contains("values ($1, $2"), "dimension/key 必须参数绑定");
        let nasty = "agent'; drop table memories; --";
        assert!(!sql.contains(nasty), "拼出来的 SQL 不得含有调用方可控文本");
    }
}
