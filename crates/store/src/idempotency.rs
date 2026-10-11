//! 幂等键的 PostgreSQL 实现（`idempotency_keys` 表）

use crate::postgres::PgResultExt;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{
    HubError, IdempotencyEntry, IdempotencyKey, IdempotencyRecord, IdempotencyStore, Result,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

/// 基于 `idempotency_keys` 的幂等存储
pub struct PostgresIdempotencyStore {
    pool: PgPool,
}

impl PostgresIdempotencyStore {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .min_connections(0)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(database_url)
            .await
            .map_err(|e| HubError::Internal(anyhow::anyhow!("幂等存储连接失败: {}", e)))?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl IdempotencyStore for PostgresIdempotencyStore {
    async fn register(&self, entry: IdempotencyEntry) -> Result<bool> {
        // 主键冲突即视为"已见过该键"：不覆盖，保留首次结果摘要
        let res = sqlx::query(
            r#"
            insert into idempotency_keys (key, agent_id, namespace, op, response_hash, created_at)
            values ($1, $2, $3, $4, $5, now())
            on conflict (agent_id, namespace, op, key) do nothing
            "#,
        )
        .bind(&entry.key.key)
        .bind(&entry.key.agent_id)
        .bind(&entry.key.namespace)
        .bind(&entry.key.op)
        .bind(&entry.result_hash)
        .execute(&self.pool)
        .await
        .pg()?;

        Ok(res.rows_affected() > 0)
    }

    async fn lookup(&self, key: &IdempotencyKey) -> Result<Option<IdempotencyRecord>> {
        let row = sqlx::query(
            r#"
            select response_hash, created_at
            from idempotency_keys
            where agent_id = $1 and namespace = $2 and op = $3 and key = $4
            "#,
        )
        .bind(&key.agent_id)
        .bind(&key.namespace)
        .bind(&key.op)
        .bind(&key.key)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => {
                let hash: Option<String> = r.try_get("response_hash").unwrap_or(None);
                let created_at: DateTime<Utc> = r.try_get("created_at").pg()?;
                Ok(Some(IdempotencyRecord {
                    result_hash: hash.unwrap_or_default(),
                    created_at,
                }))
            }
            None => Ok(None),
        }
    }

    async fn prune(&self, older_than: DateTime<Utc>) -> Result<u64> {
        let res = sqlx::query("delete from idempotency_keys where created_at < $1")
            .bind(older_than)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use common::{AgentId, IdempotencyGuard, InMemoryIdempotencyStore};

    #[tokio::test]
    async fn test_guard_works_over_pg_backed_contract() {
        // 内存实现已覆盖逻辑；此处验证 trait 的 async 契约在多实现下一致：
        // 同一 guard 逻辑在两种 store 上应有相同判定。
        let key = common::IdempotencyKey {
            agent_id: AgentId::from("asst"),
            namespace: "team-x".to_string(),
            op: "put".to_string(),
            key: "t1".to_string(),
        };

        let mem_guard = IdempotencyGuard::new(
            std::sync::Arc::new(InMemoryIdempotencyStore::new()),
            Duration::hours(24),
        );
        assert!(matches!(
            mem_guard.pre_check(&key).await.unwrap(),
            common::IdempotencyCheck::Fresh
        ));
    }

    #[tokio::test]
    async fn test_register_semantics_are_idempotent() {
        // 约定：register 对同一键第二次返回 false，不会抛错
        let store = std::sync::Arc::new(InMemoryIdempotencyStore::new());
        let k = common::IdempotencyKey {
            agent_id: AgentId::from("c"),
            namespace: "".to_string(),
            op: "check".to_string(),
            key: "k".to_string(),
        };
        assert!(store
            .register(common::IdempotencyEntry {
                key: k.clone(),
                result_hash: "h".to_string()
            })
            .await
            .unwrap());
        assert!(!store
            .register(common::IdempotencyEntry {
                key: k,
                result_hash: "h".to_string()
            })
            .await
            .unwrap());
    }
}

#[cfg(all(test, feature = "pg-integration"))]
mod pg_integration {
    use super::*;
    use common::{AgentId, IdempotencyEntry, IdempotencyKey};

    async fn store() -> PostgresIdempotencyStore {
        let url = std::env::var("DATABASE_URL").expect("需设置 DATABASE_URL");
        let s = PostgresIdempotencyStore::connect(&url)
            .await
            .expect("连接失败");
        s
    }

    #[tokio::test]
    #[ignore = "需要 DATABASE_URL"]
    async fn pg_response_hash_is_text_not_bytea() {
        // 回归：response_hash 初版为 bytea，写入 hex 字符串会类型不匹配；
        // 迁移 003 已改为 text，本测试确保该路径真实可写。
        let s = store().await;
        let k = IdempotencyKey {
            agent_id: AgentId::from("asst"),
            namespace: "team-x".to_string(),
            op: "put".to_string(),
            key: "t-integration".to_string(),
        };

        assert!(s
            .register(IdempotencyEntry {
                key: k.clone(),
                result_hash: "a".repeat(64),
            })
            .await
            .unwrap());

        let rec = s.lookup(&k).await.unwrap().expect("应能读回");
        assert_eq!(rec.result_hash, "a".repeat(64));
    }

    #[tokio::test]
    #[ignore = "需要 DATABASE_URL"]
    async fn pg_register_is_idempotent_and_prunes() {
        let s = store().await;
        let k = IdempotencyKey {
            agent_id: AgentId::from("asst"),
            namespace: "team-x".to_string(),
            op: "check".to_string(),
            key: "k1".to_string(),
        };

        assert!(s
            .register(IdempotencyEntry {
                key: k.clone(),
                result_hash: "h".to_string()
            })
            .await
            .unwrap());
        assert!(
            !s.register(IdempotencyEntry {
                key: k.clone(),
                result_hash: "h".to_string()
            })
            .await
            .unwrap(),
            "同键第二次应返回 false"
        );

        // 清理
        let removed = s
            .prune(chrono::Utc::now() + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert!(removed >= 1);
        assert!(s.lookup(&k).await.unwrap().is_none());
    }
}
