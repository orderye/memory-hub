use super::*;

/// Agent 记忆集合的 PostgreSQL 实现（账户级配置，与命名空间前缀无关）
#[async_trait]
impl CollectionStore for PostgresHubStore {
    async fn upsert_collection(&self, collection: &MemoryCollection) -> Result<MemoryCollection> {
        let row = sqlx::query(
            r#"
            insert into memory_collections (
                id, account_id, name, description, filter, created_by, created_at, updated_at
            )
            values ($1, $2, $3, $4, $5, $6, $7, $7)
            on conflict (account_id, name) do update set
                description = excluded.description,
                filter = excluded.filter,
                updated_at = now()
            returning id, account_id, name, description, filter, created_by, created_at, updated_at
            "#,
        )
        .bind(collection.id)
        .bind(collection.account_id)
        .bind(&collection.name)
        .bind(&collection.description)
        .bind(serde_json::to_value(&collection.filter).unwrap_or_else(|_| serde_json::json!({})))
        .bind(&collection.created_by)
        .bind(collection.created_at)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::collection_from_row(&row)
    }

    async fn get_collection(
        &self,
        account_id: AccountId,
        id: Uuid,
    ) -> Result<Option<MemoryCollection>> {
        let row = sqlx::query(
            r#"
            select id, account_id, name, description, filter, created_by, created_at, updated_at
            from memory_collections
            where account_id = $1 and id = $2
            "#,
        )
        .bind(account_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.map(|r| Self::collection_from_row(&r)).transpose()
    }

    async fn get_collection_by_name(
        &self,
        account_id: AccountId,
        name: &str,
    ) -> Result<Option<MemoryCollection>> {
        let row = sqlx::query(
            r#"
            select id, account_id, name, description, filter, created_by, created_at, updated_at
            from memory_collections
            where account_id = $1 and name = $2
            "#,
        )
        .bind(account_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.map(|r| Self::collection_from_row(&r)).transpose()
    }

    async fn list_collections(&self, account_id: AccountId) -> Result<Vec<MemoryCollection>> {
        let rows = sqlx::query(
            r#"
            select id, account_id, name, description, filter, created_by, created_at, updated_at
            from memory_collections
            where account_id = $1
            order by updated_at desc, name asc
            "#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::collection_from_row).collect()
    }

    async fn delete_collection(&self, account_id: AccountId, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from memory_collections where account_id = $1 and id = $2")
            .bind(account_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn bind_collection(&self, binding: &CollectionBinding) -> Result<()> {
        sqlx::query(
            r#"
            insert into memory_collection_bindings (collection_id, agent_id, created_by, created_at)
            values ($1, $2, $3, $4)
            on conflict (collection_id, agent_id) do nothing
            "#,
        )
        .bind(binding.collection_id)
        .bind(&binding.agent_id)
        .bind(&binding.created_by)
        .bind(binding.created_at)
        .execute(&self.pool)
        .await
        .pg()?;

        Ok(())
    }

    async fn unbind_collection(&self, collection_id: Uuid, agent_id: &AgentId) -> Result<()> {
        let res = sqlx::query(
            "delete from memory_collection_bindings where collection_id = $1 and agent_id = $2",
        )
        .bind(collection_id)
        .bind(agent_id)
        .execute(&self.pool)
        .await
        .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn list_bindings(&self, collection_id: Uuid) -> Result<Vec<CollectionBinding>> {
        let rows = sqlx::query(
            r#"
            select collection_id, agent_id, created_by, created_at
            from memory_collection_bindings
            where collection_id = $1
            order by created_at asc, agent_id asc
            "#,
        )
        .bind(collection_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::collection_binding_from_row).collect()
    }

    async fn is_bound(&self, collection_id: Uuid, agent_id: &AgentId) -> Result<bool> {
        let exists: bool = sqlx::query_scalar(
            "select exists(select 1 from memory_collection_bindings where collection_id = $1 and agent_id = $2)",
        )
        .bind(collection_id)
        .bind(agent_id)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Ok(exists)
    }
}
