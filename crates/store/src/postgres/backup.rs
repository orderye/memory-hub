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

    async fn replace_namespaces_from_snapshots(
        &self,
        snapshots: &[(String, mem_core::RestoreSnapshot)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.pg()?;
        for (namespace, snapshot) in snapshots {
            for entity_ns in snapshot
                .entries
                .iter()
                .map(|e| &e.namespace)
                .chain(snapshot.evidences.iter().map(|e| &e.namespace))
                .chain(snapshot.tasks.iter().map(|e| &e.namespace))
                .chain(snapshot.experiences.iter().map(|e| &e.namespace))
                .chain(snapshot.experience_versions.iter().map(|e| &e.namespace))
                .chain(snapshot.coding_threads.iter().map(|e| &e.namespace))
                .chain(snapshot.coding_revisions.iter().map(|e| &e.namespace))
            {
                if entity_ns != namespace {
                    return Err(HubError::InvalidArgument(
                        "restore snapshot namespace mismatch".into(),
                    ));
                }
            }

            for table in [
                "coding_revisions",
                "coding_threads",
                "memory_task_experiences",
                "memory_experience_versions",
                "memory_experiences",
                "memory_tasks",
                "memory_relations",
                "memory_evidence",
                "memory_versions",
                "memories",
            ] {
                let sql = format!("delete from {table} where namespace = $1");
                sqlx::query(&sql)
                    .bind(namespace)
                    .execute(&mut *tx)
                    .await
                    .pg()?;
            }

            for evidence in &snapshot.evidences {
                sqlx::query(
                    "insert into memory_evidence (id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at) values ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
                )
                .bind(evidence.id).bind(namespace).bind(evidence.source_kind.as_str())
                .bind(&evidence.source_ref).bind(&evidence.content_hash).bind(&evidence.excerpt)
                .bind(evidence.occurred_at).bind(evidence.created_at).bind(evidence.expires_at)
                .execute(&mut *tx).await.pg()?;
            }
            for entry in &snapshot.entries {
                Self::insert_memory_on(&mut tx, entry).await?;
            }
            for version in &snapshot.memory_versions {
                Self::insert_version_on(&mut tx, version).await?;
            }
            for task in &snapshot.tasks {
                sqlx::query(
                    "insert into memory_tasks (id,namespace,scene,task,status,started_at,finished_at,result_summary,failure_reason,source_evidence_id,metadata) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
                )
                .bind(task.id).bind(namespace).bind(task.scene.as_str()).bind(&task.task)
                .bind(task.status.as_str()).bind(task.started_at).bind(task.finished_at)
                .bind(&task.result_summary).bind(&task.failure_reason).bind(task.source_evidence_id)
                .bind(&task.metadata).execute(&mut *tx).await.pg()?;
            }
            for experience in &snapshot.experiences {
                sqlx::query(
                    "insert into memory_experiences (id,namespace,task,search_keys,solution,preconditions,boundary_envs,failure_modes,validation,confidence,status,feedback_score,version,superseded_by,verified_at,verified_by,verification_note,status_reason,source_count,created_at,updated_at) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)",
                )
                .bind(experience.id).bind(namespace).bind(&experience.task).bind(&experience.search_keys)
                .bind(&experience.solution).bind(&experience.preconditions).bind(&experience.boundary_envs)
                .bind(&experience.failure_modes).bind(&experience.validation).bind(experience.confidence)
                .bind(experience.status.as_str()).bind(experience.feedback_score).bind(experience.version)
                .bind(experience.superseded_by).bind(experience.verified_at).bind(&experience.verified_by)
                .bind(&experience.verification_note).bind(&experience.status_reason).bind(experience.source_count)
                .bind(experience.created_at).bind(experience.updated_at).execute(&mut *tx).await.pg()?;
            }
            for version in &snapshot.experience_versions {
                sqlx::query(
                    "insert into memory_experience_versions (id,experience_id,namespace,version,snapshot,change_kind,changed_by,status,note,changed_at) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
                )
                .bind(version.id).bind(version.experience_id).bind(namespace).bind(version.version)
                .bind(&version.snapshot).bind(version.change_kind.as_str()).bind(&version.changed_by)
                .bind(version.status.as_str()).bind(&version.note).bind(version.changed_at)
                .execute(&mut *tx).await.pg()?;
            }
            for link in &snapshot.task_experiences {
                sqlx::query(
                    "insert into memory_task_experiences (namespace,task_id,experience_id,relation_kind,created_at) values ($1,$2,$3,$4,$5)",
                )
                .bind(namespace).bind(link.task_id).bind(link.experience_id).bind(&link.relation_kind)
                .bind(link.created_at).execute(&mut *tx).await.pg()?;
            }
            for relation in &snapshot.relations {
                sqlx::query(
                    "insert into memory_relations (namespace,from_memory_id,to_memory_id,relation_kind,confidence,source_evidence_id,created_at) values ($1,$2,$3,$4,$5,$6,$7)",
                )
                .bind(namespace).bind(relation.from_memory_id).bind(relation.to_memory_id)
                .bind(relation.relation_kind.as_str()).bind(relation.confidence)
                .bind(relation.source_evidence_id).bind(relation.created_at)
                .execute(&mut *tx).await.pg()?;
            }
            for thread in &snapshot.coding_threads {
                sqlx::query(
                    "insert into coding_threads (id,namespace,repository,branch,task_id,title,description,created_at,updated_at) values ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
                )
                .bind(thread.id).bind(namespace).bind(&thread.repository).bind(&thread.branch)
                .bind(thread.task_id).bind(&thread.title).bind(&thread.description)
                .bind(thread.created_at).bind(thread.updated_at).execute(&mut *tx).await.pg()?;
            }
            for revision in &snapshot.coding_revisions {
                let sql = format!(
                    "insert into coding_revisions ({}) values ({})",
                    Self::REVISION_COLUMNS,
                    Self::REVISION_INSERT_VALUES
                );
                sqlx::query(&sql)
                    .bind(revision.id)
                    .bind(revision.thread_id)
                    .bind(namespace)
                    .bind(&revision.repository)
                    .bind(&revision.branch)
                    .bind(&revision.commit_sha)
                    .bind(&revision.parent_sha)
                    .bind(&revision.file_path)
                    .bind(revision.change_kind.as_str())
                    .bind(&revision.diff_content)
                    .bind(&revision.diff_hash)
                    .bind(&revision.symbol_fingerprint)
                    .bind(revision.is_committed)
                    .bind(&revision.author)
                    .bind(revision.occurred_at)
                    .bind(revision.created_at)
                    .bind(revision.source_evidence_id)
                    .bind(revision.source_task_id)
                    .execute(&mut *tx)
                    .await
                    .pg()?;
            }
        }
        tx.commit().await.pg()?;
        Ok(())
    }
}
