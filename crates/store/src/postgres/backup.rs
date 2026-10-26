use super::*;

#[async_trait]
impl mem_core::BackupStore for PostgresHubStore {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()> {
        sqlx::query(
            r#"
            insert into memory_backups
              (id, namespace, created_by, note, entry_count, checksum, payload, retain_until, created_at)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            on conflict (id) do update set
              note = excluded.note, entry_count = excluded.entry_count,
              checksum = excluded.checksum, payload = excluded.payload,
              retain_until = excluded.retain_until
            "#,
        )
        .bind(backup.id)
        .bind(&backup.namespace)
        .bind(&backup.created_by)
        .bind(&backup.note)
        .bind(backup.entry_count as i32)
        .bind(&backup.checksum)
        .bind(&backup.payload)
        .bind(backup.retain_until)
        .bind(backup.created_at)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
        let row = sqlx::query(
            r#"
            select id, namespace, created_by, note, entry_count, checksum, payload, retain_until, created_at
            from memory_backups where id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?
        .ok_or(HubError::NotFound)?;
        Self::backup_from_row(&row)
    }

    /// 列举快照（摘要视图）。
    ///
    /// 刻意不取 `payload` 列：本方法被保留策略 `prune` 以 `None` 全库调用，
    /// 历史上会把所有历史快照的整份 JSONB 一次性读进内存（每份都是全命名空间副本）。
    /// 需要负载的调用方必须走 `get_backup(id)`。
    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>> {
        const META_COLUMNS: &str =
            "id, namespace, created_by, note, entry_count, checksum, retain_until, created_at";
        let rows = match namespace {
            Some(ns) => sqlx::query(&format!(
                "select {} from memory_backups where namespace = $1 order by created_at desc",
                META_COLUMNS
            ))
            .bind(ns)
            .fetch_all(&self.pool)
            .await
            .pg()?,
            None => sqlx::query(&format!(
                "select {} from memory_backups order by created_at desc",
                META_COLUMNS
            ))
            .fetch_all(&self.pool)
            .await
            .pg()?,
        };
        rows.iter().map(Self::backup_meta_from_row).collect()
    }

    async fn delete_backup(&self, id: Uuid) -> Result<()> {
        sqlx::query("delete from memory_backups where id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(())
    }
}
