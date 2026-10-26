use super::*;

#[async_trait]
impl EvidenceStore for PostgresHubStore {
    async fn append_evidence(&self, evidence: &NewEvidence) -> Result<MemoryEvidence> {
        let id = evidence.id.unwrap_or_else(Uuid::new_v4);
        let row = sqlx::query(
            r#"
            insert into memory_evidence (id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at)
            values ($1, $2, $3, $4, $5, $6, $7, now(), $8)
            returning id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at
            "#,
        )
        .bind(id)
        .bind(&evidence.namespace)
        .bind(evidence.source_kind.as_str())
        .bind(&evidence.source_ref)
        .bind(&evidence.content_hash)
        .bind(&evidence.excerpt)
        .bind(evidence.occurred_at)
        .bind(evidence.expires_at)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::evidence_from_row(&row)
    }

    async fn get_evidence(&self, namespace: &str, id: Uuid) -> Result<MemoryEvidence> {
        let row = sqlx::query(
            r#"
            select id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at
            from memory_evidence
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => Self::evidence_from_row(&r),
            None => Err(HubError::NotFound),
        }
    }

    async fn list_evidence(
        &self,
        namespace: &str,
        source_kind: Option<EvidenceSourceKind>,
        source_ref: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEvidence>> {
        let mut builder: sqlx::QueryBuilder<'_, sqlx::Postgres> = sqlx::QueryBuilder::new(
            "select id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at from memory_evidence where namespace = ",
        );
        builder.push_bind(namespace);

        if let Some(sk) = source_kind {
            builder.push(" and source_kind = ");
            builder.push_bind(sk.as_str());
        }
        if let Some(sref) = source_ref {
            builder.push(" and source_ref = ");
            builder.push_bind(sref);
        }

        builder.push(" order by created_at desc limit ");
        builder.push_bind(limit as i64);

        let rows = builder.build().fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::evidence_from_row).collect()
    }

    async fn all_evidence_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryEvidence>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at
            from memory_evidence
            where namespace = $1
            order by created_at desc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::evidence_from_row).collect()
    }

    async fn delete_evidence(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from memory_evidence where namespace = $1 and id = $2")
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

    /// 清理已过保留期限的证据（NULL = 永久保留，不动）
    async fn purge_expired_evidence(&self, namespace: &str, now: DateTime<Utc>) -> Result<usize> {
        let res = sqlx::query(
            "delete from memory_evidence where namespace = $1 and expires_at is not null and expires_at <= $2",
        )
        .bind(namespace)
        .bind(now)
        .execute(&self.pool)
        .await
        .pg()?;

        Ok(res.rows_affected() as usize)
    }
}

#[async_trait]
impl RelationStore for PostgresHubStore {
    async fn add_relation(
        &self,
        namespace: &str,
        relation: &NewMemoryRelation,
    ) -> Result<MemoryRelation> {
        if relation.from_memory_id == relation.to_memory_id {
            return Err(HubError::InvalidArgument(
                "实体自环关系不允许 (from == to)".into(),
            ));
        }

        let count: i64 = sqlx::query_scalar(
            "select count(*) from memories where namespace = $1 and id in ($2, $3)",
        )
        .bind(namespace)
        .bind(relation.from_memory_id)
        .bind(relation.to_memory_id)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        if count != 2 {
            return Err(HubError::NotFound);
        }

        if let Some(ev_id) = relation.source_evidence_id {
            let ev_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from memory_evidence where namespace = $1 and id = $2)",
            )
            .bind(namespace)
            .bind(ev_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !ev_exists {
                return Err(HubError::NotFound);
            }
        }

        let row = sqlx::query(
            r#"
            insert into memory_relations (namespace, from_memory_id, to_memory_id, relation_kind, confidence, source_evidence_id, created_at)
            values ($1, $2, $3, $4, $5, $6, now())
            on conflict (from_memory_id, to_memory_id, relation_kind) do update set
                namespace = excluded.namespace,
                confidence = excluded.confidence,
                source_evidence_id = coalesce(excluded.source_evidence_id, memory_relations.source_evidence_id)
            returning namespace, from_memory_id, to_memory_id, relation_kind, confidence, source_evidence_id, created_at
            "#,
        )
        .bind(namespace)
        .bind(relation.from_memory_id)
        .bind(relation.to_memory_id)
        .bind(relation.relation_kind.as_str())
        .bind(relation.confidence)
        .bind(relation.source_evidence_id)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::relation_from_row(&row)
    }

    async fn list_relations_from(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        let rows = sqlx::query(
            r#"
            select r.namespace, r.from_memory_id, r.to_memory_id, r.relation_kind, r.confidence, r.source_evidence_id, r.created_at
            from memory_relations r
            join memories m on r.from_memory_id = m.id
            where m.namespace = $1 and r.from_memory_id = $2
            order by r.created_at desc
            "#,
        )
        .bind(namespace)
        .bind(from_memory_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::relation_from_row).collect()
    }

    async fn list_relations_to(
        &self,
        namespace: &str,
        to_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        let rows = sqlx::query(
            r#"
            select r.namespace, r.from_memory_id, r.to_memory_id, r.relation_kind, r.confidence, r.source_evidence_id, r.created_at
            from memory_relations r
            join memories m on r.to_memory_id = m.id
            where m.namespace = $1 and r.to_memory_id = $2
            order by r.created_at desc
            "#,
        )
        .bind(namespace)
        .bind(to_memory_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::relation_from_row).collect()
    }

    async fn delete_relation(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
        to_memory_id: Uuid,
        relation_kind: MemoryRelationKind,
    ) -> Result<()> {
        let res = sqlx::query(
            r#"
            delete from memory_relations
            where namespace = $1
              and from_memory_id = $2
              and to_memory_id = $3
              and relation_kind = $4
            "#,
        )
        .bind(namespace)
        .bind(from_memory_id)
        .bind(to_memory_id)
        .bind(relation_kind.as_str())
        .execute(&self.pool)
        .await
        .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn all_relations_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryRelation>> {
        let rows = sqlx::query(
            r#"
            select namespace, from_memory_id, to_memory_id, relation_kind, confidence, source_evidence_id, created_at
            from memory_relations
            where namespace = $1
            order by created_at desc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::relation_from_row).collect()
    }
}

// ===== TaskStore =====

#[async_trait]
impl TaskStore for PostgresHubStore {
    async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask> {
        let id = task.id.unwrap_or_else(Uuid::new_v4);
        let row = sqlx::query(
            r#"
            insert into memory_tasks (
                id, namespace, scene, task, status, started_at, finished_at,
                result_summary, failure_reason, source_evidence_id, metadata
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            returning id, namespace, scene, task, status, started_at, finished_at,
                      result_summary, failure_reason, source_evidence_id, metadata
            "#,
        )
        .bind(id)
        .bind(&task.namespace)
        .bind(task.scene.as_str())
        .bind(&task.task)
        .bind(task.status.as_str())
        .bind(task.started_at)
        .bind(task.finished_at)
        .bind(&task.result_summary)
        .bind(&task.failure_reason)
        .bind(task.source_evidence_id)
        .bind(&task.metadata)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::task_from_row(&row)
    }

    async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
        let row = sqlx::query(
            r#"
            update memory_tasks
            set scene = $3,
                task = $4,
                status = $5,
                started_at = $6,
                finished_at = $7,
                result_summary = $8,
                failure_reason = $9,
                source_evidence_id = $10,
                metadata = $11
            where namespace = $1 and id = $2
            returning id, namespace, scene, task, status, started_at, finished_at,
                      result_summary, failure_reason, source_evidence_id, metadata
            "#,
        )
        .bind(&task.namespace)
        .bind(task.id)
        .bind(task.scene.as_str())
        .bind(&task.task)
        .bind(task.status.as_str())
        .bind(task.started_at)
        .bind(task.finished_at)
        .bind(&task.result_summary)
        .bind(&task.failure_reason)
        .bind(task.source_evidence_id)
        .bind(&task.metadata)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => Self::task_from_row(&r),
            None => Err(HubError::NotFound),
        }
    }

    async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
        let row = sqlx::query(
            r#"
            select id, namespace, scene, task, status, started_at, finished_at,
                   result_summary, failure_reason, source_evidence_id, metadata
            from memory_tasks
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.map(|r| Self::task_from_row(&r)).transpose()
    }

    async fn list_tasks(
        &self,
        namespace: &str,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> Result<Vec<MemoryTask>> {
        let mut builder: sqlx::QueryBuilder<'_, sqlx::Postgres> = sqlx::QueryBuilder::new(
            "select id, namespace, scene, task, status, started_at, finished_at, result_summary, failure_reason, source_evidence_id, metadata from memory_tasks where namespace = ",
        );
        builder.push_bind(namespace);

        if let Some(s) = status {
            builder.push(" and status = ");
            builder.push_bind(s.as_str());
        }

        builder.push(" order by started_at desc limit ");
        builder.push_bind(limit.max(1) as i64);

        let rows = builder.build().fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::task_from_row).collect()
    }

    async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, scene, task, status, started_at, finished_at,
                   result_summary, failure_reason, source_evidence_id, metadata
            from memory_tasks
            where namespace = $1
            order by started_at asc, id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::task_from_row).collect()
    }

    async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from memory_tasks where namespace = $1 and id = $2")
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

// ===== ExperienceStore =====

#[async_trait]
impl ExperienceStore for PostgresHubStore {
    async fn save_experience(&self, exp: &NewMemoryExperience) -> Result<MemoryExperience> {
        let id = exp.id.unwrap_or_else(Uuid::new_v4);
        let row = sqlx::query(
            r#"
            insert into memory_experiences (
                id, namespace, task, search_keys, solution, preconditions,
                boundary_envs, failure_modes, validation, confidence, status,
                feedback_score, created_at, updated_at
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now(), now())
            returning id, namespace, task, search_keys, solution, preconditions,
                      boundary_envs, failure_modes, validation, confidence, status,
                      feedback_score, created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(&exp.namespace)
        .bind(&exp.task)
        .bind(&exp.search_keys)
        .bind(&exp.solution)
        .bind(&exp.preconditions)
        .bind(&exp.boundary_envs)
        .bind(&exp.failure_modes)
        .bind(&exp.validation)
        .bind(exp.confidence)
        .bind(exp.status.as_str())
        .bind(exp.feedback_score)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::experience_from_row(&row)
    }

    async fn update_experience(&self, exp: &MemoryExperience) -> Result<MemoryExperience> {
        let row = sqlx::query(
            r#"
            update memory_experiences
            set task = $3,
                search_keys = $4,
                solution = $5,
                preconditions = $6,
                boundary_envs = $7,
                failure_modes = $8,
                validation = $9,
                confidence = $10,
                status = $11,
                feedback_score = $12,
                updated_at = now()
            where namespace = $1 and id = $2
            returning id, namespace, task, search_keys, solution, preconditions,
                      boundary_envs, failure_modes, validation, confidence, status,
                      feedback_score, created_at, updated_at
            "#,
        )
        .bind(&exp.namespace)
        .bind(exp.id)
        .bind(&exp.task)
        .bind(&exp.search_keys)
        .bind(&exp.solution)
        .bind(&exp.preconditions)
        .bind(&exp.boundary_envs)
        .bind(&exp.failure_modes)
        .bind(&exp.validation)
        .bind(exp.confidence)
        .bind(exp.status.as_str())
        .bind(exp.feedback_score)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => Self::experience_from_row(&r),
            None => Err(HubError::NotFound),
        }
    }

    async fn get_experience(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryExperience>> {
        let row = sqlx::query(
            r#"
            select id, namespace, task, search_keys, solution, preconditions,
                   boundary_envs, failure_modes, validation, confidence, status,
                   feedback_score, created_at, updated_at
            from memory_experiences
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.map(|r| Self::experience_from_row(&r)).transpose()
    }

    async fn list_experiences(
        &self,
        namespace: &str,
        status: Option<ExperienceStatus>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryExperience>> {
        let mut builder: sqlx::QueryBuilder<'_, sqlx::Postgres> = sqlx::QueryBuilder::new(
            "select id, namespace, task, search_keys, solution, preconditions, boundary_envs, failure_modes, validation, confidence, status, feedback_score, created_at, updated_at from memory_experiences where namespace = ",
        );
        builder.push_bind(namespace);

        if let Some(s) = status {
            builder.push(" and status = ");
            builder.push_bind(s.as_str());
        }

        builder.push(" order by updated_at desc, id asc limit ");
        builder.push_bind(limit.max(1) as i64);
        builder.push(" offset ");
        builder.push_bind(offset as i64);

        let rows = builder.build().fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::experience_from_row).collect()
    }

    async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
        let (task_ns, exp_ns): (Option<String>, Option<String>) = sqlx::query_as(
            r#"
            select 
                (select namespace from memory_tasks where id = $1) as task_ns,
                (select namespace from memory_experiences where id = $2) as exp_ns
            "#,
        )
        .bind(link.task_id)
        .bind(link.experience_id)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        let (t_ns, e_ns) = match (task_ns, exp_ns) {
            (Some(t), Some(e)) => (t, e),
            _ => {
                return Err(HubError::NotFound);
            }
        };

        if t_ns != e_ns {
            return Err(HubError::InvalidArgument(
                "任务与经验不属于同一命名空间".into(),
            ));
        }

        if let Some(ref link_ns) = link.namespace {
            if *link_ns != t_ns {
                return Err(HubError::InvalidArgument(
                    "指定命名空间与任务所属命名空间不一致".into(),
                ));
            }
        }

        sqlx::query(
            r#"
            insert into memory_task_experiences (namespace, task_id, experience_id, relation_kind, created_at)
            values ($1, $2, $3, $4, $5)
            on conflict (task_id, experience_id, relation_kind) do nothing
            "#,
        )
        .bind(&t_ns)
        .bind(link.task_id)
        .bind(link.experience_id)
        .bind(&link.relation_kind)
        .bind(link.created_at)
        .execute(&self.pool)
        .await
        .pg()?;

        Ok(())
    }

    async fn list_experiences_for_task(&self, task_id: Uuid) -> Result<Vec<MemoryTaskExperience>> {
        let rows = sqlx::query(
            r#"
            select namespace, task_id, experience_id, relation_kind, created_at
            from memory_task_experiences
            where task_id = $1
            order by created_at desc
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::task_experience_from_row).collect()
    }

    async fn all_experiences_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryExperience>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, task, search_keys, solution, preconditions,
                    boundary_envs, failure_modes, validation, confidence, status,
                    feedback_score, created_at, updated_at
            from memory_experiences
            where namespace = $1
            order by created_at asc, id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::experience_from_row).collect()
    }

    async fn all_task_experiences_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<MemoryTaskExperience>> {
        let rows = sqlx::query(
            r#"
            select namespace, task_id, experience_id, relation_kind, created_at
            from memory_task_experiences
            where namespace = $1
            order by created_at asc, task_id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::task_experience_from_row).collect()
    }

    async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from memory_experiences where namespace = $1 and id = $2")
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
