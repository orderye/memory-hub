use super::*;

#[async_trait]
impl CodingStore for PostgresHubStore {
    async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
        if let Some(task_id) = thread.task_id {
            let task_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from memory_tasks where namespace = $1 and id = $2)",
            )
            .bind(&thread.namespace)
            .bind(task_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !task_exists {
                return Err(HubError::InvalidArgument(
                    "关联任务不存在于该命名空间".into(),
                ));
            }
        }

        let id = thread.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let row = sqlx::query(
            r#"
            insert into coding_threads (
                id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $8)
            returning id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(&thread.namespace)
        .bind(&thread.repository)
        .bind(&thread.branch)
        .bind(thread.task_id)
        .bind(&thread.title)
        .bind(&thread.description)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::thread_from_row(&row)
    }

    async fn get_thread(&self, namespace: &str, id: Uuid) -> Result<Option<CodingThread>> {
        let row = sqlx::query(
            r#"
            select id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            from coding_threads
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.as_ref().map(Self::thread_from_row).transpose()
    }

    async fn list_threads(
        &self,
        namespace: &str,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodingThread>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            from coding_threads
            where namespace = $1 and ($2::text is null or repository = $2)
            order by updated_at desc, id asc
            limit $3
            "#,
        )
        .bind(namespace)
        .bind(repository)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::thread_from_row).collect()
    }

    async fn save_revision(
        &self,
        revision: &NewCodingRevision,
        diff_hash: &str,
    ) -> Result<CodingRevision> {
        if let Some(tid) = revision.thread_id {
            let thread_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from coding_threads where namespace = $1 and id = $2)",
            )
            .bind(&revision.namespace)
            .bind(tid)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !thread_exists {
                return Err(HubError::InvalidArgument(
                    "关联线程不存在于该命名空间".into(),
                ));
            }
        }
        if let Some(task_id) = revision.source_task_id {
            let task_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from memory_tasks where namespace = $1 and id = $2)",
            )
            .bind(&revision.namespace)
            .bind(task_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !task_exists {
                return Err(HubError::InvalidArgument(
                    "关联任务不存在于该命名空间".into(),
                ));
            }
        }
        if let Some(ev_id) = revision.source_evidence_id {
            let ev_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from memory_evidence where namespace = $1 and id = $2)",
            )
            .bind(&revision.namespace)
            .bind(ev_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !ev_exists {
                return Err(HubError::InvalidArgument(
                    "关联证据不存在于该命名空间".into(),
                ));
            }
        }
        let id = revision.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let occurred_at = revision.occurred_at.unwrap_or(now);
        let sql = format!(
            "insert into coding_revisions ({columns}) values ({values}) returning {columns}",
            columns = Self::REVISION_COLUMNS,
            values = Self::REVISION_INSERT_VALUES
        );
        let row = sqlx::query(&sql)
            .bind(id)
            .bind(revision.thread_id)
            .bind(&revision.namespace)
            .bind(&revision.repository)
            .bind(&revision.branch)
            .bind(&revision.commit_sha)
            .bind(&revision.parent_sha)
            .bind(&revision.file_path)
            .bind(revision.change_kind.as_str())
            .bind(&revision.diff_content)
            .bind(diff_hash)
            .bind(&revision.symbol_fingerprint)
            .bind(revision.is_committed)
            .bind(&revision.author)
            .bind(occurred_at)
            .bind(now)
            .bind(revision.source_evidence_id)
            .bind(revision.source_task_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

        Self::revision_from_row(&row)
    }

    async fn get_revision(&self, namespace: &str, id: Uuid) -> Result<Option<CodingRevision>> {
        let sql = format!(
            "select {} from coding_revisions where namespace = $1 and id = $2",
            Self::REVISION_COLUMNS
        );
        let row = sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .pg()?;

        row.as_ref().map(Self::revision_from_row).transpose()
    }

    async fn get_revision_by_hash(
        &self,
        namespace: &str,
        repository: &str,
        diff_hash: &str,
    ) -> Result<Option<CodingRevision>> {
        let sql = format!(
            "select {} from coding_revisions \
             where namespace = $1 and repository = $2 and diff_hash = $3 limit 1",
            Self::REVISION_COLUMNS
        );
        let row = sqlx::query(&sql)
            .bind(namespace)
            .bind(repository)
            .bind(diff_hash)
            .fetch_optional(&self.pool)
            .await
            .pg()?;

        row.as_ref().map(Self::revision_from_row).transpose()
    }

    async fn list_revisions_for_thread(
        &self,
        namespace: &str,
        thread_id: Uuid,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let sql = format!(
            "select {} from coding_revisions where namespace = $1 and thread_id = $2 \
             order by occurred_at desc, id asc limit $3",
            Self::REVISION_COLUMNS
        );
        let rows = sqlx::query(&sql)
            .bind(namespace)
            .bind(thread_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .pg()?;

        rows.iter().map(Self::revision_from_row).collect()
    }

    async fn list_revisions_for_file(
        &self,
        namespace: &str,
        repository: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let sql = format!(
            "select {} from coding_revisions \
             where namespace = $1 and repository = $2 and file_path = $3 \
             order by occurred_at desc, id asc limit $4",
            Self::REVISION_COLUMNS
        );
        let rows = sqlx::query(&sql)
            .bind(namespace)
            .bind(repository)
            .bind(file_path)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .pg()?;

        rows.iter().map(Self::revision_from_row).collect()
    }

    async fn all_threads_in_namespace(&self, namespace: &str) -> Result<Vec<CodingThread>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            from coding_threads
            where namespace = $1
            order by created_at asc, id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::thread_from_row).collect()
    }

    async fn all_revisions_in_namespace(&self, namespace: &str) -> Result<Vec<CodingRevision>> {
        let sql = format!(
            "select {} from coding_revisions where namespace = $1 \
             order by occurred_at asc, id asc",
            Self::REVISION_COLUMNS
        );
        let rows = sqlx::query(&sql)
            .bind(namespace)
            .fetch_all(&self.pool)
            .await
            .pg()?;

        rows.iter().map(Self::revision_from_row).collect()
    }

    async fn delete_thread(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from coding_threads where namespace = $1 and id = $2")
            .bind(namespace)
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

    async fn delete_revision(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from coding_revisions where namespace = $1 and id = $2")
            .bind(namespace)
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
}
